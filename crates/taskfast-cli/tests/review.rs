//! Wiremock tests for `taskfast review`.

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::review::{run, Command, CreateArgs, ListArgs};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const TASK: &str = "11111111-1111-1111-1111-111111111111";
const AGENT: &str = "44444444-4444-4444-4444-444444444444";
const REVIEWEE: &str = "55555555-5555-5555-5555-555555555555";

fn ctx_for(server: &MockServer) -> Ctx {
    Ctx {
        api_key: Some("k".into()),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path: std::path::PathBuf::from("/dev/null"),
        dry_run: false,
        quiet: true,
        ..Default::default()
    }
}

fn env_value(e: &Envelope) -> serde_json::Value {
    serde_json::to_value(e).unwrap()
}

#[tokio::test]
async fn review_create_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK}/reviews")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "review": {
                "id": "66666666-6666-6666-6666-666666666666",
                "comment": "great",
                "created_at": "2026-01-01T00:00:00Z",
                "rating": 5,
                "reviewee_id": REVIEWEE,
                "reviewer_id": AGENT,
                "task_id": TASK,
            }
        })))
        .mount(&server)
        .await;
    let envelope = run(
        &ctx_for(&server),
        Command::Create(CreateArgs {
            task_id: TASK.into(),
            reviewee_id: REVIEWEE.into(),
            rating: 5,
            comment: "great".into(),
        }),
    )
    .await
    .expect("create ok");
    assert_eq!(env_value(&envelope)["ok"], true);
}

#[tokio::test]
async fn review_create_rejects_out_of_range_rating() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server),
        Command::Create(CreateArgs {
            task_id: TASK.into(),
            reviewee_id: REVIEWEE.into(),
            rating: 6,
            comment: "x".into(),
        }),
    )
    .await
    .expect_err("6 > 5");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn review_list_by_task() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK}/reviews")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": {"next_cursor": null, "has_more": false, "total_count": 0}
        })))
        .mount(&server)
        .await;
    let envelope = run(
        &ctx_for(&server),
        Command::List(ListArgs {
            task: Some(TASK.into()),
            agent: None,
            cursor: None,
            limit: None,
        }),
    )
    .await
    .expect("list task ok");
    assert_eq!(env_value(&envelope)["ok"], true);
}

#[tokio::test]
async fn review_list_by_agent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/agents/{AGENT}/reviews")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": {"next_cursor": null, "has_more": false, "total_count": 0}
        })))
        .mount(&server)
        .await;
    let envelope = run(
        &ctx_for(&server),
        Command::List(ListArgs {
            task: None,
            agent: Some(AGENT.into()),
            cursor: None,
            limit: None,
        }),
    )
    .await
    .expect("list agent ok");
    assert_eq!(env_value(&envelope)["ok"], true);
}

#[tokio::test]
async fn review_list_without_axis_is_usage_error() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server),
        Command::List(ListArgs {
            task: None,
            agent: None,
            cursor: None,
            limit: None,
        }),
    )
    .await
    .expect_err("must pick axis");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}
