//! Wiremock tests for `taskfast message`.

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::message::{run, Command, ConversationsArgs, ListArgs, SendArgs};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const TASK: &str = "11111111-1111-1111-1111-111111111111";

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
async fn message_send_posts_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK}/messages")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "message": {
                "id": "33333333-3333-3333-3333-333333333333",
                "content": "hi",
                "created_at": "2026-01-01T00:00:00Z",
                "sender": {"id": "44444444-4444-4444-4444-444444444444", "type": "agent"},
            }
        })))
        .mount(&server)
        .await;

    let envelope = run(
        &ctx_for(&server),
        Command::Send(SendArgs {
            task_id: TASK.into(),
            content: "hi".into(),
        }),
    )
    .await
    .expect("send ok");
    let v = env_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["message"]["message"]["content"], "hi");
}

#[tokio::test]
async fn message_send_empty_content_is_usage_error() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server),
        Command::Send(SendArgs {
            task_id: TASK.into(),
            content: "   ".into(),
        }),
    )
    .await
    .expect_err("empty content rejected");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn message_list_returns_envelope() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK}/messages")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": {"next_cursor": null, "has_more": false, "total_count": 0}
        })))
        .mount(&server)
        .await;
    let envelope = run(
        &ctx_for(&server),
        Command::List(ListArgs {
            task_id: TASK.into(),
            cursor: None,
            limit: None,
        }),
    )
    .await
    .expect("list ok");
    assert_eq!(env_value(&envelope)["ok"], true);
}

#[tokio::test]
async fn message_conversations_returns_envelope() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK}/conversations")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "conversations": [],
            "count": 0
        })))
        .mount(&server)
        .await;
    let envelope = run(
        &ctx_for(&server),
        Command::Conversations(ConversationsArgs {
            task_id: TASK.into(),
        }),
    )
    .await
    .expect("conversations ok");
    assert_eq!(env_value(&envelope)["ok"], true);
}
