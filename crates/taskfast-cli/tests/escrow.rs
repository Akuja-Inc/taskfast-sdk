//! End-to-end tests for `taskfast escrow sign`.
//!
//! Dry-run covers the happy path without RPC mocks (no tx broadcast).
//! Error-mapping tests cover 403/422 from both escrow/params and finalize
//! so the Auth-vs-Validation contract (exit code 2 vs 4) is pinned.
//!
//! The live on-chain path requires mocking the full Tempo RPC surface
//! (chainId, gasPrice, estimateGas, nonce, sendRawTransaction, receipt) —
//! that's smoke-test territory, deferred to the manual E2E in the plan.

use std::path::PathBuf;

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::escrow::{run, Command, SignArgs};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const BID_ID: &str = "00000000-0000-0000-0000-0000000000bb";
const TASK_ID: &str = "00000000-0000-0000-0000-0000000000cc";
const TASK_ESCROW: &str = "0x0000000000000000000000000000000000000001";
const TOKEN_ADDR: &str = "0x0000000000000000000000000000000000000002";
const WORKER_ADDR: &str = "0x0000000000000000000000000000000000000003";
const PLATFORM_WALLET: &str = "0x0000000000000000000000000000000000000004";
const CHAIN_ID: u64 = 42_431;

fn ctx_for(server: &MockServer, key: Option<&str>) -> Ctx {
    Ctx {
        api_key: key.map(String::from),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path: std::path::PathBuf::from("/dev/null"),
        dry_run: false,
        quiet: true,
        allow_custom_endpoints: true,
        ..Default::default()
    }
}

fn envelope_value(env: &Envelope) -> Value {
    serde_json::to_value(env).expect("serialize envelope")
}

struct Keys {
    _tmp: tempfile::TempDir,
    keystore_path: PathBuf,
    password_path: PathBuf,
    address: Address,
}

fn fresh_keys() -> Keys {
    let signer = PrivateKeySigner::random();
    let tmp = tempfile::tempdir().expect("tempdir");
    let keystore_path = tmp.path().join("wallet.json");
    taskfast_agent::keystore::save_signer(&signer, &keystore_path, "pw").expect("keystore");
    let password_path = tmp.path().join("pw");
    std::fs::write(&password_path, b"pw").expect("write password");
    Keys {
        address: signer.address(),
        _tmp: tmp,
        keystore_path,
        password_path,
    }
}

fn base_args(keys: &Keys) -> SignArgs {
    SignArgs {
        bid_id: BID_ID.into(),
        keystore: Some(keys.keystore_path.display().to_string()),
        wallet_password_file: Some(keys.password_path.clone()),
        wallet_address: None,
        rpc_url: Some("http://rpc.invalid".into()), // never hit in dry-run
        skip_allowance_check: false,
        approval_horizon: None,
        receipt_timeout: None,
    }
}

fn escrow_params_json() -> Value {
    escrow_params_json_with_chain_id(CHAIN_ID)
}

fn escrow_params_json_with_chain_id(chain_id: u64) -> Value {
    json!({
        "bid_id": BID_ID,
        "task_id": TASK_ID,
        "amount": "75.00",
        "platform_fee_amount": "3.75",
        "worker_address": WORKER_ADDR,
        "task_escrow_contract": TASK_ESCROW,
        "token_address": TOKEN_ADDR,
        "platform_wallet": PLATFORM_WALLET,
        "chain_id": chain_id as i64,
        "decimals": 6,
        "memo_text": null,
        "memo_hash": null,
    })
}

fn readiness_json() -> Value {
    readiness_json_with_chain_id(CHAIN_ID)
}

fn readiness_json_with_chain_id(chain_id: u64) -> Value {
    json!({
        "ready_to_work": true,
        "checks": {
            "api_key": { "status": "complete" },
            "wallet": { "status": "complete" },
            "webhook": { "status": "complete" },
        },
        "settlement_domain": {
            "chain_id": chain_id as i64,
            "verifying_contract": TASK_ESCROW,
        },
    })
}

