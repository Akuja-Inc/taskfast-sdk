//! End-to-end tests for `taskfast task` read path (list + get).
//!
//! Each test stands up a wiremock server, drives `cmd::task::run`
//! directly, and asserts on the JSON envelope shape.

use std::io::Write;

use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::task::{
    run, ApproveArgs, CancelArgs, Command, DisputeArgs, GetArgs, ListArgs, ListKind, SubmitArgs,
    TaskStatus,
};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

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
        Some(c) => json!({ "next_cursor": c, "has_more": true, "total_count": 0 }),
        None => json!({ "next_cursor": null, "has_more": false, "total_count": 0 }),
    }
}

#[tokio::test]
async fn list_mine_forwards_status_and_cursor_and_returns_tasks() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/tasks"))
        .and(query_param("status", "in_progress"))
        .and(query_param("cursor", "abc"))
        .and(query_param("limit", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": paginated(Some("next-abc")),
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        kind: ListKind::Mine,
        status: Some(TaskStatus::InProgress),
        cursor: Some("abc".into()),
        limit: 5,
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect("list mine should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["kind"], "mine");
    assert_eq!(v["data"]["meta"]["next_cursor"], "next-abc");
    assert_eq!(v["data"]["tasks"], json!([]));
}

#[tokio::test]
async fn list_queue_hits_queue_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": paginated(None),
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        kind: ListKind::Queue,
        status: None,
        cursor: None,
        limit: 20,
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect("list queue should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["kind"], "queue");
}

#[tokio::test]
async fn list_posted_hits_posted_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/posted_tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": paginated(None),
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        kind: ListKind::Posted,
        status: None,
        cursor: None,
        limit: 20,
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect("list posted should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["kind"], "posted");
}

#[tokio::test]
async fn list_status_with_non_mine_kind_is_usage_error() {
    // No server hit expected — usage error fires before any HTTP call.
    let server = MockServer::start().await;
    let args = ListArgs {
        kind: ListKind::Queue,
        status: Some(TaskStatus::Assigned),
        cursor: None,
        limit: 20,
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect_err("status + non-mine kind must fail");
    match err {
        CmdError::Usage(msg) => assert!(msg.contains("--status"), "got: {msg}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

/// Fixture builder for the minimum required fields of `AgentTaskSummary`
/// in the `/agents/me/tasks` response envelope.
fn mine_task(id: &str, status: &str) -> serde_json::Value {
    json!({
        "id": id,
        "title": format!("task {id}"),
        "description": "desc",
        "budget_max": "1.00",
        "status": status,
    })
}

#[tokio::test]
async fn list_mine_defaults_to_active_filter_when_status_absent() {
    // PLAN #12: raw `task list --kind mine` with no `--status` should
    // drop closed/cancelled noise client-side. Active set =
    // {assigned,in_progress,under_review,disputed,remedied}.
    let server = MockServer::start().await;
    let body = json!({
        "data": [
            mine_task("00000000-0000-0000-0000-000000000001", "in_progress"),
            mine_task("00000000-0000-0000-0000-000000000002", "completed"),
            mine_task("00000000-0000-0000-0000-000000000003", "assigned"),
            mine_task("00000000-0000-0000-0000-000000000004", "cancelled"),
            mine_task("00000000-0000-0000-0000-000000000005", "under_review"),
        ],
        "meta": paginated(None),
    });
    Mock::given(method("GET"))
        .and(path("/agents/me/tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let args = ListArgs {
        kind: ListKind::Mine,
        status: None,
        cursor: None,
        limit: 20,
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect("list mine should succeed");

    let v = envelope_value(&envelope);
    let tasks = v["data"]["tasks"].as_array().expect("tasks array");
    let statuses: Vec<String> = tasks
        .iter()
        .map(|t| t["status"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(statuses, vec!["in_progress", "assigned", "under_review"]);
}

#[tokio::test]
async fn list_mine_status_all_disables_active_filter() {
    // Escape hatch: `--status all` preserves the historical "everything"
    // view for operators who rely on it.
    let server = MockServer::start().await;
    let body = json!({
        "data": [
            mine_task("00000000-0000-0000-0000-000000000001", "in_progress"),
            mine_task("00000000-0000-0000-0000-000000000002", "completed"),
            mine_task("00000000-0000-0000-0000-000000000003", "cancelled"),
        ],
        "meta": paginated(None),
    });
    Mock::given(method("GET"))
        .and(path("/agents/me/tasks"))
        .and(query_param("status", "all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let args = ListArgs {
        kind: ListKind::Mine,
        status: Some(TaskStatus::All),
        cursor: None,
        limit: 20,
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect("list mine should succeed");

    let v = envelope_value(&envelope);
    let tasks = v["data"]["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 3, "--status all must not filter");
}

#[tokio::test]
async fn list_mine_default_filter_over_fetches_to_preserve_limit() {
    // Mitigation for Path B (client-side filter): over-fetch 2× so
    // `--limit` stays approximately honored even when some server rows
    // get filtered out. Strict pagination math is sacrificed — documented
    // caveat until the server grows an `Active` aggregate.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/tasks"))
        .and(query_param("limit", "10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": paginated(None),
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        kind: ListKind::Mine,
        status: None,
        cursor: None,
        limit: 5,
    };
    run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect("list mine should succeed");
}

#[tokio::test]
async fn list_mine_default_filter_truncates_to_limit() {
    // After over-fetch + filter, result must honor the user-supplied
    // limit — never surface more rows than they asked for.
    let server = MockServer::start().await;
    let body = json!({
        "data": [
            mine_task("00000000-0000-0000-0000-000000000001", "in_progress"),
            mine_task("00000000-0000-0000-0000-000000000002", "assigned"),
            mine_task("00000000-0000-0000-0000-000000000003", "under_review"),
            mine_task("00000000-0000-0000-0000-000000000004", "disputed"),
        ],
        "meta": paginated(None),
    });
    Mock::given(method("GET"))
        .and(path("/agents/me/tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let args = ListArgs {
        kind: ListKind::Mine,
        status: None,
        cursor: None,
        limit: 2,
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect("list mine should succeed");

    let v = envelope_value(&envelope);
    let tasks = v["data"]["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 2);
}

#[tokio::test]
async fn get_returns_task_detail() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": TASK_ID,
            "title": "test task",
            "status": "open",
            "description": "hello",
        })))
        .mount(&server)
        .await;

    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        Command::Get(GetArgs { id: TASK_ID.into() }),
    )
    .await
    .expect("get should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["task"]["id"], TASK_ID);
    assert_eq!(v["data"]["task"]["title"], "test task");
}

#[tokio::test]
async fn get_bad_uuid_is_usage_error_without_hitting_server() {
    let server = MockServer::start().await;
    // Deliberately no mock — a hit would 404 and fail the test.

    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Get(GetArgs {
            id: "not-a-uuid".into(),
        }),
    )
    .await
    .expect_err("bad uuid must error locally");
    match err {
        CmdError::Usage(msg) => assert!(msg.contains("UUID"), "got: {msg}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn get_404_surfaces_as_validation_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}")))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": "task_not_found",
            "message": "no task with that id",
        })))
        .mount(&server)
        .await;

    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Get(GetArgs { id: TASK_ID.into() }),
    )
    .await
    .expect_err("404 must surface as Validation per client mapping");
    match err {
        CmdError::Validation { code, .. } => assert_eq!(code, "task_not_found"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn list_401_surfaces_as_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/tasks"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        kind: ListKind::Mine,
        status: None,
        cursor: None,
        limit: 20,
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
async fn missing_api_key_errors_before_any_http_call() {
    let server = MockServer::start().await;
    let args = ListArgs {
        kind: ListKind::Mine,
        status: None,
        cursor: None,
        limit: 20,
    };
    let err = run(&ctx_for(&server, None), Command::List(args))
        .await
        .expect_err("no key → MissingApiKey");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
}

#[tokio::test]
async fn submit_zero_artifact_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/submit")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "task_id": TASK_ID,
            "status": "under_review",
            "message": "submitted",
            "evaluation": {
                "passed": true,
                "criteria_results": [],
                "evaluated_at": "2026-04-13T21:00:00Z",
            },
        })))
        .mount(&server)
        .await;

    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        Command::Submit(SubmitArgs {
            id: TASK_ID.into(),
            summary: "done".into(),
            artifact: vec![],
        }),
    )
    .await
    .expect("submit should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["artifacts"], json!([]));
    assert_eq!(v["data"]["submission"]["status"], "under_review");
}

#[tokio::test]
async fn submit_with_artifacts_uploads_each_then_submits() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let p1 = tmp.path().join("results.json");
    let p2 = tmp.path().join("notes.txt");
    {
        let mut f = std::fs::File::create(&p1).unwrap();
        f.write_all(br#"{"ok":true}"#).unwrap();
    }
    {
        let mut f = std::fs::File::create(&p2).unwrap();
        f.write_all(b"hello").unwrap();
    }

    let artifact1_id = "00000000-0000-0000-0000-0000000000f1";
    let artifact2_id = "00000000-0000-0000-0000-0000000000f2";

    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/artifacts")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": artifact1_id,
            "filename": "results.json",
            "content_type": "application/json",
            "url": "https://example/results.json",
            "size_bytes": 11,
            "created_at": "2026-04-13T21:00:00Z",
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/artifacts")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": artifact2_id,
            "filename": "notes.txt",
            "content_type": "text/plain",
            "url": "https://example/notes.txt",
            "size_bytes": 5,
            "created_at": "2026-04-13T21:00:01Z",
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/submit")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "task_id": TASK_ID,
            "status": "under_review",
            "message": "submitted",
            "evaluation": {
                "passed": true,
                "criteria_results": [],
                "evaluated_at": "2026-04-13T21:00:00Z",
            },
        })))
        .mount(&server)
        .await;

    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        Command::Submit(SubmitArgs {
            id: TASK_ID.into(),
            summary: "two files".into(),
            artifact: vec![p1.clone(), p2.clone()],
        }),
    )
    .await
    .expect("submit should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["artifacts"].as_array().unwrap().len(), 2);
    assert_eq!(v["data"]["artifacts"][0]["id"], artifact1_id);
    assert_eq!(v["data"]["artifacts"][1]["id"], artifact2_id);
    assert_eq!(v["data"]["submission"]["status"], "under_review");
}

