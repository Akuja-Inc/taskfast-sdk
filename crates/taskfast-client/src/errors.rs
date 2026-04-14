//! Cross-cutting error type for the typed client.
//!
//! Variants mirror the orchestrator-centric exit-code buckets used by
//! `taskfast-cli` (see `map_api_error` in [`crate::client`]): callers branch
//! on `Auth` to re-credential, `Validation` to surface payload/state issues,
//! `RateLimited` to back off, `Server`/`Network` to retry, `Decode` to bail.

use std::time::Duration;

use thiserror::Error;

/// Convenience alias for `Result<T, [Error]>` returned by client APIs.
pub type Result<T> = std::result::Result<T, Error>;

/// Unified error surface for typed-client operations.
#[derive(Debug, Error)]
pub enum Error {
    /// Authentication or authorization failure (HTTP 401/403, bad header bytes,
    /// etc.). Caller should re-credential, not retry the same payload.
    #[error("auth: {0}")]
    Auth(String),

    /// Server rejected the request payload or current state (HTTP 4xx other
    /// than 401/403/429). `code` is the TaskFast `error` envelope tag (e.g.
    /// `"validation_error"`); `message` is the human-readable detail.
    #[error("validation ({code}): {message}")]
    Validation {
        /// TaskFast `components/schemas/Error.error` short code.
        code: String,
        /// Human-readable detail from the API response body.
        message: String,
    },

    /// Server responded 429 Too Many Requests. `retry_after` reflects the
    /// `Retry-After` header (or a 1-second default when absent/unparseable).
    #[error("rate limited (retry in {retry_after:?})")]
    RateLimited {
        /// Duration callers should sleep before re-issuing the request.
        retry_after: Duration,
    },

    /// Server-side failure (HTTP 5xx) or pre-flight client-side error that
    /// callers should treat as transient + retry-eligible.
    #[error("server: {0}")]
    Server(String),

    /// Underlying transport (DNS, connect, TLS, body stream) failed.
    #[error("network: {0}")]
    Network(#[from] reqwest::Error),

    /// Response body could not be deserialised into the expected schema.
    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),
}