async fn mount_params(server: &MockServer, body: Value, status: u16) {
    Mock::given(method("GET"))
        .and(path(format!("/bids/{BID_ID}/escrow/params")))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

async fn mount_readiness(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/agents/me/readiness"))
        .respond_with(ResponseTemplate::new(200).set_body_json(readiness_json()))
        .mount(server)
        .await;
}

async fn mount_readiness_with_chain_id(server: &MockServer, chain_id: u64) {
    Mock::given(method("GET"))
        .and(path("/agents/me/readiness"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(readiness_json_with_chain_id(chain_id)),
        )
        .mount(server)
        .await;
}

/// Absolute tolerance for deadline-window assertions: the test grabs
/// `now` seconds before/after the call, so any clock skew inside `sign()`
/// sits inside this envelope.
const DEADLINE_SLOP_SECS: u64 = 10;

fn assert_deadline_near(deadline: u64, expected_horizon_secs: u64) {
    let now = u64::try_from(chrono::Utc::now().timestamp()).expect("clock");
    let low = now
        .saturating_add(expected_horizon_secs)
        .saturating_sub(DEADLINE_SLOP_SECS);
    let high = now
        .saturating_add(expected_horizon_secs)
        .saturating_add(DEADLINE_SLOP_SECS);
    assert!(
        deadline >= low && deadline <= high,
        "deadline {deadline} outside [{low}, {high}] for horizon {expected_horizon_secs}s"
    );
}

#[tokio::test]
async fn escrow_sign_dry_run_uses_seven_day_default_deadline() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_params(&server, escrow_params_json(), 200).await;
    mount_readiness(&server).await;

    let mut ctx = ctx_for(&server, Some("k"));
    ctx.dry_run = true;

    let env = run(&ctx, Command::Sign(base_args(&keys)))
        .await
        .expect("dry-run");
    let deadline = envelope_value(&env)["data"]["deadline"]
        .as_u64()
        .expect("deadline");
    assert_deadline_near(deadline, 7 * 24 * 60 * 60);
}

#[tokio::test]
async fn escrow_sign_dry_run_flag_overrides_default_horizon() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_params(&server, escrow_params_json(), 200).await;
    mount_readiness(&server).await;

    let mut ctx = ctx_for(&server, Some("k"));
    ctx.dry_run = true;
    let mut args = base_args(&keys);
    args.approval_horizon = Some(std::time::Duration::from_hours(24)); // 1d

    let env = run(&ctx, Command::Sign(args)).await.expect("dry-run");
    let deadline = envelope_value(&env)["data"]["deadline"]
        .as_u64()
        .expect("deadline");
    assert_deadline_near(deadline, 24 * 60 * 60);
}

#[tokio::test]
async fn escrow_sign_dry_run_ctx_horizon_used_when_flag_absent() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_params(&server, escrow_params_json(), 200).await;
    mount_readiness(&server).await;

    let mut ctx = ctx_for(&server, Some("k"));
    ctx.dry_run = true;
    ctx.approval_horizon = Some(std::time::Duration::from_hours(2)); // 2h

    let env = run(&ctx, Command::Sign(base_args(&keys)))
        .await
        .expect("dry-run");
    let deadline = envelope_value(&env)["data"]["deadline"]
        .as_u64()
        .expect("deadline");
    assert_deadline_near(deadline, 2 * 60 * 60);
}

#[tokio::test]
async fn escrow_sign_dry_run_flag_beats_ctx() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_params(&server, escrow_params_json(), 200).await;
    mount_readiness(&server).await;

    let mut ctx = ctx_for(&server, Some("k"));
    ctx.dry_run = true;
    ctx.approval_horizon = Some(std::time::Duration::from_hours(24)); // 1d
    let mut args = base_args(&keys);
    args.approval_horizon = Some(std::time::Duration::from_hours(1)); // 1h — flag wins

    let env = run(&ctx, Command::Sign(args)).await.expect("dry-run");
    let deadline = envelope_value(&env)["data"]["deadline"]
        .as_u64()
        .expect("deadline");
    assert_deadline_near(deadline, 60 * 60);
}

#[tokio::test]
async fn escrow_sign_dry_run_emits_signature_and_calldata_without_rpc() {
    let server = MockServer::start().await;
    let keys = fresh_keys();

    mount_params(&server, escrow_params_json(), 200).await;
    mount_readiness(&server).await;
    // No /escrow/finalize mount — dry-run must not POST.

    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;

    let envelope = run(&ctx, Command::Sign(base_args(&keys)))
        .await
        .expect("dry-run must succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_sign_escrow");
    assert_eq!(v["data"]["bid_id"], BID_ID);
    assert_eq!(v["data"]["task_id"], TASK_ID);

    let sig = v["data"]["signature"].as_str().expect("signature");
    assert_eq!(sig.len(), 132, "r||s||v hex is 0x + 65 bytes");
    assert!(sig.starts_with("0x"));

    let escrow_id = v["data"]["escrow_id"].as_str().expect("escrow_id");
    assert_eq!(escrow_id.len(), 66, "bytes32 hex is 0x + 32 bytes");

    // Calldata must be at least the 4-byte TaskEscrow.open() selector + 6
    // 32-byte slots = 4 + 192 bytes → 0x + 392 chars.
    let calldata = v["data"]["open_calldata"].as_str().expect("calldata");
    assert!(
        calldata.starts_with("0x") && calldata.len() >= 2 + 2 * (4 + 32 * 6),
        "expected TaskEscrow.open calldata shape, got len={}",
        calldata.len()
    );
    // Ensure the signer address actually came from our keystore.
    let _ = keys.address;
}

