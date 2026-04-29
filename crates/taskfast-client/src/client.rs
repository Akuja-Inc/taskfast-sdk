//! Authenticated TaskFast client.
//!
//! [`TaskFastClient`] wraps the progenitor-generated [`crate::api::Client`]
//! with the cross-cutting concerns the generated code deliberately omits:
//!
//!   1. **Auth** — injects `X-API-Key` on every request via reqwest's
//!      `default_headers`, so every generated method call carries it without
//!      per-call plumbing.
//!   2. **Typed errors** — translates [`crate::api::Error<()>`] into
//!      [`crate::errors::Error`] by reading the response body of an
//!      `UnexpectedResponse` and classifying by status code. The normalizer
//!      strips non-2xx response declarations from the spec (see
//!      `xtask::normalize_spec`), so every TaskFast failure arrives here as
//!      `UnexpectedResponse` — we are the single funnel.
//!   3. **Retry** — [`TaskFastClient::call_with_retry`] runs a closure
//!      under the configured [`RetryPolicy`], honoring 429 `Retry-After`
//!      and retrying 5xx with exponential backoff.
//!
//! Direct access to the generated client is available via [`TaskFastClient::inner`]
//! so callers can invoke `client.inner().get_agent_profile().await` and then
//! pipe the result through [`map_api_error`] or the retry wrapper as they prefer.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use crate::api;
use crate::errors::{Error, Result};
use crate::retry::{with_backoff, RetryPolicy};

/// Default connect timeout for the underlying reqwest::Client.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Default total request timeout (connect + body read).
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap on any single response body we read (16 MiB). TaskFast
/// responses are JSON envelopes — the largest realistic payload is a
/// task list page, ~KBs. A 16-MiB ceiling gives generous headroom
/// while preventing a compromised (or buggy) server from memory-
/// exhausting the CLI with a gigabyte body.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Authenticated TaskFast API client.
pub struct TaskFastClient {
    inner: api::Client,
    policy: RetryPolicy,
    /// Per-client cache of the public `GET /config/network` payload.
    /// Fetched lazily on first access; immutable per deployment, so a
    /// process-lifetime cache is sufficient.
    network_cfg: OnceCell<NetworkConfigResponse>,
}

impl TaskFastClient {
    /// Construct a client from an API key. The key is sent as `X-API-Key`
    /// on every request; the underlying reqwest::Client marks the header
    /// sensitive so it won't appear in debug traces.
    ///
    /// `base_url` is the API host (e.g. `https://api.taskfast.app`). The
    /// generated client appends the unprefixed path keys from the spec.
    /// Callers pass hosts, not versioned paths; a trailing `/` is tolerated.
    pub fn from_api_key(base_url: &str, api_key: &str) -> Result<Self> {
        let mut value = HeaderValue::from_str(api_key)
            .map_err(|_| Error::Auth("api key contains invalid header bytes".into()))?;
        value.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert("X-API-Key", value);

        // F5: pin redirect policy to `none`. reqwest's default follows up
        // to 10 redirects. `Authorization` is stripped cross-host by
        // reqwest, but `X-API-Key` is a custom header so it would be
        // replayed verbatim to whatever 3xx `Location` the server points
        // at — exfiltrating the PAT on a single malicious redirect. The
        // real API never 3xx's, so refusing all redirects is safe: a 302
        // from `api.taskfast.app` is itself the anomaly we want to catch.
        let http = reqwest::ClientBuilder::new()
            .default_headers(headers)
            .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        let resolved = base_url.trim_end_matches('/').to_owned();

        Ok(Self {
            inner: api::Client::new_with_client(&resolved, http),
            policy: RetryPolicy::default(),
            network_cfg: OnceCell::new(),
        })
    }

    /// Underlying reqwest::Client, pre-configured with the `X-API-Key` header.
    /// Lets callers build requests outside the progenitor-generated surface
    /// (e.g. to forward a JSON-RPC body to `POST /rpc/{network}` via the
    /// same connection pool + default headers).
    pub fn http_client(&self) -> reqwest::Client {
        self.inner.client().clone()
    }

    /// Override the retry policy (default: [`RetryPolicy::default`]).
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Direct access to the generated typed client. Use to invoke endpoint
    /// methods; combine with [`map_api_error`] to surface typed errors.
    pub fn inner(&self) -> &api::Client {
        &self.inner
    }

    /// The retry policy in effect for [`Self::call_with_retry`].
    pub fn retry_policy(&self) -> RetryPolicy {
        self.policy
    }

