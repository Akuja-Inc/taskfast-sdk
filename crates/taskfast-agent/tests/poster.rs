//! Wiremock fixtures for `taskfast_agent::poster`.
//!
//! Covers the two-phase task-creation flow: `prepare_task_draft` (returns
//! payload the caller signs offline) + `submit_task_draft` (exchanges the
//! signature for a live task). Each entry point gets a happy path plus the
//! failure modes the wrapper is specifically responsible for surfacing
//! distinctly to callers.

use taskfast_agent::poster::{create_task_draft, submit_task_draft};
use taskfast_client::api::types::{
    TaskDraftPrepareRequest, TaskDraftPrepareRequestAssignmentType, TaskDraftSubmitRequest,
};
use taskfast_client::{Error, TaskFastClient};
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> TaskFastClient {
    TaskFastClient::from_api_key(&server.uri(), "test-key").expect("build client")
}

fn sample_prepare_request() -> TaskDraftPrepareRequest {
    TaskDraftPrepareRequest {
        assignment_type: TaskDraftPrepareRequestAssignmentType::default(),
        budget_max: Some("5.00".into()),
        completion_criteria: vec![],
        description: "write a haiku about ledgers".into(),
        direct_agent_id: None,
        execution_deadline: None,
        pickup_deadline: None,
        poster_wallet_address: "0x71C7656EC7ab88b098defB751B7401B5f6d8976F"
            .try_into()
            .expect("valid addr"),
        required_capabilities: vec!["coding".into()],
        title: "haiku".into(),
    }
}

fn sample_submit_request() -> TaskDraftSubmitRequest {
    TaskDraftSubmitRequest {
        signature: "0xdeadbeef".try_into().expect("valid sig"),
    }
}

#[tokio::test]
async fn create_task_draft_returns_payload_on_201() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/task_drafts"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "draft_id": "00000000-0000-0000-0000-000000000123",
            "payload_to_sign": "0xdeadbeef",
            "token_address": "0x1111111111111111111111111111111111111111",
        })))
        .mount(&server)
        .await;

    let resp = create_task_draft(&client(&server), &sample_prepare_request())
        .await
        .expect("201 decodes");
    assert_eq!(
        resp.draft_id,
        Uuid::parse_str("00000000-0000-0000-0000-000000000123").unwrap()
    );
    assert!(!resp.payload_to_sign.is_empty());
    assert_eq!(
        resp.token_address,
        "0x1111111111111111111111111111111111111111"
    );
}

#[tokio::test]
async fn create_task_draft_422_surfaces_validation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/task_drafts"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "error": "validation_error",
            "message": "poster_wallet_address is required",
        })))
        .mount(&server)
        .await;

    match create_task_draft(&client(&server), &sample_prepare_request()).await {
        Err(Error::Validation { code, .. }) => assert_eq!(code, "validation_error"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn create_task_draft_401_surfaces_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/task_drafts"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;

    match create_task_draft(&client(&server), &sample_prepare_request()).await {
        Err(Error::Auth(_)) => {}
        other => panic!("expected Auth, got {other:?}"),
    }
}

#[tokio::test]
async fn submit_task_draft_returns_task_on_201() {
    let server = MockServer::start().await;
    let draft_id = Uuid::parse_str("00000000-0000-0000-0000-000000000123").unwrap();
    Mock::given(method("POST"))
        .and(path(format!("/api/task_drafts/{draft_id}/submit")))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000777",
            "status": "open",
            "submission_fee_status": "pending_confirmation",
            "submission_fee_tx_hash": "0xabc",
        })))
        .mount(&server)
        .await;

    let resp = submit_task_draft(&client(&server), &draft_id, &sample_submit_request())
        .await
        .expect("201 decodes");
    assert_eq!(resp.status, "open");
    assert_eq!(
        resp.submission_fee_status.as_deref(),
        Some("pending_confirmation")
    );
}

#[tokio::test]
async fn submit_task_draft_bad_signature_surfaces_validation() {
    let server = MockServer::start().await;
    let draft_id = Uuid::parse_str("00000000-0000-0000-0000-000000000123").unwrap();
    Mock::given(method("POST"))
        .and(path(format!("/api/task_drafts/{draft_id}/submit")))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_signature",
            "message": "signature verification failed",
        })))
        .mount(&server)
        .await;

    match submit_task_draft(&client(&server), &draft_id, &sample_submit_request()).await {
        Err(Error::Validation { code, .. }) => assert_eq!(code, "invalid_signature"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn submit_task_draft_503_surfaces_server() {
    let server = MockServer::start().await;
    let draft_id = Uuid::parse_str("00000000-0000-0000-0000-000000000123").unwrap();
    Mock::given(method("POST"))
        .and(path(format!("/api/task_drafts/{draft_id}/submit")))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": "platform_wallet_unconfigured",
            "message": "platform wallet not configured",
        })))
        .mount(&server)
        .await;

    match submit_task_draft(&client(&server), &draft_id, &sample_submit_request()).await {
        Err(Error::Server(_)) => {}
        other => panic!("expected Server, got {other:?}"),
    }
}
