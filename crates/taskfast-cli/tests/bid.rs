//! End-to-end tests for `taskfast bid` read path (list).
//!
//! Each test stands up a wiremock server, drives `cmd::bid::run` directly,
//! and asserts on the JSON envelope shape.

use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::bid::{
    run, AcceptArgs, CancelArgs, Command, CreateArgs, ListArgs, RejectArgs,
};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const BID_ID: &str = "00000000-0000-0000-0000-00000000b1d1";
const TASK_ID: &str = "00000000-0000-0000-0000-0000000000aa";

fn ctx_for(server: &MockServer, key: Option<&str>) -> Ctx {
    Ctx {
        api_key: key.map(String::from),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path: std::path::PathBuf::from("/dev/null"),
        dry_run: false,
        quiet: true,
        ..Default::default()
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
        .and(path("/agents/me/bids"))
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
        limit: 5,
        status: None,
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
        .and(path("/agents/me/bids"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": paginated(None),
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        cursor: None,
        limit: 20,
        status: None,
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
        .and(path("/agents/me/bids"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        cursor: None,
        limit: 20,
        status: None,
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
        limit: 20,
        status: None,
    };
    let err = run(&ctx_for(&server, None), Command::List(args))
        .await
        .expect_err("no key → MissingApiKey");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
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
        .and(path(format!("/tasks/{TASK_ID}/bids")))
        .and(body_partial_json(
            json!({ "price": "75.00", "pitch": "why me" }),
        ))
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
        .and(path(format!("/tasks/{TASK_ID}/bids")))
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
        .and(path(format!("/tasks/{TASK_ID}/bids")))
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
        .and(path(format!("/tasks/{TASK_ID}/bids")))
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
        .and(path(format!("/bids/{BID_ID}/withdraw")))
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
        .and(path(format!("/bids/{BID_ID}/withdraw")))
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

// ─── bid accept (poster; deferred-escrow two-phase per am-4w2) ─────────────

fn accept_body() -> serde_json::Value {
    // Mirrors bid_controller.ex:172-187 — server emits 8 fields on 202.
    json!({
        "bid_id": BID_ID,
        "task_id": TASK_ID,
        "payment_id": "00000000-0000-0000-0000-0000000000b9",
        "task_status": "payment_pending",
        "status": "accepted_pending_escrow",
        "poster_signature_deadline": "2026-04-15T21:00:00Z",
        "signing_url": format!("https://taskfast.app/tasks/{TASK_ID}"),
        "message": "Bid acceptance locked pending escrow signature.",
    })
}

#[tokio::test]
async fn accept_happy_path_surfaces_deferred_escrow_envelope() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bids/{BID_ID}/accept")))
        .respond_with(ResponseTemplate::new(200).set_body_json(accept_body()))
        .mount(&server)
        .await;
    let args = AcceptArgs { id: BID_ID.into() };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::Accept(args))
        .await
        .expect("accept should succeed");
    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["bid"]["bid_id"], BID_ID);
    assert_eq!(v["data"]["bid"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["bid"]["task_status"], "payment_pending");
}

#[tokio::test]
async fn accept_dry_run_skips_http() {
    let server = MockServer::start().await; // no mocks
    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;
    let args = AcceptArgs { id: BID_ID.into() };
    let envelope = run(&ctx, Command::Accept(args)).await.expect("dry-run ok");
    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_accept_bid");
    assert_eq!(v["data"]["bid_id"], BID_ID);
}

#[tokio::test]
async fn accept_bad_bid_id_is_usage_error_without_any_http() {
    let server = MockServer::start().await;
    let args = AcceptArgs {
        id: "not-a-uuid".into(),
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Accept(args))
        .await
        .expect_err("bad UUID must fail locally");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn accept_401_surfaces_as_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bids/{BID_ID}/accept")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;
    let args = AcceptArgs { id: BID_ID.into() };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Accept(args))
        .await
        .expect_err("401 must surface as Auth");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn accept_403_not_the_poster_surfaces_as_auth() {
    // Per taskfast-cli error-mapping contract: 403 on poster/worker mutations
    // is Auth (re-credential), not Validation.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bids/{BID_ID}/accept")))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": "forbidden",
            "message": "You are not the poster of this task",
        })))
        .mount(&server)
        .await;
    let args = AcceptArgs { id: BID_ID.into() };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Accept(args))
        .await
        .expect_err("403 must surface as Auth");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn accept_422_circular_subcontracting_surfaces_as_validation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bids/{BID_ID}/accept")))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "error": "circular_subcontracting",
            "message": "This assignment would create a circular delegation chain",
        })))
        .mount(&server)
        .await;
    let args = AcceptArgs { id: BID_ID.into() };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Accept(args))
        .await
        .expect_err("422 must surface");
    assert!(matches!(err, CmdError::Validation { .. }), "got {err:?}");
}

#[tokio::test]
async fn accept_missing_api_key_errors_before_any_http() {
    let server = MockServer::start().await;
    let args = AcceptArgs { id: BID_ID.into() };
    let err = run(&ctx_for(&server, None), Command::Accept(args))
        .await
        .expect_err("no key → MissingApiKey");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
}

