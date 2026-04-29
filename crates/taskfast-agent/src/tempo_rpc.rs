//! Minimal Tempo JSON-RPC client for the submission-fee voucher flow.
//!
//! `taskfast post` needs to:
//!   1. turn the server-emitted ERC-20 transfer calldata into a signed,
//!      broadcast tx whose hash can be submitted as a voucher, or
//!   2. hand the server a raw RLP-encoded signed tx it can broadcast.
//!
//! Both paths need `eth_chainId` + `eth_getTransactionCount` + `eth_gasPrice`
//! + `eth_estimateGas` to build a replay-safe legacy tx with a correctly
//! sized gas limit. Path (1) additionally needs `eth_sendRawTransaction`.
//! That's it. `alloy-provider` would cover this plus subscription,
//! middleware, and batching we don't want — so this module wraps those
//! five methods over `reqwest` directly.
//!
//! # Why legacy (not EIP-1559) txs
//!
//! Tempo is a stable-fee chain fork — `eth_gasPrice` is authoritative and
//! `eth_feeHistory` isn't guaranteed to exist on every node. A legacy tx
//! with `chain_id` set (EIP-155 replay protection) is the widest-compat
//! shape and keeps the RPC surface to 4 read methods + 1 write.

use alloy_consensus::{SignableTransaction, TxEnvelope, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxHash, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error as ThisError;

/// Buffer applied to `eth_estimateGas` output, as numerator/denominator.
/// 13/10 = +30%. State drift between estimate and execution (cold SLOAD,
/// new storage slot, balance flip) can push actual gas above the estimate;
/// 30% matches the conservative end of the ethers/alloy-ecosystem default.
const GAS_ESTIMATE_BUFFER_NUM: u64 = 13;
const GAS_ESTIMATE_BUFFER_DEN: u64 = 10;

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
    #[serde(default)]
    data: Option<serde_json::Value>,
}

impl TempoRpcClient {
    /// Build from an existing reqwest client (share connection pool with the
    /// TaskFast API client) plus the Tempo RPC URL.
    pub fn new(http: reqwest::Client, url: impl Into<String>) -> Self {
        Self {
            http,
            url: url.into(),
        }
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

    /// `eth_estimateGas({from, to, data})` → gas units.
    ///
    /// Called before every ERC-20 transfer so we size `gas_limit` to actual
    /// chain state (Tempo testnet's canonical USDC `transfer` lands around
    /// 307k; a different token or recipient-with-no-prior-balance can push
    /// higher). A revert here means the tx would fail on-chain — callers
    /// should surface the error rather than broadcast a doomed tx.
    pub async fn estimate_gas(
        &self,
        from: Address,
        to: Address,
        data: &Bytes,
    ) -> Result<u64, RpcError> {
        let raw: String = self
            .call(
                "eth_estimateGas",
                json!([{
                    "from": format!("{from:#x}"),
                    "to": format!("{to:#x}"),
                    "data": format!("0x{}", hex::encode(data)),
                }]),
            )
            .await?;
        parse_hex_u64(&raw)
    }

    /// `eth_call({to, data}, "latest")` → returned bytes.
    ///
    /// Used for view-only calls (ERC-20 `allowance` / `balanceOf`, etc.) where
    /// we need to read chain state without broadcasting a tx.
    pub async fn eth_call(&self, to: Address, data: &Bytes) -> Result<Vec<u8>, RpcError> {
        let raw: String = self
            .call(
                "eth_call",
                json!([
                    {
                        "to": format!("{to:#x}"),
                        "data": format!("0x{}", hex::encode(data)),
                    },
                    "latest",
                ]),
            )
            .await?;
        decode_0x(&raw)
    }

    /// `eth_getTransactionReceipt(tx_hash)` → `Some(status_ok)` once mined,
    /// `None` if still pending.
    ///
    /// Returns `Ok(Some(true))` when the receipt's `status` field is `0x1`,
    /// `Ok(Some(false))` on `0x0` (reverted), `Ok(None)` if the receipt is
    /// not yet available.
    pub async fn transaction_receipt_status(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<bool>, RpcError> {
        let hash_hex = format!("{tx_hash:#x}");
        let value: Value = match self
            .call("eth_getTransactionReceipt", json!([hash_hex]))
            .await
        {
            Ok(v) => v,
            // Moderato public RPC occasionally returns {"jsonrpc":"2.0","id":1}
            // with neither `result` nor `error`. Treat identically to
            // `result: null` (pending) so `wait_for_receipt` keeps polling.
            Err(RpcError::Decode(ref m)) if m.contains("missing both result and error") => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        if value.is_null() {
            return Ok(None);
        }
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::Decode("receipt missing status field".into()))?;
        let ok = parse_hex_u64(status)? == 1;
        Ok(Some(ok))
    }

    /// Poll `eth_getTransactionReceipt` until mined or `timeout` elapses.
    ///
    /// Returns `Ok(true)` on `status == 0x1`, `Ok(false)` on revert, and
    /// `Err(RpcError::Decode)` on timeout. Caller maps the timeout to the
    /// command-level error variant (poster path wants `CmdError::Server`).
    pub async fn wait_for_receipt(
        &self,
        tx_hash: TxHash,
        timeout: std::time::Duration,
        interval: std::time::Duration,
    ) -> Result<bool, RpcError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(ok) = self.transaction_receipt_status(tx_hash).await? {
                return Ok(ok);
            }
            if std::time::Instant::now() >= deadline {
                return Err(RpcError::Decode(format!(
                    "receipt for {tx_hash:#x} not mined within {:?}",
                    timeout
                )));
            }
            tokio::time::sleep(interval).await;
        }
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
            let message = match err.data {
                Some(d) => format!("{} (data: {})", err.message, d),
                None => err.message,
            };
            return Err(RpcError::Rpc {
                code: err.code,
                message,
            });
        }
        let result = parsed
            .result
            .ok_or_else(|| RpcError::Decode("rpc response missing both result and error".into()))?;
        serde_json::from_value(result).map_err(|e| RpcError::Decode(e.to_string()))
    }
}

