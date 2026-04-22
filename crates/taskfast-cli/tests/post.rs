//! End-to-end tests for `taskfast post`.
//!
//! Two wiremock servers: one for the TaskFast API (draft prepare + submit),
//! one for the Tempo JSON-RPC. The happy path wires them together through
//! the real `sign_and_broadcast_erc20_transfer` path so any regression in
//! tempo_rpc (nonce tag, RLP encoding) surfaces here too.

use alloy_signer_local::PrivateKeySigner;
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::post::{run, Args, AssignmentType, Network};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

fn ctx_for(server: &MockServer, key: Option<&str>) -> Ctx {
    Ctx {
        api_key: key.map(String::from),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path: std::path::PathBuf::from("/dev/null"),
        dry_run: false,
        quiet: true,
        // Mock RPC server runs on an ephemeral 127.0.0.1 port — not
        // well-known. Tests exercise the real post pipeline, so flip the
        // F2 opt-in (same as `--allow-custom-endpoints` from main).
        allow_custom_endpoints: true,
        ..Default::default()
    }
}

fn envelope_value(env: &Envelope) -> Value {
    serde_json::to_value(env).expect("serialize envelope")
}

/// Build a minimal Args with sensible defaults for the happy path. Callers
/// override just the fields they care about.
fn base_args(wallet_address: Option<String>, keystore: Option<String>) -> Args {
    Args {
        title: "test task".into(),
        description: "a test".into(),
        budget: Some("1.00".into()),
        capabilities: vec!["testing".into()],
        criteria: vec![],
        criteria_file: None,
        pickup_deadline: None,
        execution_deadline: None,
        assignment_type: AssignmentType::Open,
        direct_agent_id: None,
        wallet_address,
        keystore,
        wallet_password_file: None,
        rpc_url: None,
        network: Network::Testnet,
        yes: false,
    }
}

/// Mount `GET /api/config/network` so the runtime path's fetch of the
/// deployment's proxy URL succeeds. Returns the testnet rpc_url that will
/// be picked (i.e. `<api_base>/api/rpc/testnet`) so callers can point the
/// RPC proxy mocks at it if they need to.
async fn mount_network_config_mock(server: &MockServer) -> String {
    let uri = server.uri();
    let payload = json!({
        "networks": {
            "testnet": {
                "chain_id": 42431,
                "rpc_url": format!("{uri}/api/rpc/testnet"),
                "wss_url": "wss://testnet.example.invalid",
                "explorer_url": "https://explorer-testnet.example.invalid",
                "default_stablecoin": "PathUSD"
            },
            "mainnet": {
                "chain_id": 4217,
                "rpc_url": format!("{uri}/api/rpc/mainnet"),
                "wss_url": "wss://mainnet.example.invalid",
                "explorer_url": "https://explorer.example.invalid",
                "default_stablecoin": null
            }
        }
    });
    Mock::given(method("GET"))
        .and(path("/api/config/network"))
        .respond_with(ResponseTemplate::new(200).set_body_json(payload))
        .mount(server)
        .await;
    format!("{uri}/api/rpc/testnet")
}

/// Mount all four JSON-RPC methods the sign-and-broadcast path needs.
async fn mount_rpc_mocks(server: &MockServer, tx_hash_hex: &str) {
    let rpc_ok = |result: Value| {
        ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": result,
        }))
    };
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "eth_chainId"})))
        .respond_with(rpc_ok(json!("0xa5bf"))) // 42431 (testnet)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "eth_getTransactionCount"}),
        ))
        .respond_with(rpc_ok(json!("0x0")))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "eth_gasPrice"})))
        .respond_with(rpc_ok(json!("0x3b9aca00"))) // 1 gwei
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "eth_estimateGas"})))
        .respond_with(rpc_ok(json!("0x4b094"))) // 307_348 — canonical testnet USDC transfer
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "eth_sendRawTransaction"}),
        ))
        .respond_with(rpc_ok(json!(tx_hash_hex)))
        .mount(server)
        .await;
}