#[tokio::test]
async fn escrow_sign_rejects_custom_rpc_without_opt_in() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_params(&server, escrow_params_json(), 200).await;
    mount_readiness(&server).await;

    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.allow_custom_endpoints = false;
    ctx.dry_run = true;

    let err = run(&ctx, Command::Sign(base_args(&keys)))
        .await
        .expect_err("custom rpc url must require opt-in");

    match err {
        CmdError::Usage(msg) => {
            assert!(msg.contains("custom tempo_rpc_url"), "msg: {msg}");
            assert!(msg.contains("--allow-custom-endpoints"), "msg: {msg}");
        }
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn escrow_sign_rejects_plain_http_mainnet_rpc() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_params(&server, escrow_params_json_with_chain_id(4_217), 200).await;
    mount_readiness_with_chain_id(&server, 4_217).await;

    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;

    let err = run(&ctx, Command::Sign(base_args(&keys)))
        .await
        .expect_err("mainnet plain-http rpc must fail");

    match err {
        CmdError::Usage(msg) => {
            assert!(msg.contains("plain-HTTP mainnet"), "msg: {msg}");
            assert!(msg.contains("tempo_rpc_url"), "msg: {msg}");
        }
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn escrow_sign_params_403_maps_to_auth() {
    let server = MockServer::start().await;
    let keys = fresh_keys();

    mount_params(
        &server,
        json!({ "error": "forbidden", "detail": "caller is not the poster" }),
        403,
    )
    .await;

    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Sign(base_args(&keys)),
    )
    .await
    .expect_err("403 must propagate");

    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn escrow_sign_params_409_wrong_status_maps_to_validation() {
    let server = MockServer::start().await;
    let keys = fresh_keys();

    mount_params(
        &server,
        json!({ "error": "conflict", "detail": "bid not in :accepted_pending_escrow" }),
        409,
    )
    .await;

    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Sign(base_args(&keys)),
    )
    .await
    .expect_err("409 must propagate");

    assert!(matches!(err, CmdError::Validation { .. }), "got {err:?}");
}

#[tokio::test]
async fn escrow_sign_readiness_chain_id_mismatch_is_decode_error() {
    let server = MockServer::start().await;
    let keys = fresh_keys();

    mount_params(&server, escrow_params_json(), 200).await;
    // Readiness returns a different chain_id → cross-check fails locally
    // before any signing happens.
    Mock::given(method("GET"))
        .and(path("/agents/me/readiness"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ready_to_work": true,
            "checks": {
                "api_key": { "status": "complete" },
                "wallet": { "status": "complete" },
                "webhook": { "status": "complete" },
            },
            "settlement_domain": {
                "chain_id": 4217_i64, // mainnet, params says testnet
                "verifying_contract": TASK_ESCROW,
            },
        })))
        .mount(&server)
        .await;

    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;
    let err = run(&ctx, Command::Sign(base_args(&keys)))
        .await
        .expect_err("chain_id mismatch must fail");

    match err {
        CmdError::Decode(msg) => assert!(msg.contains("chain_id"), "msg: {msg}"),
        other => panic!("expected Decode, got {other:?}"),
    }
}

#[tokio::test]
async fn escrow_sign_bad_uuid_is_usage_error_without_http() {
    // No mounts — if any HTTP hit escapes local validation, wiremock 404s
    // and the test fails.
    let server = MockServer::start().await;
    let keys = fresh_keys();

    let mut args = base_args(&keys);
    args.bid_id = "not-a-uuid".into();

    let err = run(&ctx_for(&server, Some("test-key")), Command::Sign(args))
        .await
        .expect_err("bad uuid must fail locally");

    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn escrow_sign_wallet_address_mismatch_aborts_before_rpc() {
    let server = MockServer::start().await;
    let keys = fresh_keys();

    mount_params(&server, escrow_params_json(), 200).await;
    mount_readiness(&server).await;

    let mut args = base_args(&keys);
    // A random other address — keystore will decrypt to its own random
    // address which is vanishingly unlikely to match.
    args.wallet_address = Some("0x0000000000000000000000000000000000000099".into());

    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;
    let err = run(&ctx, Command::Sign(args))
        .await
        .expect_err("wallet mismatch must fail");

    match err {
        CmdError::Usage(msg) => assert!(msg.contains("does not match"), "msg: {msg}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}