#[tokio::test]
async fn submit_missing_artifact_file_is_usage_error_without_hitting_server() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Submit(SubmitArgs {
            id: TASK_ID.into(),
            summary: "x".into(),
            artifact: vec![std::path::PathBuf::from("/definitely/not/a/real/path.json")],
        }),
    )
    .await
    .expect_err("missing file must fail locally");
    match err {
        CmdError::Usage(msg) => assert!(msg.contains("not found"), "got: {msg}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn submit_bad_uuid_is_usage_error_without_hitting_server() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Submit(SubmitArgs {
            id: "not-a-uuid".into(),
            summary: "x".into(),
            artifact: vec![],
        }),
    )
    .await
    .expect_err("bad uuid must error locally");
    assert!(matches!(err, CmdError::Usage(_)), "got: {err:?}");
}

#[tokio::test]
async fn submit_dry_run_short_circuits_without_uploading() {
    // No mock — any HTTP call would fail the test.
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let p1 = tmp.path().join("x.txt");
    std::fs::write(&p1, b"abc").unwrap();

    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;
    let envelope = run(
        &ctx,
        Command::Submit(SubmitArgs {
            id: TASK_ID.into(),
            summary: "dry".into(),
            artifact: vec![p1.clone()],
        }),
    )
    .await
    .expect("dry-run submit should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_submit");
    assert_eq!(v["data"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["artifacts"][0], p1.display().to_string());
}

#[tokio::test]
async fn submit_upload_401_surfaces_as_auth_error() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let p1 = tmp.path().join("x.txt");
    std::fs::write(&p1, b"abc").unwrap();

    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/artifacts")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;

    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Submit(SubmitArgs {
            id: TASK_ID.into(),
            summary: "x".into(),
            artifact: vec![p1],
        }),
    )
    .await
    .expect_err("401 on upload must surface as Auth");
    match err {
        CmdError::Auth(_) => {}
        other => panic!("expected Auth, got {other:?}"),
    }
}

