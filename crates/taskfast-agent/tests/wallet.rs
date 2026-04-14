//! Wiremock fixtures for `taskfast_agent::wallet`.
//!
//! Unit tests for `decode_wei` live in `src/wallet.rs`; this file covers
//! the HTTP-touching entry points (`register_wallet`, `poll_balance`) plus
//! the pre-flight address validation that fails without any round-trip.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy_primitives::U256;
use taskfast_agent::wallet::{poll_balance, register_wallet, PollOptions};
use taskfast_client::{Error, TaskFastClient};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn client(server: &MockServer) -> TaskFastClient {
    TaskFastClient::from_api_key(&server.uri(), "test-key").expect("build client")
}

#[tokio::test]
async fn register_wallet_returns_setup_response_on_200() {
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

    let resp = register_wallet(
        &client(&server),
        "0x71C7656EC7ab88b098defB751B7401B5f6d8976F",
    )
    .await
    .expect("200 decodes");
    assert!(resp.ready_to_work);
    assert_eq!(
        resp.tempo_wallet_address,
        "0x71C7656EC7ab88b098defB751B7401B5f6d8976F"
    );
}

#[tokio::test]
async fn register_wallet_rejects_malformed_address_without_http() {
    // Server will refuse to match any request — if we accidentally hit the
    // wire, wiremock fails the test loudly. The point is that validation
    // short-circuits before we ever build a request.
    let server = MockServer::start().await;
    match register_wallet(&client(&server), "not-a-wallet").await {
        Err(Error::Validation { code, .. }) => {
            assert_eq!(code, "invalid_wallet_address");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

/// Respond with items from a fixed sequence, saturating at the last entry
/// so long-running polls don't panic when they outlast the script.
struct Sequence {
    responses: Arc<Mutex<Vec<ResponseTemplate>>>,
    cursor: AtomicUsize,
}

impl Sequence {
    fn new(responses: Vec<ResponseTemplate>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses)),
            cursor: AtomicUsize::new(0),
        }
    }
}

impl Respond for Sequence {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let guard = self.responses.lock().unwrap();
        let idx = self
            .cursor
            .fetch_add(1, Ordering::SeqCst)
            .min(guard.len() - 1);
        guard[idx].clone()
    }
}

fn balance_body(hex: &str) -> serde_json::Value {
    serde_json::json!({
        "available_balance": hex,
        "currency": "USDC",
        "payment_method": "tempo_wallet",
        "tempo_wallet_address": "0x71C7656EC7ab88b098defB751B7401B5f6d8976F",
    })
}

#[tokio::test]
async fn poll_balance_returns_once_threshold_is_met() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me/wallet/balance"))
        .respond_with(Sequence::new(vec![
            ResponseTemplate::new(200).set_body_json(balance_body("0x0")),
            ResponseTemplate::new(200).set_body_json(balance_body("0x0")),
            ResponseTemplate::new(200).set_body_json(balance_body("0x1000")),
        ]))
        .mount(&server)
        .await;

    let opts = PollOptions {
        min_balance: U256::from(1u8),
        timeout: Duration::from_secs(5),
        poll_interval: Duration::from_millis(10),
    };
    let balance = poll_balance(&client(&server), opts)
        .await
        .expect("poll resolves when balance crosses threshold");
    assert_eq!(balance, U256::from(0x1000u32));
}

#[tokio::test]
async fn poll_balance_times_out_when_balance_stays_zero() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me/wallet/balance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(balance_body("0x0")))
        .mount(&server)
        .await;

    let opts = PollOptions {
        min_balance: U256::from(1u8),
        timeout: Duration::from_millis(50),
        poll_interval: Duration::from_millis(10),
    };
    match poll_balance(&client(&server), opts).await {
        Err(Error::Server(m)) => {
            assert!(m.contains("timeout"), "expected timeout message, got: {m}")
        }
        other => panic!("expected Server timeout error, got {other:?}"),
    }
}

#[tokio::test]
async fn poll_balance_surfaces_server_decode_errors() {
    // Server returns garbage in available_balance — decode_wei rejects it,
    // and we surface the error rather than spinning forever.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me/wallet/balance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(balance_body("0xZZZ")))
        .mount(&server)
        .await;

    let opts = PollOptions {
        min_balance: U256::from(1u8),
        timeout: Duration::from_secs(5),
        poll_interval: Duration::from_millis(10),
    };
    match poll_balance(&client(&server), opts).await {
        Err(Error::Server(m)) => assert!(m.contains("non-hex"), "unexpected message: {m}"),
        other => panic!("expected Server decode error, got {other:?}"),
    }
}
