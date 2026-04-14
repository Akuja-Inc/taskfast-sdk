//! Retry policy and exponential-backoff driver used by [`crate::TaskFastClient`].
//!
//! Only `Server`, `Network`, and `RateLimited` errors are retried; `Auth`,
//! `Validation`, and `Decode` short-circuit immediately because retrying them
//! cannot change the outcome.

use std::future::Future;
use std::time::Duration;

use crate::errors::{Error, Result};

/// Configuration for [`with_backoff`].
///
/// `max_attempts` includes the first try (so `max_attempts = 3` means up to
/// two retries). `base_delay` is the initial backoff; subsequent delays
/// double via `base_delay * 2^(attempt-1)`.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts including the first try.
    pub max_attempts: u32,
    /// Initial backoff duration; doubles each retry.
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(500),
        }
    }
}

/// Run `op` under `policy`, retrying transient failures with exponential backoff.
///
/// Retries on [`Error::Server`] / [`Error::Network`] (doubled `base_delay`) and
/// [`Error::RateLimited`] (sleeps for the server-supplied `retry_after`). Any
/// other variant — or exhausting `max_attempts` — propagates the last error.
///
/// `op` receives the 1-indexed attempt number so callers can log per-try state.
pub async fn with_backoff<T, F, Fut>(policy: RetryPolicy, mut op: F) -> Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 1u32;
    loop {
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(Error::Server(_) | Error::Network(_)) if attempt < policy.max_attempts => {
                let delay = policy.base_delay * 2u32.pow(attempt - 1);
                tracing::warn!(attempt, ?delay, "retrying transient error");
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(Error::RateLimited { retry_after }) if attempt < policy.max_attempts => {
                tracing::warn!(attempt, ?retry_after, "rate limited, honoring retry_after");
                tokio::time::sleep(retry_after).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}