// ─── task approve ─────────────────────────────────────────────────────────

#[tokio::test]
async fn approve_happy_path_returns_task_id_and_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/approve")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": TASK_ID,
            "status": "complete",
        })))
        .mount(&server)
        .await;
    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        Command::Approve(ApproveArgs { id: TASK_ID.into() }),
    )
    .await
    .expect("approve ok");
    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["status"], "complete");
}

#[tokio::test]
async fn approve_dry_run_skips_http() {
    let server = MockServer::start().await; // no mocks
    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;
    let envelope = run(&ctx, Command::Approve(ApproveArgs { id: TASK_ID.into() }))
        .await
        .expect("dry-run ok");
    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_approve");
    assert_eq!(v["data"]["task_id"], TASK_ID);
}

#[tokio::test]
async fn approve_bad_uuid_is_usage_error_without_any_http() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Approve(ApproveArgs {
            id: "not-a-uuid".into(),
        }),
    )
    .await
    .expect_err("bad UUID");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn approve_403_surfaces_as_auth() {
    // 403 = "only the task poster can approve". Client maps 401|403 → Auth
    // (see taskfast_client::client::map_api_error) — forbidden is treated as
    // an auth problem, not a validation one. Orchestrators use the Auth exit
    // code to decide "re-credential" rather than "retry payload".
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/approve")))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": "not_poster",
            "message": "only the task poster can approve",
        })))
        .mount(&server)
        .await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Approve(ApproveArgs { id: TASK_ID.into() }),
    )
    .await
    .expect_err("403 must surface");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn approve_409_surfaces() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/approve")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "wrong_status",
            "message": "task is not in under_review",
        })))
        .mount(&server)
        .await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Approve(ApproveArgs { id: TASK_ID.into() }),
    )
    .await
    .expect_err("409 must surface");
    assert!(
        matches!(err, CmdError::Validation { .. } | CmdError::Server(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn approve_401_surfaces_as_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/approve")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Approve(ApproveArgs { id: TASK_ID.into() }),
    )
    .await
    .expect_err("401 must surface");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn approve_missing_api_key_errors_before_any_http() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server, None),
        Command::Approve(ApproveArgs { id: TASK_ID.into() }),
    )
    .await
    .expect_err("no key");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
}