    /// Run `op` under the configured retry policy. `op` receives the attempt
    /// number (1-indexed) and should produce a typed [`Result`]. Typical use:
    ///
    /// ```ignore
    /// let profile = client.call_with_retry(|_| async {
    ///     client.inner().get_agent_profile().await
    ///         .map(|r| r.into_inner())
    ///         .map_err(map_api_error).await // cannot await inside map_err
    /// }).await?;
    /// ```
    ///
    /// The callsite ergonomics bite is that [`map_api_error`] is async (it
    /// reads the response body). Callers should match the Result themselves:
    ///
    /// ```ignore
    /// client.call_with_retry(|_| async {
    ///     match client.inner().get_agent_profile().await {
    ///         Ok(ok) => Ok(ok.into_inner()),
    ///         Err(e) => Err(map_api_error(e).await),
    ///     }
    /// }).await
    /// ```
    pub async fn call_with_retry<T, F, Fut>(&self, op: F) -> Result<T>
    where
        F: FnMut(u32) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        with_backoff(self.policy, op).await
    }

    /// `GET /users/me` — fetch the human owner's display name + email.
    ///
    /// Hand-rolled because the endpoint is not yet in `spec/openapi.yaml`
    /// (server-side work in flight). 404 is promoted to `Ok(None)` so
    /// callers can gracefully fall back when running against an older
    /// server deployment; every other non-2xx is surfaced via the shared
    /// `classify_response` funnel.
    pub async fn get_user_profile(&self) -> Result<Option<UserProfile>> {
        let url = format!("{}/users/me", self.inner.baseurl());
        let resp = self.inner.client().get(url).send().await?;
        let status = resp.status();
        if status.is_success() {
            let bytes = read_body_capped(resp, MAX_RESPONSE_BYTES).await?;
            return Ok(Some(serde_json::from_slice::<UserProfile>(&bytes)?));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Err(classify_response(resp).await)
    }

    /// Upload a file artifact to `POST /tasks/{task_id}/artifacts`.
    ///
    /// Hand-rolled because progenitor's default templates skip `multipart/
    /// form-data` endpoints. We reuse the inner reqwest::Client (same auth
    /// headers, same timeouts) and funnel non-2xx responses through the
    /// same `classify_response` used for generated methods — so upload
    /// errors map to the same `Error` variants as every other call.
    pub async fn upload_artifact(
        &self,
        task_id: &uuid::Uuid,
        filename: String,
        content_type: String,
        bytes: Vec<u8>,
    ) -> Result<api::types::Artifact> {
        let url = format!("{}/tasks/{}/artifacts", self.inner.baseurl(), task_id);
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename)
            .mime_str(&content_type)
            .map_err(|e| Error::Server(format!("invalid content-type: {e}")))?;
        let form = reqwest::multipart::Form::new().part("file", part);
        let resp = self.inner.client().post(url).multipart(form).send().await?;
        if resp.status().is_success() {
            let bytes = read_body_capped(resp, MAX_RESPONSE_BYTES).await?;
            Ok(serde_json::from_slice::<api::types::Artifact>(&bytes)?)
        } else {
            Err(classify_response(resp).await)
        }
    }

    /// `GET /config/network` — per-network chain metadata.
    ///
    /// Public endpoint, no auth required; the `X-API-Key` header attached by
    /// `from_api_key` is harmless on the server side. Result is cached on
    /// this client instance for the rest of the process — the payload is
    /// immutable per deployment (operator-side config changes require a
    /// redeploy to take effect).
    ///
    /// Each entry's `rpc_url` points at this same deployment's authenticated
    /// `POST /rpc/{network}` proxy, NOT the upstream Tempo gateway.
    pub async fn fetch_network_config(&self) -> Result<&NetworkConfigResponse> {
        self.network_cfg
            .get_or_try_init(|| async {
                let url = format!("{}/config/network", self.inner.baseurl());
                let resp = self.inner.client().get(url).send().await?;
                if !resp.status().is_success() {
                    return Err(classify_response(resp).await);
                }
                let bytes = read_body_capped(resp, MAX_RESPONSE_BYTES).await?;
                Ok(serde_json::from_slice::<NetworkConfigResponse>(&bytes)?)
            })
            .await
    }

    /// `POST /rpc/{network}` — forward a JSON-RPC call through the
    /// operator's upstream proxy.
    ///
    /// `network` is the key into the `fetch_network_config` map
    /// (`"testnet"` / `"mainnet"`). Response body is returned verbatim as
    /// [`serde_json::Value`] — the proxy forwards the upstream JSON-RPC
    /// envelope without touching it, so callers parse `result` / `error`
    /// per the JSON-RPC 2.0 spec.
    ///
    /// 429 surfaces as [`Error::RateLimited`] with the `Retry-After`
    /// header honored by `classify_response`; 5xx as [`Error::Server`].
    pub async fn post_json_rpc(
        &self,
        network: &str,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/rpc/{}", self.inner.baseurl(), network);
        let resp = self.inner.client().post(url).json(request).send().await?;
        if !resp.status().is_success() {
            return Err(classify_response(resp).await);
        }
        let bytes = read_body_capped(resp, MAX_RESPONSE_BYTES).await?;
        Ok(serde_json::from_slice::<serde_json::Value>(&bytes)?)
    }

    /// `GET /agents/me/events` — raw JSON escape hatch.
    ///
    /// Mirrors the generated `list_agent_events` but returns
    /// [`serde_json::Value`] instead of the strict [`api::types::AgentEventListResponse`]
    /// so the agent layer can apply per-item tolerant decoding when a
    /// single malformed event would otherwise fail the whole page.
    /// Non-2xx responses still funnel through `classify_response`.
    pub async fn list_agent_events_raw(
        &self,
        cursor: Option<&str>,
        limit: Option<i64>,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/agents/me/events", self.inner.baseurl());
        let mut req = self.inner.client().get(url);
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(c) = cursor {
            query.push(("cursor", c.to_string()));
        }
        if let Some(l) = limit {
            query.push(("limit", l.to_string()));
        }
        if !query.is_empty() {
            req = req.query(&query);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(classify_response(resp).await);
        }
        let bytes = read_body_capped(resp, MAX_RESPONSE_BYTES).await?;
        Ok(serde_json::from_slice::<serde_json::Value>(&bytes)?)
    }
}

