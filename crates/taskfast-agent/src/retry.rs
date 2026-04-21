//! Operation-level retry loops for business-layer flows (balance polling,
//! faucet waits, bid submission races, etc.).
//!
//! # Not to be confused with [`taskfast_client::retry`]
//!
//! The client's retry is scoped to HTTP transport: it retries only
//! `Server`/`Network`/`RateLimited` errors and exists to smooth over flaky
//! endpoints. This one wraps **arbitrary** async operations and retries on
//! **any** error. The policies differ in kind, not degree:
//!
//! - HTTP retry = "the request didn't land, try again"
//! - Agent retry = "the goal wasn't met yet (e.g., balance still zero), try
//!   again — regardless of why"
//!
//! # RateLimited special-case
//!
//! If the returned error is [`Error::RateLimited`] with a `retry_after`, the
//! loop sleeps that exact duration instead of the next exponential step.
//! This matches the semantic of the platform's 429 handler: the server told
//! us *when* to come back, so we respect it.

use std::future::Future;
use std::time::Duration;

use taskfast_client::{Error, Result};

/// Knobs for [`with_backoff`]. Defaults tuned for the worker hot loop:
/// 5 attempts over ~15 s of exponential wait (0.5 → 1 → 2 → 4 → 8 s) is
/// usually enough for transient hiccups without wedging an event-handler
/// on a genuine outage.
#[derive(Debug, Clone, Copy)]
pub struct BackoffOptions {
    pub max_attempts: u32,
    pub base_delay: Duration,
    /// Upper bound on a single sleep. `None` = no cap. Cap applies to the
    /// exponential branch only — `retry_after` from a server is honored
    /// verbatim (the server is authoritative on its own quota).
    pub max_delay: Option<Duration>,
}

impl Default for BackoffOptions {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Some(Duration::from_secs(30)),
        }
    }
}

