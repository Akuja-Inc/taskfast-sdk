//! End-to-end tests for `taskfast events poll` (single-page read).
//!
//! Stands up a wiremock server, drives `cmd::events::run` directly, and
//! asserts on the JSON envelope shape + error mapping.

use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::events::{run, Command, PollArgs};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

fn ctx_for(server: &MockServer, key: Option<&str>) -> Ctx {
    Ctx {
        api_key: key.map(String::from),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        dry_run: false,
        quiet: true,
    }
}

fn envelope_value(env: &Envelope) -> serde_json::Value {
    serde_json::to_value(env).expect("serialize envelope")
}

#[tokio::test]
async fn poll_forwards_cursor_and_limit_and_returns_events() {
    let server = MockServer::start().await;
    let event = json!({
        "id": "00000000-0000-0000-0000-0000000000e1",
        "event": "task_disputed",
        "occurred_at": "2026-04-13T21:00:00Z",
        "task_id": "00000000-0000-0000-0000-0000000000aa",
        "data": { "reason": "late" },
    });
    Mock::given(method("GET"))
        .and(path("/api/agents/me/events"))
        .and(query_param("cursor", "abc"))
        .and(query_param("limit", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [event],
            "meta": { "next_cursor": "next-abc", "has_more": true, "total_count": 1 },
        })))
        .mount(&server)
        .await;

    let args = PollArgs {
        cursor: Some("abc".into()),
        limit: Some(3),
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::Poll(args))
        .await
        .expect("poll should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["meta"]["next_cursor"], "next-abc");
    assert_eq!(v["data"]["events"][0]["event"], "task_disputed");
}

#[tokio::test]
async fn poll_empty_page_returns_empty_events() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": { "next_cursor": null, "has_more": false, "total_count": 0 },
        })))
        .mount(&server)
        .await;

    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        Command::Poll(PollArgs {
            cursor: None,
            limit: None,
        }),
    )
    .await
    .expect("poll should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["events"], json!([]));
    assert_eq!(v["data"]["meta"]["has_more"], false);
}

#[tokio::test]
async fn poll_401_surfaces_as_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me/events"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;

    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Poll(PollArgs {
            cursor: None,
            limit: None,
        }),
    )
    .await
    .expect_err("401 must surface as Auth");
    match err {
        CmdError::Auth(_) => {}
        other => panic!("expected Auth, got {other:?}"),
    }
}

#[tokio::test]
async fn poll_missing_api_key_errors_before_any_http_call() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server, None),
        Command::Poll(PollArgs {
            cursor: None,
            limit: None,
        }),
    )
    .await
    .expect_err("no key → MissingApiKey");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
}
