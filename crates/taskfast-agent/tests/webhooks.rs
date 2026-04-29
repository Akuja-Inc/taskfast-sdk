//! Wiremock fixtures for `taskfast_agent::webhooks` HTTP wrappers.
//!
//! Signature verification tests live in `src/webhooks.rs` — they're pure
//! functions, no server involved. This file exercises only the
//! HTTP-touching wrappers + the "secret returned once" server contract
//! invariant that callers must respect.

use taskfast_agent::webhooks::{
    configure_webhook, delete_webhook, get_subscriptions, get_webhook, test_webhook,
    update_subscriptions,
};
use taskfast_client::api::types::WebhookConfigRequest;
use taskfast_client::TaskFastClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> TaskFastClient {
    TaskFastClient::from_api_key(&server.uri(), "test-key").expect("build client")
}

fn sample_config_request() -> WebhookConfigRequest {
    WebhookConfigRequest {
        events: Some(vec!["task_assigned".into(), "payment_disbursed".into()]),
        secret: None,
        url: "https://example.com/webhook".into(),
    }
}

#[tokio::test]
async fn configure_webhook_returns_secret_on_first_creation() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/agents/me/webhooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "created_at": "2026-03-23T12:00:00Z",
            "updated_at": "2026-03-23T12:00:00Z",
            "url": "https://example.com/webhook",
            "events": ["task_assigned", "payment_disbursed"],
            "secret": "whsec_a1b2c3d4e5f6g7h8",
        })))
        .mount(&server)
        .await;

    let cfg = configure_webhook(&client(&server), &sample_config_request())
        .await
        .expect("200 decodes");
    assert_eq!(cfg.secret.as_deref(), Some("whsec_a1b2c3d4e5f6g7h8"));
}

#[tokio::test]
async fn configure_webhook_returns_null_secret_on_update() {
    // Idempotent PUT on an existing config — server contract says secret
    // is null from then on.
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/agents/me/webhooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "created_at": "2026-03-23T12:00:00Z",
            "updated_at": "2026-03-23T12:05:00Z",
            "url": "https://example.com/webhook-v2",
            "events": ["task_assigned"],
            "secret": null,
        })))
        .mount(&server)
        .await;

    let cfg = configure_webhook(&client(&server), &sample_config_request())
        .await
        .expect("200 decodes");
    assert!(cfg.secret.is_none(), "update must return null secret");
}

#[tokio::test]
async fn get_webhook_returns_config_with_null_secret() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/webhooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "created_at": "2026-03-23T12:00:00Z",
            "updated_at": "2026-03-23T12:05:00Z",
            "url": "https://example.com/webhook",
            "events": ["task_assigned"],
            "secret": null,
        })))
        .mount(&server)
        .await;

    let cfg = get_webhook(&client(&server)).await.expect("200 decodes");
    assert_eq!(cfg.url, "https://example.com/webhook");
    assert!(cfg.secret.is_none());
}

#[tokio::test]
async fn delete_webhook_accepts_204() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/agents/me/webhooks"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    delete_webhook(&client(&server)).await.expect("204 is Ok");
}

#[tokio::test]
async fn test_webhook_returns_delivery_receipt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/agents/me/webhooks/test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "status_code": 200,
            "message": "Test webhook delivered successfully",
        })))
        .mount(&server)
        .await;

    let receipt = test_webhook(&client(&server)).await.expect("200 decodes");
    assert!(receipt.success);
    assert_eq!(receipt.status_code, 200);
}

#[tokio::test]
async fn subscriptions_roundtrip() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/webhooks/subscriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "available_event_types": [
                "task_assigned", "bid_accepted", "payment_disbursed",
            ],
            "subscribed_event_types": ["task_assigned"],
        })))
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/agents/me/webhooks/subscriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "available_event_types": [
                "task_assigned", "bid_accepted", "payment_disbursed",
            ],
            "subscribed_event_types": ["task_assigned", "payment_disbursed"],
        })))
        .mount(&server)
        .await;

    let c = client(&server);
    let subs = get_subscriptions(&c).await.expect("get ok");
    assert_eq!(subs.subscribed_event_types, vec!["task_assigned"]);

    let updated =
        update_subscriptions(&c, vec!["task_assigned".into(), "payment_disbursed".into()])
            .await
            .expect("put ok");
    assert_eq!(updated.subscribed_event_types.len(), 2);
}
