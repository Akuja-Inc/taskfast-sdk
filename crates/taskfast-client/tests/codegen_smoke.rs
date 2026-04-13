//! End-to-end smoke test for the progenitor-generated client.
//!
//! Verifies that:
//!   1. The normalized spec produces a callable typed client.
//!   2. A 2xx JSON response deserializes into the expected generated type.
//!   3. A non-2xx response surfaces as `Error::UnexpectedResponse`, which is
//!      the contract our `taskfast-client::errors::Error` layer will consume.
//!
//! We deliberately pick `GET /platform/config` — no auth, no request body,
//! no path params — so the test exercises codegen wiring without fixture churn.

use taskfast_client::api::{Client, Error};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn platform_config_happy_path_roundtrips() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "submission_fee": "0.25",
        "submission_fee_currency": "USDC",
        "completion_fee_percent": 10,
        "max_task_duration_days": 7,
        "default_pickup_window_hours": 24,
        "default_review_window_hours": 24,
        "default_remedy_window_hours": 48,
        "max_open_count": 3,
        "tempo_platform_wallet": "0x2237a647792d76847D7764267598DD772d97d95d",
    });
    Mock::given(method("GET"))
        .and(path("/platform/config"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = Client::new(&server.uri());
    let resp = client
        .get_platform_config()
        .await
        .expect("generated client decodes 200");

    let cfg = resp.into_inner();
    assert_eq!(cfg.submission_fee.as_deref(), Some("0.25"));
    assert_eq!(cfg.completion_fee_percent, Some(10));
    assert_eq!(cfg.max_open_count, Some(3));
}

#[tokio::test]
async fn non_2xx_surfaces_as_unexpected_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/platform/config"))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_json(serde_json::json!({ "error": "unavailable", "message": "down" })),
        )
        .mount(&server)
        .await;

    let client = Client::new(&server.uri());
    let err = client
        .get_platform_config()
        .await
        .expect_err("503 must not decode as success");

    match err {
        Error::UnexpectedResponse(resp) => {
            assert_eq!(resp.status().as_u16(), 503);
        }
        other => panic!("expected UnexpectedResponse, got {other:?}"),
    }
}