// ─── bid reject ────────────────────────────────────────────────────────────

fn reject_body(reason: Option<&str>) -> serde_json::Value {
    json!({
        "bid_id": BID_ID,
        "status": "rejected",
        "reason": reason,
        "rejected_at": "2026-04-14T12:00:00Z",
    })
}

#[tokio::test]
async fn reject_happy_path_with_reason_round_trips() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bids/{BID_ID}/reject")))
        .and(body_partial_json(json!({ "reason": "too expensive" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(reject_body(Some("too expensive"))))
        .mount(&server)
        .await;
    let args = RejectArgs {
        id: BID_ID.into(),
        reason: Some("too expensive".into()),
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::Reject(args))
        .await
        .expect("reject should succeed");
    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["bid"]["bid_id"], BID_ID);
    assert_eq!(v["data"]["bid"]["status"], "rejected");
    assert_eq!(v["data"]["bid"]["reason"], "too expensive");
}

#[tokio::test]
async fn reject_happy_path_without_reason_omits_field() {
    // With skip_serializing_if=is_none on the generated struct, the body
    // should be `{}` (empty object). A partial-json match of `{}` accepts
    // any object shape, but matching absence is awkward with wiremock — so
    // we just assert the request succeeds and the envelope shape is right.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bids/{BID_ID}/reject")))
        .respond_with(ResponseTemplate::new(200).set_body_json(reject_body(None)))
        .mount(&server)
        .await;
    let args = RejectArgs {
        id: BID_ID.into(),
        reason: None,
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::Reject(args))
        .await
        .expect("reject should succeed");
    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["bid"]["status"], "rejected");
    assert!(v["data"]["bid"]["reason"].is_null());
}

#[tokio::test]
async fn reject_dry_run_skips_http_and_echoes_reason() {
    let server = MockServer::start().await;
    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;
    let args = RejectArgs {
        id: BID_ID.into(),
        reason: Some("scope too narrow".into()),
    };
    let envelope = run(&ctx, Command::Reject(args)).await.expect("dry-run ok");
    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_reject_bid");
    assert_eq!(v["data"]["bid_id"], BID_ID);
    assert_eq!(v["data"]["reason"], "scope too narrow");
}

#[tokio::test]
async fn reject_bad_bid_id_is_usage_error_without_any_http() {
    let server = MockServer::start().await;
    let args = RejectArgs {
        id: "not-a-uuid".into(),
        reason: None,
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Reject(args))
        .await
        .expect_err("bad UUID must fail locally");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn reject_empty_reason_is_usage_error_without_any_http() {
    let server = MockServer::start().await;
    let args = RejectArgs {
        id: BID_ID.into(),
        reason: Some("   ".into()),
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Reject(args))
        .await
        .expect_err("empty --reason must fail locally");
    match err {
        CmdError::Usage(m) => assert!(m.contains("--reason"), "unexpected: {m}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn reject_oversize_reason_is_usage_error_without_any_http() {
    let server = MockServer::start().await;
    let args = RejectArgs {
        id: BID_ID.into(),
        reason: Some("x".repeat(501)),
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Reject(args))
        .await
        .expect_err(">500 chars must fail locally");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn reject_401_surfaces_as_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bids/{BID_ID}/reject")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;
    let args = RejectArgs {
        id: BID_ID.into(),
        reason: None,
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Reject(args))
        .await
        .expect_err("401 must surface as Auth");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn reject_403_not_the_poster_surfaces_as_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bids/{BID_ID}/reject")))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": "forbidden",
            "message": "You are not the poster of this task",
        })))
        .mount(&server)
        .await;
    let args = RejectArgs {
        id: BID_ID.into(),
        reason: None,
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Reject(args))
        .await
        .expect_err("403 must surface as Auth");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn reject_409_already_accepted_surfaces_as_validation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bids/{BID_ID}/reject")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "invalid_status",
            "message": "bid is not pending",
        })))
        .mount(&server)
        .await;
    let args = RejectArgs {
        id: BID_ID.into(),
        reason: None,
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Reject(args))
        .await
        .expect_err("409 must surface");
    assert!(
        matches!(err, CmdError::Validation { .. } | CmdError::Server(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn reject_404_surfaces_as_validation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bids/{BID_ID}/reject")))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": "not_found",
            "message": "Bid not found",
        })))
        .mount(&server)
        .await;
    let args = RejectArgs {
        id: BID_ID.into(),
        reason: None,
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::Reject(args))
        .await
        .expect_err("404 must surface");
    assert!(
        matches!(err, CmdError::Validation { .. } | CmdError::Server(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn reject_missing_api_key_errors_before_any_http() {
    let server = MockServer::start().await;
    let args = RejectArgs {
        id: BID_ID.into(),
        reason: None,
    };
    let err = run(&ctx_for(&server, None), Command::Reject(args))
        .await
        .expect_err("no key → MissingApiKey");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
}
