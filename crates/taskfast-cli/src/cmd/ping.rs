//! `taskfast ping` — fast liveness probe.
//!
//! Two modes, chosen by whether an API key is present:
//!
//! * **Authenticated** (key present): `GET /agents/me` — validates
//!   network reachability, base URL, *and* key in one round-trip.
//! * **Anonymous** (no key): raw `GET` on the configured base URL —
//!   reachability only. Any HTTP response from the host counts as a pong;
//!   only a transport failure is an error. No TaskFast endpoint is
//!   unauthenticated, so the weaker signal is intentional.
//!
//! Both modes are **single-attempt** — the client's [`RetryPolicy`] is
//! bypassed on purpose. A diagnostic that silently retries hides the
//! signal the operator asked for ("is the server up *right now*?").
//!
//! Envelope `data` shape:
//! ```json
//! {
//!   "pong": true,
//!   "latency_ms": 42,
//!   "endpoint": "GET /agents/me",
//!   "base_url": "http://localhost:4000",
//!   "authenticated": true
//! }
//! ```
//!
//! [`RetryPolicy`]: taskfast_client::RetryPolicy

use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::json;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::map_api_error;

/// Connect timeout for the anonymous probe — short on purpose so `ping`
/// fails fast when the host is unreachable.
const ANON_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Total request timeout for the anonymous probe.
const ANON_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Parser)]
pub struct Args;

pub async fn run(ctx: &Ctx, _args: Args) -> CmdResult {
    let (latency_ms, endpoint, base_url, authenticated) = match ctx.api_key.as_deref() {
        Some(_) => probe_authenticated(ctx).await?,
        None => probe_anonymous(ctx).await?,
    };

    let data = json!({
        "pong": true,
        "latency_ms": latency_ms,
        "endpoint": endpoint,
        "base_url": base_url,
        "authenticated": authenticated,
    });
    Ok(Envelope::success(ctx.environment, ctx.dry_run, data))
}

async fn probe_authenticated(ctx: &Ctx) -> Result<(u64, &'static str, String, bool), CmdError> {
    let client = ctx.client()?;
    let base_url = client.inner().baseurl().clone();

    let started = Instant::now();
    let result = client.inner().get_agent_profile().await;
    let latency_ms = started.elapsed().as_millis() as u64;

    if let Err(e) = result {
        return Err(map_api_error(e).await.into());
    }
    Ok((latency_ms, "GET /agents/me", base_url, true))
}

async fn probe_anonymous(ctx: &Ctx) -> Result<(u64, &'static str, String, bool), CmdError> {
    let base_url = ctx.base_url().to_string();
    let http = reqwest::Client::builder()
        .connect_timeout(ANON_CONNECT_TIMEOUT)
        .timeout(ANON_REQUEST_TIMEOUT)
        .build()
        .map_err(|e| CmdError::Network(e.to_string()))?;

    let started = Instant::now();
    let resp = http
        .get(&base_url)
        .send()
        .await
        .map_err(|e| CmdError::Network(e.to_string()))?;
    let latency_ms = started.elapsed().as_millis() as u64;

    // Any HTTP response — even 4xx/5xx — proves the host is reachable and
    // speaking HTTP. Status-code semantics need an authenticated probe.
    let _ = resp.status();

    Ok((latency_ms, "GET /", base_url, false))
}
