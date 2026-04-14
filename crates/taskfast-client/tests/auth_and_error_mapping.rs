//! Fixture tests for `TaskFastClient`: auth header injection + typed error
//! translation from wiremock responses.
//!
//! Covers the four error classes the CLI's exit-code taxonomy depends on:
//!   - 401 → `Error::Auth`           → exit code 3
//!   - 422 → `Error::Validation`     → exit code 7
//!   - 429 → `Error::RateLimited`    → exit code 4
//!   - 503 (exhausted retries) → `Error::Server` → exit code 6
//!
//! Also verifies the retry loop *does* retry on 5xx, and stops after
//! `max_attempts` without infinite-looping.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use taskfast_client::{map_api_error, Error, RetryPolicy, TaskFastClient};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// `GET /platform/config` is the ideal fixture endpoint — no auth required
/// *by contract*, no request body, no path params. We still send the
/// `X-API-Key` header (the client injects it unconditionally) and the mock
/// verifies presence.
fn fixture_client(base_url: &str) -> TaskFastClient {
    TaskFastClient::from_api_key(base_url, "test-key-123").expect("construct client")
}

#[tokio::test]
async fn x_api_key_header_is_injected() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/platform/config"))
        .and(header("x-api-key", "test-key-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let client = fixture_client(&server.uri());
    client
        .inner()
        .get_platform_config()
        .await
        .expect("header matcher satisfied");
}

#[tokio::test]
async fn status_401_maps_to_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/platform/config"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "invalid_api_key",
            "message": "API key is not recognized",
        })))
        .mount(&server)
        .await;

    let client = fixture_client(&server.uri());
    let err = client.inner().get_platform_config().await.unwrap_err();
    match map_api_error(err).await {
        Error::Auth(msg) => assert_eq!(msg, "HTTP 401: API key is not recognized"),
        other => panic!("expected Auth, got {other:?}"),
    }
}

#[tokio::test]
async fn status_422_maps_to_validation_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/platform/config"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "error": "missing_field",
            "message": "name is required",
        })))
        .mount(&server)
        .await;

    let client = fixture_client(&server.uri());
    let err = client.inner().get_platform_config().await.unwrap_err();
    match map_api_error(err).await {
        Error::Validation { code, message } => {
            assert_eq!(code, "missing_field");
            assert_eq!(message, "name is required");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn status_429_maps_to_rate_limited_with_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/platform/config"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "7")
                .set_body_json(
                    serde_json::json!({ "error": "rate_limited", "message": "slow down" }),
                ),
        )
        .mount(&server)
        .await;

    let client = fixture_client(&server.uri());
    let err = client.inner().get_platform_config().await.unwrap_err();
    match map_api_error(err).await {
        Error::RateLimited { retry_after } => {
            assert_eq!(retry_after, Duration::from_secs(7));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn status_429_without_retry_after_defaults_to_one_second() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/platform/config"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    let client = fixture_client(&server.uri());
    let err = client.inner().get_platform_config().await.unwrap_err();
    match map_api_error(err).await {
        Error::RateLimited { retry_after } => {
            assert_eq!(retry_after, Duration::from_secs(1));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

/// Counts requests. Each 503 is retried per RetryPolicy; after max_attempts
/// the call_with_retry wrapper surfaces Error::Server.
#[derive(Clone, Default)]
struct CountingRespond(Arc<AtomicUsize>);

impl Respond for CountingRespond {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.0.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": "upstream_down",
            "message": "database unreachable",
        }))
    }
}

#[tokio::test]
async fn retry_503_exhausts_max_attempts_then_returns_server_error() {
    let server = MockServer::start().await;
    let counter = CountingRespond::default();
    Mock::given(method("GET"))
        .and(path("/api/platform/config"))
        .respond_with(counter.clone())
        .mount(&server)
        .await;

    // Minimal delay policy so the test doesn't sleep seconds.
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
    };
    let client = fixture_client(&server.uri()).with_retry_policy(policy);
    let inner = client.inner();

    let result: Result<(), Error> = client
        .call_with_retry(|_attempt| async move {
            match inner.get_platform_config().await {
                Ok(_) => Ok(()),
                Err(e) => Err(map_api_error(e).await),
            }
        })
        .await;

    match result {
        Err(Error::Server(msg)) => assert_eq!(msg, "HTTP 503: database unreachable"),
        other => panic!("expected Server error after retry exhaustion, got {other:?}"),
    }
    assert_eq!(
        counter.0.load(Ordering::SeqCst),
        3,
        "expected exactly 3 attempts (max_attempts=3)"
    );
}

/// 5xx with eventual success on attempt 2 returns Ok and didn't exhaust retries.
#[derive(Clone)]
struct FailThenSucceed {
    count: Arc<AtomicUsize>,
    fail_until_attempt: usize,
}

impl Respond for FailThenSucceed {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        if n < self.fail_until_attempt {
            ResponseTemplate::new(503)
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({}))
        }
    }
}

#[tokio::test]
async fn retry_recovers_when_upstream_heals() {
    let server = MockServer::start().await;
    let responder = FailThenSucceed {
        count: Arc::new(AtomicUsize::new(0)),
        fail_until_attempt: 2,
    };
    Mock::given(method("GET"))
        .and(path("/api/platform/config"))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
    };
    let client = fixture_client(&server.uri()).with_retry_policy(policy);
    let inner = client.inner();

    let result: Result<(), Error> = client
        .call_with_retry(|_| async move {
            match inner.get_platform_config().await {
                Ok(_) => Ok(()),
                Err(e) => Err(map_api_error(e).await),
            }
        })
        .await;

    assert!(
        result.is_ok(),
        "expected success on attempt 2, got {result:?}"
    );
    assert_eq!(responder.count.load(Ordering::SeqCst), 2);
}
