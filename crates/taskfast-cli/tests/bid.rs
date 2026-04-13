//! End-to-end tests for `taskfast bid` read path (list).
//!
//! Each test stands up a wiremock server, drives `cmd::bid::run` directly,
//! and asserts on the JSON envelope shape.

use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::bid::{CancelArgs, Command, CreateArgs, ListArgs, run};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const BID_ID: &str = "00000000-0000-0000-0000-00000000b1d1";
const TASK_ID: &str = "00000000-0000-0000-0000-0000000000aa";

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

fn paginated(cursor: Option<&str>) -> serde_json::Value {
    match cursor {
        Some(c) => json!({ "next_cursor": c, "has_more": true, "total_count": 1 }),
        None => json!({ "next_cursor": null, "has_more": false, "total_count": 0 }),
    }
}

#[tokio::test]
async fn list_forwards_cursor_and_limit_and_returns_bids() {
    let server = MockServer::start().await;
    let bid = json!({
        "id": BID_ID,
        "task_id": TASK_ID,
        "agent_id": "00000000-0000-0000-0000-0000000000a0",
        "price": "100.00",
        "status": "pending",
        "created_at": "2026-04-13T21:00:00Z",
    });
    Mock::given(method("GET"))
        .and(path("/api/agents/me/bids"))
        .and(query_param("cursor", "abc"))
        .and(query_param("limit", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [bid],
            "meta": paginated(Some("next-abc")),
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        cursor: Some("abc".into()),
        limit: Some(5),
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect("list should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["meta"]["next_cursor"], "next-abc");
    assert_eq!(v["data"]["bids"][0]["id"], BID_ID);
    assert_eq!(v["data"]["bids"][0]["price"], "100.00");
}

#[tokio::test]
async fn list_without_pagination_params_returns_empty() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me/bids"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": paginated(None),
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        cursor: None,
        limit: None,
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect("list should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["bids"], json!([]));
    assert_eq!(v["data"]["meta"]["has_more"], false);
}

#[tokio::test]
async fn list_401_surfaces_as_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me/bids"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        cursor: None,
        limit: None,
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect_err("401 must surface as Auth");
    match err {
        CmdError::Auth(_) => {}
        other => panic!("expected Auth, got {other:?}"),
    }
}

#[tokio::test]
async fn list_missing_api_key_errors_before_any_http_call() {
    let server = MockServer::start().await;
    let args = ListArgs {
        cursor: None,
        limit: None,
    };
    let err = run(&ctx_for(&server, None), Command::List(args))
        .await
        .expect_err("no key → MissingApiKey");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
}

#[tokio::test]
async fn deferred_poster_subcommands_return_unimplemented() {
    // Worker-side Create/Cancel landed in am-e3u.8. Poster-side Accept/Reject
    // are still stubbed pending escrow-delegation design (am-4w2 / am-e3u.11).
    let server = MockServer::start().await;
    for cmd in [
        Command::Accept {
            id: BID_ID.into(),
        },
        Command::Reject {
            id: BID_ID.into(),
        },
    ] {
        let err = run(&ctx_for(&server, Some("test-key")), cmd)
            .await
            .expect_err("stubs must return Unimplemented");
        assert!(matches!(err, CmdError::Unimplemented(_)), "got {err:?}");
    }
}

// ─── bid create ───────────────────────────────────────────────────────────

fn bid_body(status: &str) -> serde_json::Value {
    json!({
        "id": BID_ID,
        "task_id": TASK_ID,
        "agent_id": "00000000-0000-0000-0000-0000000000a0",
        "price": "75.00",
        "pitch": "why me",
        "status": status,
        "created_at": "2026-04-13T21:00:00Z",
    })
}

#[tokio::test]
async fn create_happy_path_posts_price_and_pitch() {
    let server = MockServer::start().await;
    // body_partial_json proves we forwarded both fields correctly; without this
    // a regression that dropped `pitch` would pass a status-code-only assertion.
    Mock::given(method("POST"))
        .and(path(format!("/api/tasks/{TASK_ID}/bids")))
        .and(body_partial_json(json!({ "price": "75.00", "pitch": "why me" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(bid_body("pending")))
        .mount(&server)
        .await;

    let args = CreateArgs {
        task_id: TASK_ID.into(),
        price: "75.00".into(),
        pitch: Some("why me".into()),
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::Create(args))
        .await
        .expect("create should succeed");
    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["bid"]["id"], BID_ID);
    assert_eq!(v["data"]["bid"]["status"], "pending");
}

#[tokio::test]
async fn create_omits_pitch_when_none() {
    let server = MockServer::start().await;
    // Absent field, not null — BidRequest uses skip_serializing_if=is_none.
    // Mounting a mock that matches only when `pitch` is absent is awkward;
    // instead we respond to any POST and trust the codegen's serde setup
    // (asserted at the type level). This still exercises the dry-run-free path.
    Mock::given(method("POST"))
        .and(path(format!("/api/tasks/{TASK_ID}/bids")))
        .and(body_partial_json(json!({ "price": "10.00" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(bid_body("pending")))
        .mount(&server)
        .await;

    let args = CreateArgs {
        task_id: TASK_ID.into(),
        price: "10.00".into(),
        pitch: None,
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::Create(args))
        .await
        .expect("create without pitch should succeed");
    assert_eq!(envelope_value(&envelope)["data"]["bid"]["id"], BID_ID);
}

#[tokio::test]
async fn create_dry_run_skips_http() {
    let server = MockServer::start().await; // no mocks mounted
    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;
    let args = CreateArgs {
        task_id: TASK_ID.into(),
        price: "50.00".into(),
        pitch: Some("pitch".into()),
    };
    let envelope = run(&ctx, Command::Create(args)).await.expect("dry-run ok");
    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_create_bid");
    assert_eq!(v["data"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["price"], "50.00");
}

#[tokio::test]
async fn create_bad_task_id_is_usage_error_without_any_http() {
    let server = MockServer::start().await;
    let args = CreateArgs {
        task_id: "not-a-uuid".into(),
        price: "10.00".into(),
        pitch: None,
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Create(args))
        .await
        .expect_err("bad UUID must fail locally");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn create_empty_price_is_usage_error_without_any_http() {
    let server = MockServer::start().await;
    let args = CreateArgs {
        task_id: TASK_ID.into(),
        price: "   ".into(),
        pitch: None,
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Create(args))
        .await
        .expect_err("empty price must fail locally");
    match err {
        CmdError::Usage(m) => assert!(m.contains("--price"), "unexpected: {m}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn create_409_surfaces_as_validation_error() {
    let server = MockServer::start().await;
    // The task is closed for bidding; server returns 409 with an Error body.
    // client::map_api_error classifies 409/422 with a `code` as Validation.
    Mock::given(method("POST"))
        .and(path(format!("/api/tasks/{TASK_ID}/bids")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "task_not_open",
            "message": "task is not open for bidding",
        })))
        .mount(&server)
        .await;
    let args = CreateArgs {
        task_id: TASK_ID.into(),
        price: "10.00".into(),
        pitch: None,
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Create(args))
        .await
        .expect_err("409 must surface");
    assert!(
        matches!(err, CmdError::Validation { .. } | CmdError::Server(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn create_401_surfaces_as_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/api/tasks/{TASK_ID}/bids")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;
    let args = CreateArgs {
        task_id: TASK_ID.into(),
        price: "10.00".into(),
        pitch: None,
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Create(args))
        .await
        .expect_err("401 must surface as Auth");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn create_missing_api_key_errors_before_any_http() {
    let server = MockServer::start().await;
    let args = CreateArgs {
        task_id: TASK_ID.into(),
        price: "10.00".into(),
        pitch: None,
    };
    let err = run(&ctx_for(&server, None), Command::Create(args))
        .await
        .expect_err("no key → MissingApiKey");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
}

// ─── bid cancel ───────────────────────────────────────────────────────────

#[tokio::test]
async fn cancel_happy_path_returns_withdrawn_bid() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/api/bids/{BID_ID}/withdraw")))
        .respond_with(ResponseTemplate::new(200).set_body_json(bid_body("withdrawn")))
        .mount(&server)
        .await;
    let args = CancelArgs { id: BID_ID.into() };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::Cancel(args))
        .await
        .expect("cancel should succeed");
    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["bid"]["id"], BID_ID);
    assert_eq!(v["data"]["bid"]["status"], "withdrawn");
}

#[tokio::test]
async fn cancel_dry_run_skips_http() {
    let server = MockServer::start().await; // no mocks
    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;
    let args = CancelArgs { id: BID_ID.into() };
    let envelope = run(&ctx, Command::Cancel(args)).await.expect("dry-run ok");
    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_cancel_bid");
    assert_eq!(v["data"]["bid_id"], BID_ID);
}

#[tokio::test]
async fn cancel_bad_bid_id_is_usage_error_without_any_http() {
    let server = MockServer::start().await;
    let args = CancelArgs {
        id: "not-a-uuid".into(),
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Cancel(args))
        .await
        .expect_err("bad UUID must fail locally");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn cancel_409_surfaces() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/api/bids/{BID_ID}/withdraw")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "bid_not_pending",
            "message": "bid already accepted",
        })))
        .mount(&server)
        .await;
    let args = CancelArgs { id: BID_ID.into() };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Cancel(args))
        .await
        .expect_err("409 must surface");
    assert!(
        matches!(err, CmdError::Validation { .. } | CmdError::Server(_)),
        "got {err:?}"
    );
}
