//! Wiremock tests for `taskfast artifact`.

use std::io::Write;

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::artifact::{run, Command, GetArgs, ListArgs, UploadArgs};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const TASK: &str = "11111111-1111-1111-1111-111111111111";
const ART: &str = "22222222-2222-2222-2222-222222222222";

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
async fn artifact_list_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK}/artifacts")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": ART,
                "filename": "a.txt",
                "content_type": "text/plain",
                "size_bytes": 5,
                "created_at": "2026-01-01T00:00:00Z",
                "url": "https://example.com/a.txt",
            }],
            "meta": {"next_cursor": null, "has_more": false, "total_count": 1}
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
    let v = env_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["artifacts"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn artifact_delete_404_maps_to_server() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!("/tasks/{TASK}/artifacts/{ART}")))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "gone"})))
        .mount(&server)
        .await;
    let err = run(
        &ctx_for(&server),
        Command::Delete(GetArgs {
            task_id: TASK.into(),
            artifact_id: ART.into(),
        }),
    )
    .await
    .expect_err("404 surfaces");
    // 404 is an unmapped status → decode path or server path; the point is
    // it's surfaced as an error, not silently swallowed.
    assert_ne!(err.code(), "ok");
}

#[tokio::test]
async fn artifact_upload_dry_run_skips_network() {
    let server = MockServer::start().await;
    // No mocks registered — if we hit the network, wiremock returns 404
    // and the test fails with an error-envelope surfacing.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    writeln!(tmp.as_file(), "hello").unwrap();

    let ctx = Ctx {
        dry_run: true,
        ..ctx_for(&server)
    };
    let envelope = run(
        &ctx,
        Command::Upload(UploadArgs {
            task_id: TASK.into(),
            file: tmp.path().to_path_buf(),
        }),
    )
    .await
    .expect("dry-run upload ok");
    let v = env_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["action"], "would_upload_artifact");
}

#[tokio::test]
async fn artifact_upload_rejects_missing_file() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server),
        Command::Upload(UploadArgs {
            task_id: TASK.into(),
            file: "/nonexistent/file.txt".into(),
        }),
    )
    .await
    .expect_err("missing file → usage");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}
