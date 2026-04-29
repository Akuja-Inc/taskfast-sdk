//! End-to-end wiremock tests for `taskfast webhook`.
//!
//! Exercises the full pipeline for each subcommand: clap args → Ctx →
//! taskfast_agent::webhooks wrapper → envelope. Companion to
//! `taskfast-agent/tests/webhooks.rs` which covers the HTTP wrappers
//! in isolation.

use std::fs;

use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::webhook::{self, Command, RegisterArgs, SubscribeArgs};
use taskfast_cli::cmd::Ctx;
use taskfast_cli::{Envelope, Environment};

fn ctx_for(server: &MockServer, dry_run: bool) -> Ctx {
    Ctx {
        api_key: Some("test-key".into()),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path: std::path::PathBuf::from("/dev/null"),
        dry_run,
        quiet: true,
        ..Default::default()
    }
}

fn value(env: &Envelope) -> serde_json::Value {
    serde_json::to_value(env).expect("serialize envelope")
}

#[tokio::test]
async fn register_persists_fresh_secret_and_optionally_subscribes() {
    let server = MockServer::start().await;

    Mock::given(method("PUT"))
        .and(path("/agents/me/webhooks"))
        .and(body_partial_json(json!({
            "url": "https://example.com/hook",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created_at": "2026-04-13T00:00:00Z",
            "updated_at": "2026-04-13T00:00:00Z",
            "url": "https://example.com/hook",
            "events": ["task_assigned"],
            "secret": "whsec_first_time_only",
        })))
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/agents/me/webhooks/subscriptions"))
        .and(body_partial_json(json!({
            "subscribed_event_types": ["task_assigned"],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "subscribed_event_types": ["task_assigned"],
            "available_event_types": ["task_assigned", "bid_accepted"],
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let secret_path = tmp.path().join("hook.secret");

    let env = webhook::run(
        &ctx_for(&server, false),
        Command::Register(RegisterArgs {
            url: "https://example.com/hook".into(),
            secret_file: Some(secret_path.clone()),
            events: vec!["task_assigned".into()],
        }),
    )
    .await
    .expect("register succeeds");

    let v = value(&env);
    assert_eq!(v["data"]["action"], "registered");
    assert_eq!(v["data"]["secret_returned"], true);
    assert_eq!(v["data"]["secret_persisted"], true);
    assert_eq!(v["data"]["subscription"]["subscribed"][0], "task_assigned");

    let persisted = fs::read_to_string(&secret_path).unwrap();
    assert_eq!(persisted, "whsec_first_time_only");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&secret_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[tokio::test]
async fn register_leaves_existing_secret_file_untouched_when_server_returns_null() {
    // Idempotent re-register: server returns secret=null. The existing
    // secret file must not be clobbered — losing the secret is
    // unrecoverable per the spec.
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/agents/me/webhooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created_at": "2026-04-13T00:00:00Z",
            "updated_at": "2026-04-13T00:05:00Z",
            "url": "https://example.com/hook",
            "events": [],
            "secret": null,
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let secret_path = tmp.path().join("hook.secret");
    fs::write(&secret_path, "whsec_already_saved").unwrap();

    let env = webhook::run(
        &ctx_for(&server, false),
        Command::Register(RegisterArgs {
            url: "https://example.com/hook".into(),
            secret_file: Some(secret_path.clone()),
            events: Vec::new(),
        }),
    )
    .await
    .expect("re-register succeeds");

    let v = value(&env);
    assert_eq!(v["data"]["secret_returned"], false);
    assert_eq!(v["data"]["secret_persisted"], false);
    assert_eq!(
        fs::read_to_string(&secret_path).unwrap(),
        "whsec_already_saved",
        "existing secret must survive idempotent re-register",
    );
}

#[tokio::test]
async fn register_dry_run_skips_http_and_reports_intent() {
    let server = MockServer::start().await;
    // Deliberately no mounts: any hit would 404 and fail the test.
    let env = webhook::run(
        &ctx_for(&server, true),
        Command::Register(RegisterArgs {
            url: "https://example.com/hook".into(),
            secret_file: Some("/tmp/xyz".into()),
            events: vec!["task_assigned".into()],
        }),
    )
    .await
    .expect("dry-run succeeds without HTTP");

    let v = value(&env);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_register");
    assert_eq!(v["data"]["events"][0], "task_assigned");
}

#[tokio::test]
async fn test_delivery_surfaces_server_receipt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/agents/me/webhooks/test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "status_code": 200,
            "message": "Test webhook delivered successfully",
        })))
        .mount(&server)
        .await;

    let env = webhook::run(&ctx_for(&server, false), Command::Test)
        .await
        .expect("test delivery ok");

    let v = value(&env);
    assert_eq!(v["data"]["success"], true);
    assert_eq!(v["data"]["status_code"], 200);
}

