//! Minimal Tempo JSON-RPC client for the submission-fee voucher flow.
//!
//! `taskfast post` needs to:
//!   1. turn the server-emitted ERC-20 transfer calldata into a signed,
//!      broadcast tx whose hash can be submitted as a voucher, or
//!   2. hand the server a raw RLP-encoded signed tx it can broadcast.
//!
//! Both paths need `eth_chainId` + `eth_getTransactionCount` + `eth_gasPrice`
//! to build a replay-safe legacy tx. Path (1) additionally needs
//! `eth_sendRawTransaction`. That's it. `alloy-provider` would cover this
//! plus subscription, middleware, and batching we don't want — so this
//! module wraps those four methods over `reqwest` directly.
//!
//! # Why legacy (not EIP-1559) txs
//!
//! Tempo is a stable-fee chain fork — `eth_gasPrice` is authoritative and
//! `eth_feeHistory` isn't guaranteed to exist on every node. A legacy tx
//! with `chain_id` set (EIP-155 replay protection) is the widest-compat
//! shape and keeps the RPC surface to 3 read methods + 1 write.

use alloy_consensus::{SignableTransaction, TxEnvelope, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxHash, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error as ThisError;

/// Fixed gas limit for ERC-20 transfers. An ERC-20 `transfer` costs ~50k on
/// most tokens; 100k is a safe ceiling that avoids an extra `eth_estimateGas`
/// round-trip without risking an OOG revert.
pub const ERC20_TRANSFER_GAS_LIMIT: u64 = 100_000;

/// Errors surfaced by [`TempoRpcClient`] and [`sign_and_broadcast_erc20_transfer`].
#[derive(Debug, ThisError)]
pub enum RpcError {
    #[error("rpc transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("rpc returned non-2xx {status}: {body}")]
    Http { status: u16, body: String },
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("rpc response shape invalid: {0}")]
    Decode(String),
    #[error("hex decode: {0}")]
    Hex(String),
    #[error("signing failed: {0}")]
    Sign(String),
}

/// Thin JSON-RPC client over reqwest. One instance = one RPC endpoint.
#[derive(Debug, Clone)]
pub struct TempoRpcClient {
    http: reqwest::Client,
    url: String,
}

#[derive(Serialize)]
struct RpcRequest<'a, P: Serialize> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: P,
}

#[derive(Deserialize)]
struct RpcResponse {
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcErrorBody>,
}

#[derive(Deserialize)]
struct RpcErrorBody {
    code: i64,
    #[serde(default)]
    message: String,
}

impl TempoRpcClient {
    /// Build from an existing reqwest client (share connection pool with the
    /// TaskFast API client) plus the Tempo RPC URL.
    pub fn new(http: reqwest::Client, url: impl Into<String>) -> Self {
        Self { http, url: url.into() }
    }

    /// Convenience: build with a fresh reqwest client. Tests use this.
    pub fn with_default_client(url: impl Into<String>) -> Self {
        Self::new(reqwest::Client::new(), url)
    }

    /// `eth_chainId` → u64. Hex-prefixed-hex decode.
    pub async fn chain_id(&self) -> Result<u64, RpcError> {
        let raw: String = self.call("eth_chainId", json!([])).await?;
        parse_hex_u64(&raw)
    }

    /// `eth_getTransactionCount(address, "pending")` → nonce.
    ///
    /// Uses `pending` not `latest` so a second tx from the same signer doesn't
    /// collide on nonce if the first hasn't been mined yet.
    pub async fn pending_nonce(&self, addr: Address) -> Result<u64, RpcError> {
        let raw: String = self
            .call(
                "eth_getTransactionCount",
                json!([format!("{addr:#x}"), "pending"]),
            )
            .await?;
        parse_hex_u64(&raw)
    }

    /// `eth_gasPrice` → u128. Wei.
    pub async fn gas_price(&self) -> Result<u128, RpcError> {
        let raw: String = self.call("eth_gasPrice", json!([])).await?;
        parse_hex_u128(&raw)
    }

