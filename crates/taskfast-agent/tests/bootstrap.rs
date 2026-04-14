//! Wiremock fixtures for `taskfast_agent::bootstrap`.
//!
//! Each test covers one entry point's happy path plus one failure mode that
//! the function is *specifically responsible* for (vs. generic error-mapping
//! that's already proven in `taskfast-client` tests).
//!
//! The api_key-missing invariant on [`create_agent_headless`] is the only
//! bootstrap-layer invariant that can't fall through to the client's error
//! machinery — it gates a successful 201 response.

use taskfast_agent::bootstrap::{
    create_agent_headless, get_readiness, register_wallet, validate_auth, WalletRegistration,
};
use taskfast_client::api::types::{AgentCreateRequest, WalletSetupRequest};
use taskfast_client::{Error, TaskFastClient};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> TaskFastClient {
    TaskFastClient::from_api_key(&server.uri(), "test-key").expect("build client")
}

fn sample_create_request() -> AgentCreateRequest {
    AgentCreateRequest {
        capabilities: vec!["coding".into()],
        daily_spend_limit: None,
        description: "test agent".into(),
        max_task_budget: None,
        name: "alice".into(),
        owner_id: None,
        payment_method: None,
        payout_method: None,
        rate: None,
        tempo_wallet_address: None,
    }
}

#[tokio::test]
async fn validate_auth_returns_profile_on_200() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000042",
            "name": "alice",
            "status": "active",
            "capabilities": ["coding"],
        })))
        .mount(&server)
        .await;

    let profile = validate_auth(&client(&server)).await.expect("200 decodes");
    assert_eq!(profile.name.as_deref(), Some("alice"));
}

#[tokio::test]
async fn validate_auth_401_surfaces_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "invalid_api_key", "message": "bad key",
        })))
        .mount(&server)
        .await;

    match validate_auth(&client(&server)).await {
        Err(Error::Auth(m)) => assert_eq!(m, "HTTP 401: bad key"),
        other => panic!("expected Auth, got {other:?}"),
    }
}

#[tokio::test]
async fn create_agent_headless_returns_response_with_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/agents"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000099",
            "name": "alice",
            "api_key": "tsk_live_secret",
            "status": "active",
        })))
        .mount(&server)
        .await;

    let resp = create_agent_headless(&client(&server), &sample_create_request())
        .await
        .expect("201 with api_key");
    assert_eq!(resp.api_key.as_deref(), Some("tsk_live_secret"));
}

#[tokio::test]
async fn create_agent_headless_fails_when_response_omits_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/agents"))
        // 201 but no api_key — server contract violation.
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000099",
            "name": "alice",
            "status": "active",
        })))
        .mount(&server)
        .await;

    match create_agent_headless(&client(&server), &sample_create_request()).await {
        Err(Error::Server(m)) => assert!(
            m.contains("missing api_key"),
            "expected missing-api_key message, got: {m}"
        ),
        other => panic!("expected Server error, got {other:?}"),
    }
}

#[tokio::test]
async fn create_agent_headless_fails_when_api_key_empty_string() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/agents"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000099",
            "api_key": "",
            "status": "active",
        })))
        .mount(&server)
        .await;

    match create_agent_headless(&client(&server), &sample_create_request()).await {
        Err(Error::Server(m)) => assert!(m.contains("missing api_key")),
        other => panic!("empty api_key must fail, got {other:?}"),
    }
}

fn sample_wallet_request() -> WalletSetupRequest {
    WalletSetupRequest {
        tempo_wallet_address: "0x71C7656EC7ab88b098defB751B7401B5f6d8976F"
            .try_into()
            .expect("valid addr"),
    }
}

#[tokio::test]
async fn register_wallet_returns_configured_on_200() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/agents/me/wallet"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "payment_method": "tempo",
            "payout_method": "tempo_wallet",
            "ready_to_work": true,
            "tempo_wallet_address": "0x71C7656EC7ab88b098defB751B7401B5f6d8976F",
        })))
        .mount(&server)
        .await;

    match register_wallet(&client(&server), &sample_wallet_request()).await {
        Ok(WalletRegistration::Configured(r)) => {
            assert!(r.ready_to_work);
            assert_eq!(
                r.tempo_wallet_address,
                "0x71C7656EC7ab88b098defB751B7401B5f6d8976F"
            );
        }
        other => panic!("expected Configured, got {other:?}"),
    }
}

#[tokio::test]
async fn register_wallet_treats_409_already_configured_as_idempotent_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/agents/me/wallet"))
        .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
            "error": "wallet_already_configured",
            "message": "agent already has a tempo wallet configured",
        })))
        .mount(&server)
        .await;

    match register_wallet(&client(&server), &sample_wallet_request()).await {
        Ok(WalletRegistration::AlreadyConfigured) => {}
        other => panic!("expected AlreadyConfigured, got {other:?}"),
    }
}

#[tokio::test]
async fn register_wallet_propagates_other_validation_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/agents/me/wallet"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "error": "invalid_wallet_address",
            "message": "wallet address does not match pattern",
        })))
        .mount(&server)
        .await;

    match register_wallet(&client(&server), &sample_wallet_request()).await {
        Err(Error::Validation { code, .. }) => assert_eq!(code, "invalid_wallet_address"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn register_wallet_401_surfaces_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/agents/me/wallet"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;

    match register_wallet(&client(&server), &sample_wallet_request()).await {
        Err(Error::Auth(_)) => {}
        other => panic!("expected Auth, got {other:?}"),
    }
}

#[tokio::test]
async fn get_readiness_returns_checks() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me/readiness"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ready_to_work": false,
            "checks": {
                "api_key":  { "status": "ok" },
                "wallet":   { "status": "missing", "hint": "POST /agents/me/wallet" },
                "webhook":  { "status": "ok" },
            },
        })))
        .mount(&server)
        .await;

    let readiness = get_readiness(&client(&server)).await.expect("200 decodes");
    assert!(!readiness.ready_to_work);
}