#[tokio::test]
async fn subscribe_list_returns_current_and_available_types() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/webhooks/subscriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "subscribed_event_types": ["task_assigned"],
            "available_event_types": ["task_assigned", "bid_accepted"],
        })))
        .mount(&server)
        .await;

    let env = webhook::run(
        &ctx_for(&server, false),
        Command::Subscribe(SubscribeArgs {
            events: Vec::new(),
            default_events: false,
            list: true,
        }),
    )
    .await
    .expect("list ok");

    let v = value(&env);
    assert_eq!(v["data"]["subscribed"][0], "task_assigned");
    assert_eq!(v["data"]["available"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn subscribe_default_events_ships_canonical_worker_set() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/agents/me/webhooks/subscriptions"))
        .and(body_partial_json(json!({
            "subscribed_event_types": [
                "task_assigned", "bid_accepted", "bid_rejected",
                "pickup_deadline_warning", "payment_held", "payment_disbursed",
                "dispute_resolved", "review_received", "message_received",
            ],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "subscribed_event_types": [
                "task_assigned", "bid_accepted", "bid_rejected",
                "pickup_deadline_warning", "payment_held", "payment_disbursed",
                "dispute_resolved", "review_received", "message_received",
            ],
            "available_event_types": [
                "task_assigned", "bid_accepted", "bid_rejected",
                "pickup_deadline_warning", "payment_held", "payment_disbursed",
                "dispute_resolved", "review_received", "message_received",
                "task_disputed",
            ],
        })))
        .mount(&server)
        .await;

    let env = webhook::run(
        &ctx_for(&server, false),
        Command::Subscribe(SubscribeArgs {
            events: Vec::new(),
            default_events: true,
            list: false,
        }),
    )
    .await
    .expect("default subscribe ok");

    let v = value(&env);
    assert_eq!(v["data"]["subscribed"].as_array().unwrap().len(), 9);
}

#[tokio::test]
async fn subscribe_without_events_or_flags_is_usage_error() {
    let server = MockServer::start().await;
    // No mounts: the usage error must surface before any HTTP.
    let err = webhook::run(
        &ctx_for(&server, false),
        Command::Subscribe(SubscribeArgs {
            events: Vec::new(),
            default_events: false,
            list: false,
        }),
    )
    .await
    .expect_err("must demand an input");
    assert!(matches!(err, taskfast_cli::cmd::CmdError::Usage(_)));
}

#[tokio::test]
async fn get_returns_current_config() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/webhooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created_at": "2026-04-13T00:00:00Z",
            "updated_at": "2026-04-13T00:05:00Z",
            "url": "https://example.com/hook",
            "events": ["task_assigned"],
            "secret": null,
        })))
        .mount(&server)
        .await;

    let env = webhook::run(&ctx_for(&server, false), Command::Get)
        .await
        .expect("get ok");
    let v = value(&env);
    assert_eq!(v["data"]["url"], "https://example.com/hook");
}

#[tokio::test]
async fn delete_returns_action_deleted_on_204() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/agents/me/webhooks"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let env = webhook::run(&ctx_for(&server, false), Command::Delete)
        .await
        .expect("delete ok");
    let v = value(&env);
    assert_eq!(v["data"]["action"], "deleted");
}
