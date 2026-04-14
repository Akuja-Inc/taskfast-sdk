//! Testnet faucet client. Wraps the public Tempo moderato faucet at
//! `POST https://docs.tempo.xyz/api/faucet`, which returns `{"data": [{tx...}]}`
//! with one entry per token dispensed (gas token + USDC).
//!
//! **Dev / staging only.** Production agents never hit a faucet — the
//! owning human funds the wallet manually at <https://wallet.tempo.xyz>
//! before the agent posts or settles. Callers must gate on network
//! before calling in; this module does not self-gate because it has no
//! network context of its own.
//!
//! Scope is narrow on purpose — we POST, parse enough to surface tx hashes
//! to the caller, and return. Balance polling is the caller's concern
//! (am-c74 is the beads ticket tracking the polling side).

use serde::Deserialize;

pub const TEMPO_TESTNET_FAUCET_URL: &str = "https://docs.tempo.xyz/api/faucet";

#[derive(Debug, thiserror::Error)]
pub enum FaucetError {
    #[error("faucet http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("faucet returned status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("faucet response missing data array")]
    MalformedResponse,
}

#[derive(Debug, Deserialize)]
struct FaucetResponse {
    #[serde(default)]
    data: Option<Vec<FaucetEntry>>,
}

#[derive(Debug, Deserialize)]
struct FaucetEntry {
    /// The faucet emits `{"hash": "0x..."}` per drop. Kept as a bare
    /// `hash` here to match the server; aliased in case the schema ever
    /// grows a `tx_hash` synonym without breaking us.
    #[serde(default, alias = "tx_hash")]
    hash: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

/// Summary of one faucet dispense — what token was dropped and the
/// broadcast tx hash, if the faucet surfaced one.
#[derive(Debug, Clone)]
pub struct FaucetDrop {
    pub token: Option<String>,
    pub tx_hash: Option<String>,
}

/// POST to the Tempo moderato faucet for `address` and return a summary
/// per dispensed token. The faucet response shape varies (gas token +
/// USDC come back as separate entries), so we keep the return loose.
pub async fn request_testnet_funds(
    http: &reqwest::Client,
    address: &str,
) -> Result<Vec<FaucetDrop>, FaucetError> {
    let resp = http
        .post(TEMPO_TESTNET_FAUCET_URL)
        .json(&serde_json::json!({ "address": address }))
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(FaucetError::Status {
            status: status.as_u16(),
            body,
        });
    }

    let parsed: FaucetResponse = resp.json().await?;
    let entries = parsed.data.ok_or(FaucetError::MalformedResponse)?;
    Ok(entries
        .into_iter()
        .map(|e| FaucetDrop {
            token: e.token,
            tx_hash: e.hash,
        })
        .collect())
}
