//! Wallet keygen, registration with TaskFast, and balance polling.
//!
//! Replaces `cast wallet new` + the `scripts/init.sh` wallet provisioning
//! block. Five entry points, each one step of the flow:
//!
//!   - [`generate_signer`] — `alloy` in-process keygen (replaces `cast wallet new`).
//!   - [`register_wallet`] — `POST /agents/me/wallet` with the validated address.
//!   - [`decode_wei`]      — hex wei → [`U256`]; the server returns `"0x0"` for zero.
//!   - [`fetch_balance_once`] — one-shot `GET /agents/me/wallet/balance`.
//!   - [`poll_balance`]    — repeats `fetch_balance_once` until the balance
//!     meets [`PollOptions::min_balance`] or [`PollOptions::timeout`] elapses.
//!
//! Hex decoding is the single invariant this layer adds on top of the
//! generated client: the server's wire shape is a hex string, but every
//! caller downstream wants a numeric comparison, so we centralize the
//! conversion here and surface a typed error rather than letting each
//! caller roll its own parser.

use std::time::Duration;

use alloy_primitives::U256;
use alloy_signer_local::PrivateKeySigner;
use taskfast_client::api::types::{
    WalletBalance, WalletSetupRequest, WalletSetupRequestTempoWalletAddress, WalletSetupResponse,
};
use taskfast_client::{Error, Result, TaskFastClient, map_api_error};
use tokio::time::{Instant, sleep};

/// Generate a fresh secp256k1 signer via `alloy_signer_local`.
///
/// The returned [`PrivateKeySigner`] owns the only copy of the private key;
/// callers are responsible for persisting it (keystore, OS keyring, env).
pub fn generate_signer() -> PrivateKeySigner {
    PrivateKeySigner::random()
}

/// `POST /agents/me/wallet` — tells TaskFast about the agent's wallet address.
///
/// The address string is validated against the 0x+40-hex pattern *before*
/// the HTTP call via the generated newtype, so a bad address fails locally
/// as [`Error::Validation`] rather than surfacing as a 422 after a round-trip.
pub async fn register_wallet(
    client: &TaskFastClient,
    address: &str,
) -> Result<WalletSetupResponse> {
    let validated =
        WalletSetupRequestTempoWalletAddress::try_from(address).map_err(|e| Error::Validation {
            code: "invalid_wallet_address".into(),
            message: e.to_string(),
        })?;
    let body = WalletSetupRequest {
        tempo_wallet_address: validated,
    };
    match client.inner().register_agent_wallet(&body).await {
        Ok(v) => Ok(v.into_inner()),
        Err(e) => Err(map_api_error(e).await),
    }
}

/// Parse a hex-encoded wei string into a [`U256`].
///
/// Accepts an optional `0x`/`0X` prefix. Empty strings and non-hex bytes are
/// protocol violations on the server's part — we surface them as
/// [`Error::Server`] so init.sh-style callers abort instead of spinning.
pub fn decode_wei(hex: &str) -> Result<U256> {
    let trimmed = hex
        .strip_prefix("0x")
        .or_else(|| hex.strip_prefix("0X"))
        .unwrap_or(hex);
    if trimmed.is_empty() {
        return Err(Error::Server(
            "decode_wei: expected hex digits, got empty string".into(),
        ));
    }
    if !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Server(format!(
            "decode_wei: non-hex characters in {hex:?}"
        )));
    }
    U256::from_str_radix(trimmed, 16)
        .map_err(|e| Error::Server(format!("decode_wei: {e} (input {hex:?})")))
}

/// One-shot `GET /agents/me/wallet/balance` returning decoded wei.
///
/// A missing `available_balance` field is treated as zero (server contract
/// allows it to be absent when the wallet is not yet indexed).
pub async fn fetch_balance_once(client: &TaskFastClient) -> Result<U256> {
    let resp: WalletBalance = match client.inner().get_wallet_balance().await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await),
    };
    match resp.available_balance.as_deref() {
        Some(s) => decode_wei(s),
        None => Ok(U256::ZERO),
    }
}

/// Knobs for [`poll_balance`]. Defaults mirror `scripts/init.sh`:
/// 10 s between polls, 120 s total, `>= 1 wei` counts as funded.
#[derive(Debug, Clone, Copy)]
pub struct PollOptions {
    pub min_balance: U256,
    pub timeout: Duration,
    pub poll_interval: Duration,
}

impl Default for PollOptions {
    fn default() -> Self {
        Self {
            min_balance: U256::from(1u8),
            timeout: Duration::from_secs(120),
            poll_interval: Duration::from_secs(10),
        }
    }
}

/// Poll the balance endpoint until it meets `min_balance` or we time out.
///
/// Always performs at least one fetch before checking the clock — a tight
/// timeout with a funded wallet still succeeds. Network/auth errors bubble
/// up on the first occurrence (we don't silently swallow them to keep
/// polling; a bad API key will never start working).
pub async fn poll_balance(client: &TaskFastClient, opts: PollOptions) -> Result<U256> {
    let start = Instant::now();
    loop {
        let balance = fetch_balance_once(client).await?;
        if balance >= opts.min_balance {
            return Ok(balance);
        }
        if start.elapsed() >= opts.timeout {
            return Err(Error::Server(format!(
                "wallet::poll_balance: timeout after {:?} (last balance {})",
                opts.timeout, balance
            )));
        }
        sleep(opts.poll_interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_wei_accepts_zero_variants() {
        assert_eq!(decode_wei("0x0").unwrap(), U256::ZERO);
        assert_eq!(decode_wei("0X0").unwrap(), U256::ZERO);
        assert_eq!(decode_wei("0").unwrap(), U256::ZERO);
        assert_eq!(
            decode_wei("0x0000000000000000000000000000000000000000000000000000000000000000")
                .unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn decode_wei_accepts_prefixed_and_bare_hex() {
        assert_eq!(decode_wei("0x1").unwrap(), U256::from(1u8));
        assert_eq!(decode_wei("ff").unwrap(), U256::from(255u16));
        assert_eq!(decode_wei("0xFF").unwrap(), U256::from(255u16));
    }

    #[test]
    fn decode_wei_accepts_u256_max() {
        let max_hex = format!("0x{}", "f".repeat(64));
        assert_eq!(decode_wei(&max_hex).unwrap(), U256::MAX);
    }

    #[test]
    fn decode_wei_rejects_empty_and_non_hex() {
        assert!(matches!(decode_wei(""), Err(Error::Server(_))));
        assert!(matches!(decode_wei("0x"), Err(Error::Server(_))));
        assert!(matches!(decode_wei("0xzz"), Err(Error::Server(_))));
        assert!(matches!(decode_wei("hello"), Err(Error::Server(_))));
    }

    #[test]
    fn generate_signer_yields_distinct_keys() {
        let a = generate_signer();
        let b = generate_signer();
        assert_ne!(a.address(), b.address());
    }
}