/// Stream `resp` body chunks into memory, refusing to allocate past `cap`.
///
/// Reqwest offers no native body-size cap on the ClientBuilder. Wrapping
/// `chunk()` is the cheap route: each call yields whatever reqwest has
/// buffered, so we can bail as soon as the running total would exceed
/// the ceiling — no need for a trusted `Content-Length` header (chunked
/// bodies from a malicious server carry none).
async fn read_body_capped(mut resp: reqwest::Response, cap: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if buf.len().saturating_add(chunk.len()) > cap {
            return Err(Error::Server(format!(
                "response body exceeds {cap}-byte safety cap"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Translate a progenitor [`api::Error<()>`] into a typed [`Error`].
///
/// Async because the `UnexpectedResponse` path has to consume the response
/// body to extract TaskFast's `{error, message}` envelope.
pub async fn map_api_error(err: api::Error<()>) -> Error {
    use api::Error as AE;
    match err {
        AE::InvalidRequest(m) => Error::Server(format!("invalid request: {m}")),
        AE::CommunicationError(e) => Error::Network(e),
        AE::InvalidUpgrade(e) => Error::Network(e),
        AE::ResponseBodyError(e) => Error::Network(e),
        AE::InvalidResponsePayload(_, e) => Error::Decode(e),
        AE::UnexpectedResponse(resp) => classify_response(resp).await,
        AE::ErrorResponse(_) => {
            // Unreachable with stripped-non-2xx normalization (E = ()), but we
            // refuse to panic — produce a descriptive Server error instead.
            Error::Server(
                "progenitor emitted typed ErrorResponse for E=() — spec normalization regression"
                    .into(),
            )
        }
        AE::PreHookError(m) => Error::Server(format!("pre-hook: {m}")),
        AE::PostHookError(m) => Error::Server(format!("post-hook: {m}")),
    }
}

/// Per-network chain configuration returned by `GET /config/network`.
///
/// Keys of `networks` are network names (`"testnet"`, `"mainnet"`);
/// networks the deployment does not support are simply absent.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkConfigResponse {
    /// Network name → chain metadata.
    pub networks: HashMap<String, NetworkConfigEntry>,
}

impl NetworkConfigResponse {
    /// Return the entry for `network` name or an `Error::Validation` if
    /// the deployment does not advertise it.
    pub fn entry(&self, network: &str) -> Result<&NetworkConfigEntry> {
        self.networks.get(network).ok_or_else(|| Error::Validation {
            code: "unknown_network".to_string(),
            message: format!("deployment does not advertise network `{network}`"),
        })
    }

    /// Reverse-lookup: find the entry whose `chain_id` matches. Used when
    /// the caller knows the chain ID (e.g. from escrow params) but not
    /// the network name. Errors if no entry matches.
    pub fn entry_by_chain_id(&self, chain_id: i64) -> Result<(&str, &NetworkConfigEntry)> {
        self.networks
            .iter()
            .find(|(_, e)| e.chain_id == chain_id)
            .map(|(name, entry)| (name.as_str(), entry))
            .ok_or_else(|| Error::Validation {
                code: "unknown_chain_id".to_string(),
                message: format!("no network in /config/network advertises chain_id={chain_id}"),
            })
    }
}

/// Chain metadata for a single Tempo network.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkConfigEntry {
    /// EVM chain ID (e.g. `4217` for Tempo mainnet, `42_431` for moderato testnet).
    pub chain_id: i64,
    /// Authenticated JSON-RPC proxy URL on this deployment
    /// (`{api_base}/rpc/{network}`). NOT the upstream Tempo gateway.
    pub rpc_url: String,
    /// WebSocket JSON-RPC URL on the native Tempo gateway (no proxy).
    pub wss_url: String,
    /// Block-explorer base URL for this network.
    pub explorer_url: String,
    /// Default stablecoin ticker; `None` when the deployment has not
    /// finalized a token for the network (e.g. mainnet pre-launch).
    pub default_stablecoin: Option<String>,
}

/// Owning human's display name + email, returned by `GET /users/me`.
/// Lives alongside the hand-rolled fetch until the endpoint is added to
/// `spec/openapi.yaml` and the type can be regenerated under
/// [`crate::api::types`].
#[derive(Debug, Clone, Deserialize)]
pub struct UserProfile {
    /// Display name for the owning human (e.g. `"Alice Smith"`).
    pub name: String,
    /// Owning human's email address.
    pub email: String,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    /// TaskFast's `components/schemas/Error.error` — short code.
    #[serde(default)]
    error: String,
    /// Human-readable detail.
    #[serde(default)]
    message: String,
}

/// Classify a non-2xx response by status code, reading the body to extract
/// TaskFast's `{error, message}` envelope when relevant.
///
/// Body read + JSON parse are best-effort: a transport failure mid-body or a
/// non-JSON payload collapses to an empty envelope (status line surfaces in
/// `Display`). Both failure modes emit a `tracing::warn` so silent classifier
/// drops are observable in logs.
async fn classify_response(resp: reqwest::Response) -> Error {
    let status = resp.status();
    let code = status.as_u16();
    let retry_after = parse_retry_after(&resp);
    // Cap the error body read at 64 KiB. TaskFast error envelopes are
    // `{error, message}` JSON objects — a megabyte-class body on a 4xx
    // is an abuse signal, not a legitimate payload. Smaller cap than
    // success bodies: error paths never carry task-list pages.
    let body = match read_body_capped(resp, 64 * 1024).await {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => {
            tracing::warn!(error = %e.kind(), %code, "classify_response: failed to read body");
            String::new()
        }
    };
    let parsed: ErrorBody = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => {
            if !body.is_empty() {
                tracing::warn!(error = %e, %code, "classify_response: body is not JSON; using raw text");
            }
            ErrorBody {
                error: String::new(),
                message: body,
            }
        }
    };

    match code {
        401 | 403 => Error::Auth(format_status(
            code,
            display_or_status(&parsed, status.as_str()),
        )),
        422 => Error::Validation {
            code: or_default(&parsed.error, "validation_error"),
            message: or_default(&parsed.message, "request body failed validation"),
        },
        429 => Error::RateLimited {
            retry_after: retry_after.unwrap_or(Duration::from_secs(1)),
        },
        500..=599 => Error::Server(format_status(
            code,
            display_or_status(&parsed, status.as_str()),
        )),
        // 4xx other than the above (404, 409, etc.) — progenitor would have
        // surfaced this as UnexpectedResponse because we stripped non-2xx from
        // the spec. Treat as a validation-ish failure so callers can branch.
        400..=499 => Error::Validation {
            code: or_default(&parsed.error, status.as_str()),
            message: or_default(
                &parsed.message,
                status.canonical_reason().unwrap_or("client error"),
            ),
        },
        _ => Error::Server(format_status(
            code,
            display_or_status(&parsed, status.as_str()),
        )),
    }
}

fn parse_retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let v = resp.headers().get(reqwest::header::RETRY_AFTER)?;
    let s = v.to_str().ok()?.trim();
    // Only seconds form supported for now; HTTP-date form can land later.
    s.parse::<u64>().ok().map(Duration::from_secs)
}

fn or_default(s: &str, fallback: &str) -> String {
    if s.is_empty() {
        fallback.to_string()
    } else {
        s.to_string()
    }
}

fn display_or_status<'a>(body: &'a ErrorBody, status: &'a str) -> &'a str {
    if !body.message.is_empty() {
        &body.message
    } else if !body.error.is_empty() {
        &body.error
    } else {
        status
    }
}

fn format_status(code: u16, detail: &str) -> String {
    format!("HTTP {code}: {detail}")
}
