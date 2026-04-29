//! Wiremock tests for `taskfast discover`.

use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::discover::{run, Args, DiscoverAssignmentType, DiscoverStatus};
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

fn default_args() -> Args {
    Args {
        status: None,
        assignment_type: None,
        capabilities: vec![],
        budget_max: None,
        budget_min: None,
        cursor: None,
        limit: 50,
    }
}

#[tokio::test]
async fn discover_returns_task_list_envelope() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/tasks"))
        .and(query_param("status", "open"))
        .and(query_param("assignment_type", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "00000000-0000-0000-0000-000000000001", "title": "t1"},
                {"id": "00000000-0000-0000-0000-000000000002", "title": "t2"},
            ],
            "meta": {"next_cursor": null, "has_more": false, "total_count": 0}
        })))
        .mount(&server)
        .await;

    let args = Args {
        status: Some(DiscoverStatus::Open),
        assignment_type: Some(DiscoverAssignmentType::Open),
        ..default_args()
    };
    let envelope = run(&ctx_for(&server), args).await.expect("discover ok");
    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["tasks"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn discover_401_maps_to_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tasks"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "nope"})))
        .mount(&server)
        .await;
    let err = run(&ctx_for(&server), default_args())
        .await
        .expect_err("401");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}
