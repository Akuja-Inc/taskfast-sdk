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

use std::future::Future;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;

use crate::api;
use crate::errors::{Error, Result};
use crate::retry::{with_backoff, RetryPolicy};

/// Default connect timeout for the underlying reqwest::Client.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Default total request timeout (connect + body read).
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Authenticated TaskFast API client.
pub struct TaskFastClient {
    inner: api::Client,
    policy: RetryPolicy,
}

impl TaskFastClient {
    /// Construct a client from an API key. The key is sent as `X-API-Key`
    /// on every request; the underlying reqwest::Client marks the header
    /// sensitive so it won't appear in debug traces.
    ///
    /// `base_url` is the API host (e.g. `https://api.taskfast.app`). The
    /// spec's server prefix (`/api`) is appended here — progenitor bakes
    /// the unprefixed path keys from the spec into generated code, so the
    /// prefix must be carried on the baseurl or every endpoint is off by one.
    /// Callers pass hosts, not versioned paths; a trailing `/` is tolerated.
    pub fn from_api_key(base_url: &str, api_key: &str) -> Result<Self> {
        let mut value = HeaderValue::from_str(api_key)
            .map_err(|_| Error::Auth("api key contains invalid header bytes".into()))?;
        value.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert("X-API-Key", value);

        let http = reqwest::ClientBuilder::new()
            .default_headers(headers)
            .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .build()?;

        let resolved = format!("{}/api", base_url.trim_end_matches('/'));

        Ok(Self {
            inner: api::Client::new_with_client(&resolved, http),
            policy: RetryPolicy::default(),
        })
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
            Ok(resp.json::<api::types::Artifact>().await?)
        } else {
            Err(classify_response(resp).await)
        }
    }
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
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, %code, "classify_response: failed to read body");
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