// ─── task dispute ─────────────────────────────────────────────────────────

#[tokio::test]
async fn dispute_happy_path_returns_dispute_block() {
    let server = MockServer::start().await;
    let dispute_id = "00000000-0000-0000-0000-00000000d15b";
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/dispute")))
        .and(wiremock::matchers::body_partial_json(json!({
            "reason": "not what I asked for",
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "success": true,
            "task_id": TASK_ID,
            "status": "disputed",
            "message": "Dispute raised successfully",
            "dispute": {
                "id": dispute_id,
                "reason": "not what I asked for",
                "remedy_deadline": "2026-04-20T00:00:00Z",
            },
        })))
        .mount(&server)
        .await;
    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        Command::Dispute(DisputeArgs {
            id: TASK_ID.into(),
            reason: "not what I asked for".into(),
        }),
    )
    .await
    .expect("dispute ok");
    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["status"], "disputed");
    assert_eq!(v["data"]["dispute"]["id"], dispute_id);
    assert_eq!(v["data"]["dispute"]["reason"], "not what I asked for");
}

#[tokio::test]
async fn dispute_dry_run_skips_http() {
    let server = MockServer::start().await;
    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;
    let envelope = run(
        &ctx,
        Command::Dispute(DisputeArgs {
            id: TASK_ID.into(),
            reason: "because".into(),
        }),
    )
    .await
    .expect("dry-run ok");
    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_dispute");
    assert_eq!(v["data"]["reason"], "because");
}

#[tokio::test]
async fn dispute_bad_uuid_is_usage_error_without_any_http() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Dispute(DisputeArgs {
            id: "bad".into(),
            reason: "r".into(),
        }),
    )
    .await
    .expect_err("bad UUID");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn dispute_empty_reason_is_usage_error_without_any_http() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Dispute(DisputeArgs {
            id: TASK_ID.into(),
            reason: "   \n".into(),
        }),
    )
    .await
    .expect_err("empty reason");
    match err {
        CmdError::Usage(m) => assert!(m.contains("--reason"), "unexpected: {m}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn dispute_409_surfaces() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/dispute")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "already_disputed",
            "message": "dispute exists",
        })))
        .mount(&server)
        .await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Dispute(DisputeArgs {
            id: TASK_ID.into(),
            reason: "r".into(),
        }),
    )
    .await
    .expect_err("409 must surface");
    assert!(
        matches!(err, CmdError::Validation { .. } | CmdError::Server(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn dispute_401_surfaces_as_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/dispute")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Dispute(DisputeArgs {
            id: TASK_ID.into(),
            reason: "r".into(),
        }),
    )
    .await
    .expect_err("401");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}

// ─── task cancel ──────────────────────────────────────────────────────────

#[tokio::test]
async fn cancel_happy_path_returns_id_and_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/cancel")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": TASK_ID,
            "status": "cancelled",
        })))
        .mount(&server)
        .await;
    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        Command::Cancel(CancelArgs { id: TASK_ID.into() }),
    )
    .await
    .expect("cancel ok");
    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["id"], TASK_ID);
    assert_eq!(v["data"]["status"], "cancelled");
}

#[tokio::test]
async fn cancel_dry_run_skips_http() {
    let server = MockServer::start().await;
    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;
    let envelope = run(&ctx, Command::Cancel(CancelArgs { id: TASK_ID.into() }))
        .await
        .expect("dry-run ok");
    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_cancel");
    assert_eq!(v["data"]["task_id"], TASK_ID);
}

#[tokio::test]
async fn cancel_bad_uuid_is_usage_error_without_any_http() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Cancel(CancelArgs { id: "bad".into() }),
    )
    .await
    .expect_err("bad UUID");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn cancel_409_surfaces_non_cancellable_state() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/cancel")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "wrong_status",
            "message": "task cannot be cancelled from its current state",
        })))
        .mount(&server)
        .await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Cancel(CancelArgs { id: TASK_ID.into() }),
    )
    .await
    .expect_err("409 must surface");
    assert!(
        matches!(err, CmdError::Validation { .. } | CmdError::Server(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn cancel_401_surfaces_as_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/cancel")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Cancel(CancelArgs { id: TASK_ID.into() }),
    )
    .await
    .expect_err("401");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}