/// Sign `calldata` as a legacy tx targeting `to`, broadcast it, return tx hash.
///
/// Generalization of `sign_and_broadcast_erc20_transfer`: used by the poster's
/// `taskfast escrow sign` flow for both `IERC20.approve` (target = token) and
/// `TaskEscrow.open(...)` (target = escrow contract).
///
/// Fetches chain_id + nonce + gas_price + gas_limit (`eth_estimateGas` with a
/// 30% buffer) from the RPC.
pub async fn sign_and_broadcast_tx(
    rpc: &TempoRpcClient,
    signer: &PrivateKeySigner,
    to: Address,
    calldata: Bytes,
) -> Result<TxHash, RpcError> {
    let raw = sign_tx(rpc, signer, to, calldata).await?;
    rpc.send_raw_transaction(&raw).await
}

/// Build + sign a legacy tx targeting `to` without broadcasting. Returns the
/// raw RLP-encoded signed tx — suitable for dry-runs or server-broadcast flows.
pub async fn sign_tx(
    rpc: &TempoRpcClient,
    signer: &PrivateKeySigner,
    to: Address,
    calldata: Bytes,
) -> Result<Vec<u8>, RpcError> {
    let chain_id = rpc.chain_id().await?;
    let nonce = rpc.pending_nonce(signer.address()).await?;
    let gas_price = rpc.gas_price().await?;
    let estimate = rpc.estimate_gas(signer.address(), to, &calldata).await?;
    let gas_limit = estimate.saturating_mul(GAS_ESTIMATE_BUFFER_NUM) / GAS_ESTIMATE_BUFFER_DEN;

    let tx = TxLegacy {
        chain_id: Some(chain_id),
        nonce,
        gas_price,
        gas_limit,
        to: TxKind::Call(to),
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

/// Thin ERC-20 wrapper around [`sign_and_broadcast_tx`]. The returned hash is
/// the `submission_fee_voucher` for `POST /task_drafts/{id}/submit`.
pub async fn sign_and_broadcast_erc20_transfer(
    rpc: &TempoRpcClient,
    signer: &PrivateKeySigner,
    token_address: Address,
    calldata: Bytes,
) -> Result<TxHash, RpcError> {
    sign_and_broadcast_tx(rpc, signer, token_address, calldata).await
}

/// Thin ERC-20 wrapper around [`sign_tx`]. Returns raw RLP bytes for the
/// "raw-signed-tx" voucher form.
pub async fn sign_erc20_transfer(
    rpc: &TempoRpcClient,
    signer: &PrivateKeySigner,
    token_address: Address,
    calldata: Bytes,
) -> Result<Vec<u8>, RpcError> {
    sign_tx(rpc, signer, token_address, calldata).await
}

fn parse_hex_u64(s: &str) -> Result<u64, RpcError> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(stripped, 16).map_err(|e| RpcError::Hex(format!("u64 from {s}: {e}")))
}

fn parse_hex_u128(s: &str) -> Result<u128, RpcError> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    u128::from_str_radix(stripped, 16).map_err(|e| RpcError::Hex(format!("u128 from {s}: {e}")))
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
        let addr: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
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
            .and(body_partial_json(
                json!({"method": "eth_sendRawTransaction"}),
            ))
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
        assert!(
            matches!(err, RpcError::Http { status: 502, .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn estimate_gas_parses_hex_u64() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({"method": "eth_estimateGas"})))
            .respond_with(rpc_ok(json!("0x4b094"))) // 307_348 — observed Tempo testnet USDC transfer
            .mount(&server)
            .await;
        let rpc = TempoRpcClient::with_default_client(server.uri());
        let from: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let to: Address = "0x0000000000000000000000000000000000000002"
            .parse()
            .unwrap();
        let gas = rpc
            .estimate_gas(from, to, &Bytes::from_static(&[0xa9u8, 0x05, 0x9c, 0xbb]))
            .await
            .unwrap();
        assert_eq!(gas, 307_348);
    }

    #[tokio::test]
    async fn estimate_gas_rpc_error_propagates() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({"method": "eth_estimateGas"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": { "code": 3, "message": "execution reverted: ERC20: insufficient balance" },
            })))
            .mount(&server)
            .await;
        let rpc = TempoRpcClient::with_default_client(server.uri());
        let from: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let to: Address = "0x0000000000000000000000000000000000000002"
            .parse()
            .unwrap();
        let err = rpc
            .estimate_gas(from, to, &Bytes::new())
            .await
            .expect_err("revert must propagate");
        assert!(
            matches!(err, RpcError::Rpc { code: 3, .. }),
            "expected Rpc variant, got {err:?}"
        );
    }

    #[tokio::test]
    async fn sign_erc20_transfer_roundtrips_through_rpc() {
        use alloy_eips::eip2718::Decodable2718;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_chainId"})))
            .respond_with(rpc_ok(json!("0xa5bf"))) // 42431 testnet
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "eth_getTransactionCount"}),
            ))
            .respond_with(rpc_ok(json!("0x0")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_gasPrice"})))
            .respond_with(rpc_ok(json!("0x3b9aca00")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "eth_estimateGas"})))
            .respond_with(rpc_ok(json!("0x4b094"))) // 307_348
            .mount(&server)
            .await;
        let signer = PrivateKeySigner::random();
        let rpc = TempoRpcClient::with_default_client(server.uri());
        let token: Address = "0x00000000000000000000000000000000000000aa"
            .parse()
            .unwrap();
        // Real-looking ERC-20 transfer calldata: selector 0xa9059cbb + 32b addr + 32b amount.
        let calldata = Bytes::from_iter(
            [0xa9u8, 0x05, 0x9c, 0xbb]
                .into_iter()
                .chain(vec![0u8; 32 + 32]),
        );
        let raw = sign_erc20_transfer(&rpc, &signer, token, calldata)
            .await
            .expect("sign");
        assert!(!raw.is_empty(), "raw tx must encode to non-empty bytes");
        // Legacy tx RLP starts with list prefix 0xc0..0xf7 or 0xf8..0xff.
        assert!(
            raw[0] >= 0xc0,
            "legacy tx must be an RLP list, got first byte {:#x}",
            raw[0]
        );

        // Decode the signed envelope and assert the buffered gas limit:
        // floor(307_348 * 13 / 10) = 399_552.
        let envelope = TxEnvelope::decode_2718(&mut raw.as_slice()).expect("decode envelope");
        let TxEnvelope::Legacy(signed) = envelope else {
            panic!("expected legacy envelope");
        };
        assert_eq!(signed.tx().gas_limit, 399_552);
    }
}