    /// `eth_sendRawTransaction(0x<rlp>)` → tx hash.
    pub async fn send_raw_transaction(&self, raw_tx: &[u8]) -> Result<TxHash, RpcError> {
        let hex_str = format!("0x{}", hex::encode(raw_tx));
        let raw: String = self
            .call("eth_sendRawTransaction", json!([hex_str]))
            .await?;
        let bytes = decode_0x(&raw)?;
        if bytes.len() != 32 {
            return Err(RpcError::Decode(format!(
                "tx hash must be 32 bytes, got {}",
                bytes.len()
            )));
        }
        Ok(TxHash::from_slice(&bytes))
    }

    async fn call<R>(&self, method: &str, params: Value) -> Result<R, RpcError>
    where
        R: for<'de> Deserialize<'de>,
    {
        let req = RpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method,
            params,
        };
        let resp = self.http.post(&self.url).json(&req).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RpcError::Http {
                status: status.as_u16(),
                body,
            });
        }
        let parsed: RpcResponse = resp.json().await?;
        if let Some(err) = parsed.error {
            return Err(RpcError::Rpc {
                code: err.code,
                message: err.message,
            });
        }
        let result = parsed
            .result
            .ok_or_else(|| RpcError::Decode("rpc response missing both result and error".into()))?;
        serde_json::from_value(result).map_err(|e| RpcError::Decode(e.to_string()))
    }
}

/// Sign `calldata` as an ERC-20 `transfer` tx to `token_address`, broadcast
/// it, and return the resulting tx hash. The hash is the `submission_fee_voucher`
/// value for `POST /api/task_drafts/{id}/submit` (server polls for confirmation).
///
/// Fetches chain_id + nonce + gas_price from the RPC; gas limit is fixed at
/// [`ERC20_TRANSFER_GAS_LIMIT`] to avoid an extra `eth_estimateGas` hop.
pub async fn sign_and_broadcast_erc20_transfer(
    rpc: &TempoRpcClient,
    signer: &PrivateKeySigner,
    token_address: Address,
    calldata: Bytes,
) -> Result<TxHash, RpcError> {
    let raw = sign_erc20_transfer(rpc, signer, token_address, calldata).await?;
    rpc.send_raw_transaction(&raw).await
}

/// Build + sign an ERC-20 transfer tx without broadcasting. The returned bytes
/// are the raw RLP-encoded signed tx — suitable to hand the server verbatim
/// as the voucher (the "raw-signed-tx" voucher form).
pub async fn sign_erc20_transfer(
    rpc: &TempoRpcClient,
    signer: &PrivateKeySigner,
    token_address: Address,
    calldata: Bytes,
) -> Result<Vec<u8>, RpcError> {
    let chain_id = rpc.chain_id().await?;
    let nonce = rpc.pending_nonce(signer.address()).await?;
    let gas_price = rpc.gas_price().await?;

    let tx = TxLegacy {
        chain_id: Some(chain_id),
        nonce,
        gas_price,
        gas_limit: ERC20_TRANSFER_GAS_LIMIT,
        to: TxKind::Call(token_address),
        value: U256::ZERO,
        input: calldata,
    };
    let sig_hash = tx.signature_hash();
    let sig = signer
        .sign_hash_sync(&sig_hash)
        .map_err(|e| RpcError::Sign(e.to_string()))?;
    let signed = tx.into_signed(sig);
    let envelope = TxEnvelope::Legacy(signed);
    Ok(envelope.encoded_2718())
}

fn parse_hex_u64(s: &str) -> Result<u64, RpcError> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(stripped, 16)
        .map_err(|e| RpcError::Hex(format!("u64 from {s}: {e}")))
}

fn parse_hex_u128(s: &str) -> Result<u128, RpcError> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    u128::from_str_radix(stripped, 16)
        .map_err(|e| RpcError::Hex(format!("u128 from {s}: {e}")))
}

