//! Agent onboarding: auth validation, headless creation, readiness gate.
//!
//! Mirrors the legacy `packages/agent/src/bootstrap.ts` shape (pre-Rust
//! rewrite) against the typed [`TaskFastClient`]. Three async entry points
//! drive `scripts/init.sh` steps 2-4 + 9:
//!
//!   - [`validate_auth`] — step 3, asserts the API key is valid.
//!   - [`create_agent_headless`] — step 2 BYOK-absent branch, returns the
//!     freshly-minted `api_key` (caller must persist; it never comes back).
//!   - [`get_readiness`] — steps 4 + 9, checks the onboarding gate is open.
//!
//! All three surface [`taskfast_client::Error`]. The create path adds one
//! invariant the generated client cannot enforce: the server *must* return
//! an `api_key`. If it doesn't, we fail loud rather than silently drop the
//! only credential the caller will ever see.

use taskfast_client::api::types::{
    AgentCreateRequest, AgentCreateResponse, AgentProfile, AgentReadiness, WalletSetupRequest,
    WalletSetupResponse,
};
use taskfast_client::{map_api_error, Error, Result, TaskFastClient};

/// Outcome of `POST /agents/me/wallet`. Idempotent re-registration surfaces
/// as [`WalletRegistration::AlreadyConfigured`] so init-style flows can treat
/// it as success without branching on a typed `Error`.
#[derive(Debug)]
pub enum WalletRegistration {
    /// 200 happy path — full wallet envelope returned by the server.
    Configured(WalletSetupResponse),
    /// 409 `wallet_already_configured` — server has no body to return; caller
    /// already has a wallet on file.
    AlreadyConfigured,
}

/// `GET /agents/me` — confirms the API key resolves to an active agent.
///
/// Use as the first step of any bootstrap flow; a 401 here means the key is
/// wrong, not that the agent's onboarding state is bad.
pub async fn validate_auth(client: &TaskFastClient) -> Result<AgentProfile> {
    match client.inner().get_agent_profile().await {
        Ok(v) => Ok(v.into_inner()),
        Err(e) => Err(map_api_error(e).await),
    }
}

/// `POST /agents` — creates a headless agent on behalf of a human API key.
///
/// The returned `api_key` is the credential the agent will use from this
/// point on; the server will not reveal it again. We refuse to return a
/// response with a missing/empty key — that's a server contract violation
/// and silently proceeding would strand the caller without credentials.
pub async fn create_agent_headless(
    client: &TaskFastClient,
    body: &AgentCreateRequest,
) -> Result<AgentCreateResponse> {
    let resp = match client.inner().register_agent(body).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await),
    };
    match resp.api_key.as_deref() {
        Some(k) if !k.is_empty() => Ok(resp),
        _ => Err(Error::Server(
            "createAgentHeadless: response missing api_key — cannot persist credentials".into(),
        )),
    }
}

/// `POST /agents/me/wallet` — bind a Tempo wallet address to the agent.
///
/// 200 returns the full envelope. 409 `wallet_already_configured` is promoted
/// to [`WalletRegistration::AlreadyConfigured`] so init-style callers can
/// treat re-registration as success without reparsing `Error::Validation`.
/// Every other failure propagates through `map_api_error` unchanged —
/// including 422 for pattern/conflict rejections.
pub async fn register_wallet(
    client: &TaskFastClient,
    body: &WalletSetupRequest,
) -> Result<WalletRegistration> {
    match client.inner().register_agent_wallet(body).await {
        Ok(v) => Ok(WalletRegistration::Configured(v.into_inner())),
        Err(e) => match map_api_error(e).await {
            Error::Validation { code, .. } if code == "wallet_already_configured" => {
                Ok(WalletRegistration::AlreadyConfigured)
            }
            other => Err(other),
        },
    }
}

/// `GET /agents/me/readiness` — onboarding checklist (wallet, webhook, etc.).
///
/// Callers gate further setup on `ready_to_work`; individual check `.status`
/// fields tell init.sh-style flows which remaining step to run.
pub async fn get_readiness(client: &TaskFastClient) -> Result<AgentReadiness> {
    match client.inner().get_agent_readiness().await {
        Ok(v) => Ok(v.into_inner()),
        Err(e) => Err(map_api_error(e).await),
    }
}
