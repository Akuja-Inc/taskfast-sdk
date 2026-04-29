//! End-to-end wiremock test for `taskfast ping`.
//!
//! Pins the liveness-probe contract: single round-trip, pong + latency on
//! success, no retry on failure.

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::ping::{run, Args};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

fn ctx_for(server: &MockServer) -> Ctx {
    Ctx {
        api_key: Some("test-key".into()),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path: std::path::PathBuf::from("/dev/null"),
        dry_run: false,
        quiet: true,
        ..Default::default()
    }
}

fn anon_ctx_for(server: &MockServer) -> Ctx {
    Ctx {
        api_key: None,
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path: std::path::PathBuf::from("/dev/null"),
        dry_run: false,
        quiet: true,
        ..Default::default()
    }
}

fn envelope_value(env: &Envelope) -> serde_json::Value {
    serde_json::to_value(env).expect("envelope serializes")
}

#[tokio::test]
async fn ping_happy_path_returns_pong_and_latency() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "00000000-0000-0000-0000-000000000042",
            "name": "alice",
            "status": "active",
            "capabilities": [],
        })))
        .mount(&server)
        .await;

    let envelope = run(&ctx_for(&server), Args)
        .await
        .expect("ping should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["pong"], true);
    assert_eq!(v["data"]["endpoint"], "GET /agents/me");
    assert!(v["data"]["latency_ms"].is_u64(), "latency_ms must be u64");
    assert!(
        !v["data"]["base_url"].as_str().unwrap().ends_with("/api"),
        "base_url must not include the legacy /api suffix"
    );
}

#[tokio::test]
async fn ping_maps_401_to_auth_error() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;

    let err = run(&ctx_for(&server), Args)
        .await
        .expect_err("401 must surface");

    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
    assert_eq!(err.code(), "auth");
}

#[tokio::test]
async fn ping_anonymous_succeeds_on_any_http_response() {
    // No API key → anonymous reachability probe. Even a 404 at the root
    // counts as a pong because the host answered HTTP.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let envelope = run(&anon_ctx_for(&server), Args)
        .await
        .expect("anonymous ping should succeed on any HTTP response");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["pong"], true);
    assert_eq!(v["data"]["authenticated"], false);
    assert_eq!(v["data"]["endpoint"], "GET /");
    server.verify().await;
}

#[tokio::test]
async fn ping_anonymous_surfaces_network_error_when_host_unreachable() {
    // Port 1 is a sentinel: no listener, connect refused — proves the
    // anonymous path reports transport failures rather than swallowing
    // them into a false-positive pong.
    let ctx = Ctx {
        api_key: None,
        environment: Environment::Local,
        api_base: Some("http://127.0.0.1:1".into()),
        config_path: std::path::PathBuf::from("/dev/null"),
        dry_run: false,
        quiet: true,
        ..Default::default()
    };
    let err = run(&ctx, Args)
        .await
        .expect_err("unreachable host must surface as network error");
    assert!(matches!(err, CmdError::Network(_)), "got {err:?}");
    assert_eq!(err.code(), "network");
}

#[tokio::test]
async fn ping_does_not_retry_on_5xx() {
    // Single-attempt contract: a 500 must surface immediately, not trigger
    // the client's default 3-attempt retry budget. We pin this by asserting
    // the mock was hit exactly once.
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "internal",
            "message": "boom",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let err = run(&ctx_for(&server), Args)
        .await
        .expect_err("500 must surface");

    assert!(matches!(err, CmdError::Server(_)), "got {err:?}");
    // Mock server asserts expect(1) on drop; explicit verify for clarity.
    server.verify().await;
}