#[tokio::test]
async fn post_happy_path_end_to_end() {
    let api_server = MockServer::start().await;
    let rpc_server = MockServer::start().await;

    // Real keystore so the full prepare→sign→submit path exercises I/O.
    let signer = PrivateKeySigner::random();
    let wallet_addr = format!("{:#x}", signer.address());
    let tmp = tempfile::tempdir().expect("tempdir");
    let keystore_path = tmp.path().join("wallet.json");
    taskfast_agent::keystore::save_signer(&signer, &keystore_path, "pw").expect("keystore");
    let password_path = tmp.path().join("pw");
    std::fs::write(&password_path, b"pw").unwrap();

    let _ = mount_network_config_mock(&api_server).await;

    let draft_id = uuid::Uuid::new_v4();
    let task_id = uuid::Uuid::new_v4();
    // Real-looking ERC-20 `transfer(address,uint256)` calldata: 4b selector + 32b addr + 32b amount.
    let calldata_hex = {
        let mut buf = vec![0xa9u8, 0x05, 0x9c, 0xbb];
        buf.extend([0u8; 32]);
        buf.extend([0u8; 32]);
        format!("0x{}", hex::encode(buf))
    };
    let token_addr = "0x20c0000000000000000000000000000000000000";
    let tx_hash_hex = format!("0x{}", "aa".repeat(32));

    Mock::given(method("POST"))
        .and(path("/api/task_drafts"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "draft_id": draft_id,
            "payload_to_sign": calldata_hex,
            "token_address": token_addr,
        })))
        .mount(&api_server)
        .await;

    Mock::given(method("POST"))
        .and(path(format!("/api/task_drafts/{draft_id}/submit")))
        .and(body_partial_json(
            json!({ "signature": tx_hash_hex.clone() }),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": task_id,
            "status": "open",
            "submission_fee_status": "pending_confirmation",
            "submission_fee_tx_hash": tx_hash_hex,
        })))
        .mount(&api_server)
        .await;

    mount_rpc_mocks(&rpc_server, &tx_hash_hex).await;

    let mut args = base_args(
        Some(wallet_addr.clone()),
        Some(keystore_path.display().to_string()),
    );
    args.wallet_password_file = Some(password_path);
    args.rpc_url = Some(rpc_server.uri());

    let envelope = run(&ctx_for(&api_server, Some("test-key")), args)
        .await
        .expect("post should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["task_id"], task_id.to_string());
    assert_eq!(v["data"]["draft_id"], draft_id.to_string());
    assert_eq!(v["data"]["submission_fee_tx_hash"], tx_hash_hex);
    assert_eq!(v["data"]["status"], "open");
    assert_eq!(v["data"]["submission_fee_status"], "pending_confirmation");
}

#[tokio::test]
async fn post_dry_run_short_circuits_without_any_http() {
    // No mocks — any network call fails the test.
    let api_server = MockServer::start().await;

    let args = base_args(
        Some("0x0000000000000000000000000000000000000001".into()),
        None, // keystore not required in dry-run
    );
    let mut ctx = ctx_for(&api_server, Some("test-key"));
    ctx.dry_run = true;

    let envelope = run(&ctx, args).await.expect("dry-run should succeed");
    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_post");
    assert_eq!(v["data"]["draft_id"], Value::Null);
    assert_eq!(v["data"]["title"], "test task");
    assert_eq!(v["data"]["assignment_type"], "open");
    // Dry-run predicts the proxy URL locally (no HTTP); shape is
    // `{api_base}/api/rpc/{network}`.
    assert!(
        v["data"]["rpc_url"]
            .as_str()
            .unwrap()
            .ends_with("/api/rpc/testnet"),
        "dry-run rpc_url: {}",
        v["data"]["rpc_url"]
    );
}