fn decode_0x(s: &str) -> Result<Vec<u8>, RpcError> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    hex::decode(stripped).map_err(|e| RpcError::Hex(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn rpc_ok(result: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": result,
        }))
    }

    #[tokio::test]
    async fn chain_id_decodes_hex() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({"method": "eth_chainId"})))
            .respond_with(rpc_ok(json!("0xa5bf")))
            .mount(&server)
            .await;
        let rpc = TempoRpcClient::with_default_client(server.uri());
        assert_eq!(rpc.chain_id().await.unwrap(), 42_431);
    }

    #[tokio::test]
    async fn nonce_sends_pending_tag() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({
                "method": "eth_getTransactionCount",
                "params": ["0x0000000000000000000000000000000000000001", "pending"],
            })))
            .respond_with(rpc_ok(json!("0x7")))
            .mount(&server)
            .await;
        let rpc = TempoRpcClient::with_default_client(server.uri());
        let addr: Address = "0x0000000000000000000000000000000000000001".parse().unwrap();
        assert_eq!(rpc.pending_nonce(addr).await.unwrap(), 7);
    }

    #[tokio::test]
    async fn gas_price_decodes_u128() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({"method": "eth_gasPrice"})))
            .respond_with(rpc_ok(json!("0x3b9aca00"))) // 1 gwei
            .mount(&server)
            .await;
        let rpc = TempoRpcClient::with_default_client(server.uri());
        assert_eq!(rpc.gas_price().await.unwrap(), 1_000_000_000u128);
    }

    #[tokio::test]
    async fn send_raw_transaction_returns_tx_hash() {
        let server = MockServer::start().await;
        let hash_hex = format!("0x{}", "aa".repeat(32));
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({"method": "eth_sendRawTransaction"})))
            .respond_with(rpc_ok(json!(hash_hex.clone())))
            .mount(&server)
            .await;
        let rpc = TempoRpcClient::with_default_client(server.uri());
        let h = rpc.send_raw_transaction(&[1u8, 2, 3]).await.unwrap();
        assert_eq!(format!("{h:#x}"), hash_hex);
    }

    #[tokio::test]
    async fn rpc_error_body_surfaces_as_rpc_variant() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": { "code": -32000, "message": "nonce too low" },
            })))
            .mount(&server)
            .await;
        let rpc = TempoRpcClient::with_default_client(server.uri());
        let err = rpc.gas_price().await.expect_err("must fail");
        match err {
            RpcError::Rpc { code, message } => {
                assert_eq!(code, -32000);
                assert!(message.contains("nonce too low"));
            }
            other => panic!("expected Rpc, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_5xx_surfaces_as_http_variant() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(502).set_body_string("upstream"))
            .mount(&server)
            .await;
        let rpc = TempoRpcClient::with_default_client(server.uri());
        let err = rpc.chain_id().await.expect_err("must fail");
        assert!(matches!(err, RpcError::Http { status: 502, .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn sign_erc20_transfer_roundtrips_through_rpc() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_chainId"})))
            .respond_with(rpc_ok(json!("0xa5bf"))) // 42431 testnet
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_getTransactionCount"})))
            .respond_with(rpc_ok(json!("0x0")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_gasPrice"})))
            .respond_with(rpc_ok(json!("0x3b9aca00")))
            .mount(&server)
            .await;
        let signer = PrivateKeySigner::random();
        let rpc = TempoRpcClient::with_default_client(server.uri());
        let token: Address = "0x00000000000000000000000000000000000000aa".parse().unwrap();
        // Real-looking ERC-20 transfer calldata: selector 0xa9059cbb + 32b addr + 32b amount.
        let calldata = Bytes::from_iter([
            0xa9u8, 0x05, 0x9c, 0xbb,
        ].into_iter().chain(vec![0u8; 32 + 32]));
        let raw = sign_erc20_transfer(&rpc, &signer, token, calldata)
            .await
            .expect("sign");
        assert!(!raw.is_empty(), "raw tx must encode to non-empty bytes");
        // Legacy tx RLP starts with list prefix 0xc0..0xf7 or 0xf8..0xff.
        assert!(raw[0] >= 0xc0, "legacy tx must be an RLP list, got first byte {:#x}", raw[0]);
    }
}
