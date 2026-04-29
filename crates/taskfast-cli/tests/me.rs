//! End-to-end wiremock test for `taskfast me`.
//!
//! Exercises the full pipeline: Ctx → Ctx::client() → bootstrap::{validate_auth,
//! get_readiness} → Envelope. This is the canary for the whole CLI stack;
//! if it breaks, every other subcommand will too.

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::me::{run, Args};
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

fn envelope_value(env: &Envelope) -> serde_json::Value {
    serde_json::to_value(env).expect("envelope serializes")
}

#[tokio::test]
async fn me_happy_path_returns_profile_readiness_envelope() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "00000000-0000-0000-0000-000000000042",
            "name": "alice",
            "status": "active",
            "capabilities": ["coding"],
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/agents/me/readiness"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ready_to_work": true,
            "checks": {
                "api_key": {"status": "complete"},
                "wallet": {"status": "complete"},
                "webhook": {"status": "not_configured", "required": false},
            },
            "settlement_domain": {
                "chain_id": 42431,
                "verifying_contract": "0x0000000000000000000000000000000000000000",
            },
        })))
        .mount(&server)
        .await;

    let envelope = run(&ctx_for(&server), Args { resume: false })
        .await
        .expect("me should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["environment"], "local");
    assert_eq!(v["data"]["ready_to_work"], true);
    assert_eq!(v["data"]["profile"]["name"], "alice");
    assert_eq!(v["data"]["readiness"]["ready_to_work"], true);
    assert_eq!(
        v["data"]["readiness"]["checks"]["webhook"]["status"],
        "not_configured"
    );
}

#[tokio::test]
async fn me_surfaces_not_ready_verbatim() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "alice",
            "status": "active",
            "capabilities": [],
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/agents/me/readiness"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ready_to_work": false,
            "checks": {
                "api_key": {"status": "complete"},
                "wallet": {
                    "status": "missing",
                    "hint": "POST /agents/me/wallet"
                },
                "webhook": {"status": "not_configured", "required": false},
            },
            "settlement_domain": {
                "chain_id": 42431,
                "verifying_contract": "0x0000000000000000000000000000000000000000",
            },
        })))
        .mount(&server)
        .await;

    let envelope = run(&ctx_for(&server), Args { resume: false })
        .await
        .expect("me should return success even if not ready");

    let v = envelope_value(&envelope);
    // `ok` reflects CLI success (we got data), NOT readiness — that's what
    // the duplicated `ready_to_work` field is for.
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["ready_to_work"], false);
    assert_eq!(
        v["data"]["readiness"]["checks"]["wallet"]["status"],
        "missing"
    );
    assert_eq!(
        v["data"]["readiness"]["checks"]["wallet"]["hint"],
        "POST /agents/me/wallet"
    );
}

#[tokio::test]
async fn me_maps_401_to_auth_error() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;

    let err = run(&ctx_for(&server), Args { resume: false })
        .await
        .expect_err("401 must surface");

    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
    assert_eq!(err.code(), "auth");
}

#[tokio::test]
async fn me_without_api_key_errors_with_missing_api_key() {
    let ctx = Ctx {
        api_key: None,
        environment: Environment::Local,
        api_base: Some("http://unused".into()),
        config_path: std::path::PathBuf::from("/dev/null"),
        dry_run: false,
        quiet: true,
        ..Default::default()
    };
    let err = run(&ctx, Args { resume: false })
        .await
        .expect_err("no api key → missing_api_key");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
    assert_eq!(err.code(), "missing_api_key");
}

#[tokio::test]
async fn me_resume_is_unimplemented_for_now() {
    let server = MockServer::start().await;
    let err = run(&ctx_for(&server), Args { resume: true })
        .await
        .expect_err("--resume deferred");
    assert!(matches!(err, CmdError::Unimplemented(_)), "got {err:?}");
}