#[tokio::test]
async fn post_missing_wallet_address_is_usage_error_without_any_http() {
    let api_server = MockServer::start().await;
    let args = base_args(None, None);
    let err = run(&ctx_for(&api_server, Some("test-key")), args)
        .await
        .expect_err("missing wallet must fail locally");
    match err {
        CmdError::Usage(msg) => assert!(
            msg.contains("--wallet-address"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn post_bad_wallet_address_is_usage_error() {
    let api_server = MockServer::start().await;
    let args = base_args(Some("not-an-address".into()), None);
    let err = run(&ctx_for(&api_server, Some("test-key")), args)
        .await
        .expect_err("bad address must fail locally");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn post_direct_without_agent_id_is_usage_error() {
    let api_server = MockServer::start().await;
    let mut args = base_args(
        Some("0x0000000000000000000000000000000000000001".into()),
        None,
    );
    args.assignment_type = AssignmentType::Direct;
    args.direct_agent_id = None;
    let err = run(&ctx_for(&api_server, Some("test-key")), args)
        .await
        .expect_err("direct without id must fail");
    match err {
        CmdError::Usage(msg) => assert!(msg.contains("--direct-agent-id"), "unexpected: {msg}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn post_prepare_401_surfaces_as_auth_error() {
    let api_server = MockServer::start().await;
    let _ = mount_network_config_mock(&api_server).await;
    Mock::given(method("POST"))
        .and(path("/api/task_drafts"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&api_server)
        .await;

    let args = base_args(
        Some("0x0000000000000000000000000000000000000001".into()),
        Some("/tmp/does-not-matter".into()),
    );
    let err = run(&ctx_for(&api_server, Some("test-key")), args)
        .await
        .expect_err("401 must surface as Auth");
    match err {
        CmdError::Auth(_) => {}
        other => panic!("expected Auth, got {other:?}"),
    }
}

#[tokio::test]
async fn post_keystore_address_mismatch_is_usage_error() {
    let api_server = MockServer::start().await;
    let _ = mount_network_config_mock(&api_server).await;

    // Server returns a happy prepare; our signer won't match --wallet-address.
    let draft_id = uuid::Uuid::new_v4();
    let calldata_hex = format!("0x{}", "00".repeat(4 + 64));
    Mock::given(method("POST"))
        .and(path("/api/task_drafts"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "draft_id": draft_id,
            "payload_to_sign": calldata_hex,
            "token_address": "0x20c0000000000000000000000000000000000000",
        })))
        .mount(&api_server)
        .await;

    let signer = PrivateKeySigner::random();
    let tmp = tempfile::tempdir().expect("tempdir");
    let keystore_path = tmp.path().join("wallet.json");
    taskfast_agent::keystore::save_signer(&signer, &keystore_path, "pw").expect("keystore");
    let password_path = tmp.path().join("pw");
    std::fs::write(&password_path, b"pw").unwrap();

    // Deliberate mismatch: wallet_address != signer.address().
    let mut args = base_args(
        Some("0x0000000000000000000000000000000000000001".into()),
        Some(keystore_path.display().to_string()),
    );
    args.wallet_password_file = Some(password_path);

    let err = run(&ctx_for(&api_server, Some("test-key")), args)
        .await
        .expect_err("address mismatch must fail");
    match err {
        CmdError::Usage(msg) => {
            assert!(msg.contains("does not match"), "unexpected message: {msg}")
        }
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn post_rejects_non_allowlisted_fee_token() {
    // F1 regression: a compromised server (or MITM on a stolen PAT) returns
    // an attacker-controlled `token_address`. The CLI must refuse to sign
    // instead of broadcasting an ERC-20 transfer to whatever contract the
    // server named.
    let api_server = MockServer::start().await;
    let rpc_server = MockServer::start().await;
    let _ = mount_network_config_mock(&api_server).await;

    let draft_id = uuid::Uuid::new_v4();
    let calldata_hex = format!("0x{}", "00".repeat(4 + 64));
    let attacker_token = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    Mock::given(method("POST"))
        .and(path("/api/task_drafts"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "draft_id": draft_id,
            "payload_to_sign": calldata_hex,
            "token_address": attacker_token,
        })))
        .mount(&api_server)
        .await;

    // RPC returns the real testnet chain id → allowlist is active.
    mount_rpc_mocks(&rpc_server, &format!("0x{}", "aa".repeat(32))).await;

    let signer = PrivateKeySigner::random();
    let tmp = tempfile::tempdir().expect("tempdir");
    let keystore_path = tmp.path().join("wallet.json");
    taskfast_agent::keystore::save_signer(&signer, &keystore_path, "pw").expect("keystore");
    let password_path = tmp.path().join("pw");
    std::fs::write(&password_path, b"pw").unwrap();

    let wallet_addr = format!("{:#x}", signer.address());
    let mut args = base_args(Some(wallet_addr), Some(keystore_path.display().to_string()));
    args.wallet_password_file = Some(password_path);
    args.rpc_url = Some(rpc_server.uri());

    // Explicitly turn off the opt-in so the allowlist's chain-id guard is
    // active even though the api_base is a mock URL.
    let mut ctx = ctx_for(&api_server, Some("test-key"));
    ctx.allow_custom_endpoints = true; // needed for mock api/rpc URLs
    let err = run(&ctx, args)
        .await
        .expect_err("attacker token must be refused");
    match err {
        CmdError::Validation { code, message } => {
            assert_eq!(code, "fee_token_not_allowed");
            assert!(message.contains(attacker_token), "msg: {message}");
            assert!(message.contains("allowlist"), "msg: {message}");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn post_missing_api_key_errors_before_any_http() {
    let api_server = MockServer::start().await;
    let args = base_args(
        Some("0x0000000000000000000000000000000000000001".into()),
        Some("/tmp/unused".into()),
    );
    let err = run(&ctx_for(&api_server, None), args)
        .await
        .expect_err("no key → MissingApiKey");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
}

/// Regression guard for am-en56: `--criterion` payloads must land on the wire
/// in the `POST /task_drafts` body. Empty criteria previously reached the
/// server silently and disarmed worker-payout evaluation.
#[tokio::test]
async fn post_forwards_completion_criteria() {
    let api_server = MockServer::start().await;
    let rpc_server = MockServer::start().await;
    let _ = mount_network_config_mock(&api_server).await;

    let signer = PrivateKeySigner::random();
    let wallet_addr = format!("{:#x}", signer.address());
    let tmp = tempfile::tempdir().expect("tempdir");
    let keystore_path = tmp.path().join("wallet.json");
    taskfast_agent::keystore::save_signer(&signer, &keystore_path, "pw").expect("keystore");
    let password_path = tmp.path().join("pw");
    std::fs::write(&password_path, b"pw").unwrap();

    let draft_id = uuid::Uuid::new_v4();
    let task_id = uuid::Uuid::new_v4();
    let calldata_hex = {
        let mut buf = vec![0xa9u8, 0x05, 0x9c, 0xbb];
        buf.extend([0u8; 32]);
        buf.extend([0u8; 32]);
        format!("0x{}", hex::encode(buf))
    };
    let token_addr = "0x20c0000000000000000000000000000000000000";
    let tx_hash_hex = format!("0x{}", "aa".repeat(32));

    // body_partial_json matches on a subset — this asserts the field is
    // present with the expected check_type without over-constraining order.
    Mock::given(method("POST"))
        .and(path("/api/task_drafts"))
        .and(body_partial_json(json!({
            "completion_criteria": [{"check_type": "regex"}]
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "draft_id": draft_id,
            "payload_to_sign": calldata_hex,
            "token_address": token_addr,
        })))
        .mount(&api_server)
        .await;

    Mock::given(method("POST"))
        .and(path(format!("/api/task_drafts/{draft_id}/submit")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": task_id,
            "status": "open",
            "submission_fee_status": "pending_confirmation",
            "submission_fee_tx_hash": tx_hash_hex,
        })))
        .mount(&api_server)
        .await;

    mount_rpc_mocks(&rpc_server, &tx_hash_hex).await;

    let mut args = base_args(
        Some(wallet_addr.clone()),
        Some(keystore_path.display().to_string()),
    );
    args.wallet_password_file = Some(password_path);
    args.rpc_url = Some(rpc_server.uri());
    args.criteria = vec![serde_json::to_string(&json!({
        "description": "matches anything",
        "check_type": "regex",
        "check_expression": ".*",
        "expected_value": ".",
    }))
    .unwrap()];

    let envelope = run(&ctx_for(&api_server, Some("test-key")), args)
        .await
        .expect("post with criteria should succeed");
    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["task_id"], task_id.to_string());
}

#[tokio::test]
async fn post_bad_criterion_json_is_usage_error() {
    let api_server = MockServer::start().await;
    let mut args = base_args(
        Some("0x0000000000000000000000000000000000000001".into()),
        Some("/tmp/unused".into()),
    );
    args.criteria = vec!["not-json".into()];
    let err = run(&ctx_for(&api_server, Some("test-key")), args)
        .await
        .expect_err("malformed --criterion must fail locally");
    match err {
        CmdError::Usage(msg) => {
            assert!(msg.contains("--criterion"), "unexpected message: {msg}")
        }
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn post_criteria_file_merges_with_inline() {
    let api_server = MockServer::start().await;
    let rpc_server = MockServer::start().await;

    let signer = PrivateKeySigner::random();
    let wallet_addr = format!("{:#x}", signer.address());
    let tmp = tempfile::tempdir().expect("tempdir");
    let keystore_path = tmp.path().join("wallet.json");
    taskfast_agent::keystore::save_signer(&signer, &keystore_path, "pw").expect("keystore");
    let password_path = tmp.path().join("pw");
    std::fs::write(&password_path, b"pw").unwrap();

    let criteria_path = tmp.path().join("criteria.json");
    std::fs::write(
        &criteria_path,
        serde_json::to_vec(&json!([{
            "description": "from file",
            "check_type": "file_exists",
            "check_expression": "out.csv",
            "expected_value": "true",
        }]))
        .unwrap(),
    )
    .unwrap();

    let draft_id = uuid::Uuid::new_v4();
    let task_id = uuid::Uuid::new_v4();
    let calldata_hex = {
        let mut buf = vec![0xa9u8, 0x05, 0x9c, 0xbb];
        buf.extend([0u8; 32]);
        buf.extend([0u8; 32]);
        format!("0x{}", hex::encode(buf))
    };
    let tx_hash_hex = format!("0x{}", "aa".repeat(32));

    // File entry goes first, inline second — assert both land in order.
    Mock::given(method("POST"))
        .and(path("/api/task_drafts"))
        .and(body_partial_json(json!({
            "completion_criteria": [
                {"check_type": "file_exists", "description": "from file"},
                {"check_type": "regex", "description": "inline"},
            ]
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "draft_id": draft_id,
            "payload_to_sign": calldata_hex,
            "token_address": "0x20c0000000000000000000000000000000000000",
        })))
        .mount(&api_server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/task_drafts/{draft_id}/submit")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": task_id,
            "status": "open",
            "submission_fee_status": "pending_confirmation",
            "submission_fee_tx_hash": tx_hash_hex,
        })))
        .mount(&api_server)
        .await;
    mount_rpc_mocks(&rpc_server, &tx_hash_hex).await;

    let mut args = base_args(Some(wallet_addr), Some(keystore_path.display().to_string()));
    args.wallet_password_file = Some(password_path);
    args.rpc_url = Some(rpc_server.uri());
    args.criteria_file = Some(criteria_path);
    args.criteria = vec![serde_json::to_string(&json!({
        "description": "inline",
        "check_type": "regex",
        "check_expression": ".*",
        "expected_value": ".",
    }))
    .unwrap()];

    run(&ctx_for(&api_server, Some("test-key")), args)
        .await
        .expect("merged criteria should succeed");
}