/// Run `op` until it succeeds or `max_attempts` is exhausted.
///
/// - Success (`Ok(_)`): return immediately.
/// - [`Error::RateLimited`]: sleep `retry_after`, then retry.
/// - Any other error: sleep `base_delay * 2^(attempt-1)` (capped at
///   `max_delay`), then retry.
/// - Last attempt's error: returned as-is; loop does not swallow it.
///
/// `op` receives the 1-indexed attempt number, useful for logging or for
/// slightly varying the request between tries (e.g., probing a different
/// endpoint on attempt ≥ 2).
pub async fn with_backoff<T, F, Fut>(opts: BackoffOptions, mut op: F) -> Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    assert!(opts.max_attempts >= 1, "max_attempts must be at least 1");
    let mut attempt: u32 = 1;
    loop {
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(err) => {
                if attempt >= opts.max_attempts {
                    return Err(err);
                }
                let delay = delay_for(&err, attempt, &opts);
                // F6: log only the variant tag, never the inner message.
                // If a server ever echoes the X-API-Key header into an
                // error body, the full Display / Debug would carry that
                // secret into every log sink downstream.
                tracing::debug!(attempt, ?delay, kind = err.kind(), "retrying after error");
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

fn delay_for(err: &Error, attempt: u32, opts: &BackoffOptions) -> Duration {
    if let Error::RateLimited { retry_after } = err {
        return *retry_after;
    }
    // Exponential: base * 2^(attempt-1), guarding the shift against
    // overflow on absurdly large attempt counts.
    let shift = (attempt - 1).min(20);
    let multiplier = 1u32 << shift;
    let raw = opts.base_delay.saturating_mul(multiplier);
    match opts.max_delay {
        Some(cap) if raw > cap => cap,
        _ => raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    fn tiny_opts(max_attempts: u32) -> BackoffOptions {
        BackoffOptions {
            max_attempts,
            base_delay: Duration::from_millis(10),
            max_delay: Some(Duration::from_secs(1)),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn succeeds_on_first_try_without_sleeping() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let out: Result<u32> = with_backoff(tiny_opts(3), move |_| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(42u32)
            }
        })
        .await;
        assert_eq!(out.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_transient_errors_then_succeeds() {
        // Fail twice with Server, then succeed.
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let out: Result<&'static str> = with_backoff(tiny_opts(5), move |attempt| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                if attempt < 3 {
                    Err(Error::Server("boom".into()))
                } else {
                    Ok("ok")
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn returns_last_error_when_attempts_exhausted() {
        let out: Result<()> = with_backoff(tiny_opts(2), |_| async move {
            Err::<(), _>(Error::Server("persistent".into()))
        })
        .await;
        match out {
            Err(Error::Server(msg)) => assert_eq!(msg, "persistent"),
            other => panic!("expected Server error, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limited_honors_retry_after_exactly() {
        // First attempt: RateLimited(retry_after=7s). Second attempt: ok.
        // Paused runtime advances instantly when sleep is awaited, but we
        // assert the attempts sequence (1 → sleep → 2) was followed.
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let out: Result<u8> = with_backoff(tiny_opts(3), move |_attempt| {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(Error::RateLimited {
                        retry_after: Duration::from_secs(7),
                    })
                } else {
                    Ok(1u8)
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn non_retryable_errors_still_retry_at_agent_layer() {
        // Agent retry is "retry on anything" — unlike client retry, even
        // Validation gets retried (until attempts exhaust). This is the
        // caller's explicit choice: they wrapped the op because *they*
        // decided the operation was idempotent + worth re-trying.
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let out: Result<()> = with_backoff(tiny_opts(3), move |_| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(Error::Validation {
                    code: "x".into(),
                    message: "y".into(),
                })
            }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn delay_for_rate_limited_returns_retry_after_verbatim() {
        let opts = BackoffOptions {
            max_attempts: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Some(Duration::from_secs(5)),
        };
        let err = Error::RateLimited {
            retry_after: Duration::from_secs(12),
        };
        // max_delay cap does NOT apply to server-directed waits.
        assert_eq!(delay_for(&err, 1, &opts), Duration::from_secs(12));
    }

    #[test]
    fn delay_for_exponential_grows_then_caps() {
        let opts = BackoffOptions {
            max_attempts: 10,
            base_delay: Duration::from_millis(100),
            max_delay: Some(Duration::from_secs(1)),
        };
        let err = Error::Server("boom".into());
        assert_eq!(delay_for(&err, 1, &opts), Duration::from_millis(100));
        assert_eq!(delay_for(&err, 2, &opts), Duration::from_millis(200));
        assert_eq!(delay_for(&err, 3, &opts), Duration::from_millis(400));
        assert_eq!(delay_for(&err, 4, &opts), Duration::from_millis(800));
        // attempt 5 would be 1600ms > 1s cap.
        assert_eq!(delay_for(&err, 5, &opts), Duration::from_secs(1));
        // attempt 20 still capped — overflow guarded.
        assert_eq!(delay_for(&err, 20, &opts), Duration::from_secs(1));
    }

    #[test]
    fn delay_for_uncapped_is_uncapped() {
        let opts = BackoffOptions {
            max_attempts: 10,
            base_delay: Duration::from_millis(100),
            max_delay: None,
        };
        // Any non-RateLimited error takes the exponential branch; Server
        // keeps the test free of reqwest-error construction boilerplate.
        let err = Error::Server("unused".into());
        assert_eq!(delay_for(&err, 5, &opts), Duration::from_millis(1600));
    }

    #[tokio::test(start_paused = true)]
    async fn attempt_number_is_passed_to_closure() {
        let seen = Arc::new(Mutex::new(Vec::<u32>::new()));
        let s = seen.clone();
        let _: Result<()> = with_backoff(tiny_opts(3), move |attempt| {
            let s = s.clone();
            async move {
                s.lock().unwrap().push(attempt);
                Err::<(), _>(Error::Server("x".into()))
            }
        })
        .await;
        assert_eq!(*seen.lock().unwrap(), vec![1, 2, 3]);
    }
}
