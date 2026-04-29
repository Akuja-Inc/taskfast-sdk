//! End-to-end tests for `taskfast settle`.
//!
//! Wires three mocked server endpoints together (`GET /tasks/{id}`,
//! `GET /agents/me/readiness`, `POST /tasks/{id}/settle`) and drives
//! `cmd::settle::run` directly so we exercise the same code path `main`
//! dispatches in production — including real `sign_distribution` output
//! (verified against the Elixir fixture in
//! `crates/taskfast-agent/src/signing.rs`).

use std::path::PathBuf;
use std::str::FromStr;

use alloy_primitives::{Address, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_chains::tempo::{verify_distribution, DistributionDomain};
use taskfast_cli::cmd::settle::{run, Args};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const TASK_ID: &str = "00000000-0000-0000-0000-0000000000aa";
const ESCROW_ID: &str = "0xabababababababababababababababababababababababababababababababab";
const VERIFYING_CONTRACT: &str = "0x0000000000000000000000000000000000000001";
const CHAIN_ID: u64 = 42_431;

fn ctx_for(server: &MockServer, key: Option<&str>) -> Ctx {
    Ctx {
        api_key: key.map(String::from),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path: std::path::PathBuf::from("/dev/null"),
        dry_run: false,
        quiet: true,
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

/// Create a fresh random signer and persist it as a JSON v3 keystore so the
/// `settle` command's real `keystore::load` path gets exercised. The tempdir
/// is owned by the returned struct and cleaned up when it drops.
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

fn base_args(keys: &Keys) -> Args {
    Args {
        task_id: TASK_ID.into(),
        deadline_unix: None,
        keystore: Some(keys.keystore_path.display().to_string()),
        wallet_password_file: Some(keys.password_path.clone()),
        wallet_address: None,
        yes: false,
    }
}

/// Canonical task-detail response with escrow + deadline populated. Tests
/// that need a null field clone + mutate.
fn task_detail_json() -> Value {
    json!({
        "id": TASK_ID,
        "status": "complete",
        "escrow_id": ESCROW_ID,
        "settlement_deadline": "2027-01-15T08:00:00Z",
    })
}

fn readiness_json() -> Value {
    json!({
        "ready_to_work": true,
        "checks": {
            "api_key": { "status": "complete" },
            "wallet": { "status": "complete" },
            "webhook": { "status": "complete" },
        },
        "settlement_domain": {
            "chain_id": CHAIN_ID as i64,
            "verifying_contract": VERIFYING_CONTRACT,
        },
    })
}

async fn mount_task_get(server: &MockServer, body: Value) {
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
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

#[tokio::test]
async fn settle_happy_path_returns_settled_status() {
    let server = MockServer::start().await;
    let keys = fresh_keys();

    mount_task_get(&server, task_detail_json()).await;
    mount_readiness(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/settle")))
        // Signature shape is validated by the generated newtype regex; we
        // assert the key is present rather than pinning a specific value
        // (it depends on the random signer).
        .and(body_partial_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": TASK_ID,
            "status": "settled",
        })))
        .mount(&server)
        .await;

    let envelope = run(&ctx_for(&server, Some("test-key")), base_args(&keys))
        .await
        .expect("settle should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["status"], "settled");
}

#[tokio::test]
async fn settle_missing_escrow_id_is_usage_error_before_post() {
    let server = MockServer::start().await;
    let keys = fresh_keys();

    let mut task = task_detail_json();
    task["escrow_id"] = Value::Null;
    mount_task_get(&server, task).await;
    // Readiness + settle deliberately not mounted — any hit would 404 via
    // wiremock and the test would fail with a Network error rather than the
    // Usage we expect. That's the assertion.

    let err = run(&ctx_for(&server, Some("test-key")), base_args(&keys))
        .await
        .expect_err("missing escrow_id must fail locally");

    match err {
        CmdError::Usage(msg) => assert!(
            msg.contains("escrow_id"),
            "expected escrow_id usage message, got: {msg}"
        ),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn settle_403_not_poster_maps_to_auth() {
    let server = MockServer::start().await;
    let keys = fresh_keys();

    mount_task_get(&server, task_detail_json()).await;
    mount_readiness(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/settle")))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": "forbidden",
            "detail": "caller is not the task poster",
        })))
        .mount(&server)
        .await;

    let err = run(&ctx_for(&server, Some("test-key")), base_args(&keys))
        .await
        .expect_err("403 must propagate");

    assert!(matches!(err, CmdError::Auth(_)), "got: {err:?}");
}

#[tokio::test]
async fn settle_409_already_settled_maps_to_validation() {
    // Per `map_api_error`, 409 routes to Validation across the CLI — matches
    // the existing approve/dispute/cancel contract (tests in task.rs).
    let server = MockServer::start().await;
    let keys = fresh_keys();

    mount_task_get(&server, task_detail_json()).await;
    mount_readiness(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/settle")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "ineligible",
            "detail": "task already settled",
        })))
        .mount(&server)
        .await;

    let err = run(&ctx_for(&server, Some("test-key")), base_args(&keys))
        .await
        .expect_err("409 must propagate");

    assert!(matches!(err, CmdError::Validation { .. }), "got: {err:?}");
}

#[tokio::test]
async fn settle_422_signer_mismatch_maps_to_validation() {
    let server = MockServer::start().await;
    let keys = fresh_keys();

    mount_task_get(&server, task_detail_json()).await;
    mount_readiness(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/settle")))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "code": "signer_mismatch",
            "error": "recovered signer does not match agent wallet",
        })))
        .mount(&server)
        .await;

    let err = run(&ctx_for(&server, Some("test-key")), base_args(&keys))
        .await
        .expect_err("422 must propagate");

    assert!(matches!(err, CmdError::Validation { .. }), "got: {err:?}");
}

#[tokio::test]
async fn settle_bad_uuid_is_usage_error_without_any_http() {
    // No mounts — if the CLI hits the server for a UUID it couldn't parse,
    // wiremock returns 404 and the test fails loud.
    let server = MockServer::start().await;
    let keys = fresh_keys();

    let mut args = base_args(&keys);
    args.task_id = "not-a-uuid".into();

    let err = run(&ctx_for(&server, Some("test-key")), args)
        .await
        .expect_err("bad uuid must fail locally");

    assert!(matches!(err, CmdError::Usage(_)), "got: {err:?}");
}

#[tokio::test]
async fn settle_dry_run_skips_post_but_still_signs() {
    let server = MockServer::start().await;
    let keys = fresh_keys();

    mount_task_get(&server, task_detail_json()).await;
    mount_readiness(&server).await;
    // Intentionally NO `/settle` POST mount — any hit fails the test.

    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;

    let envelope = run(&ctx, base_args(&keys))
        .await
        .expect("dry-run should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_settle");
    assert_eq!(v["data"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["escrow_id"], ESCROW_ID);
    assert_eq!(v["data"]["domain"]["chain_id"], CHAIN_ID as i64);
    assert_eq!(
        v["data"]["domain"]["verifying_contract"],
        VERIFYING_CONTRACT
    );

    let sig = v["data"]["signature"].as_str().expect("signature string");
    assert_eq!(sig.len(), 132, "r||s||v hex is 0x + 65 bytes");
    assert!(sig.starts_with("0x"));

    // Cross-check the signature recovers to our signer against the same
    // domain values the envelope advertises. This is the strongest possible
    // dry-run assertion: if the CLI signed something other than what it
    // claimed, verification would fail.
    let deadline_signed = v["data"]["deadline"].as_u64().expect("deadline u64");
    let domain = DistributionDomain::new(CHAIN_ID, Address::from_str(VERIFYING_CONTRACT).unwrap());
    let escrow_id = B256::from_str(ESCROW_ID).unwrap();
    let ok = verify_distribution(
        sig,
        &domain,
        escrow_id,
        U256::from(deadline_signed),
        keys.address,
    )
    .expect("verify");
    assert!(ok, "dry-run signature must recover to keystore address");
}

#[tokio::test]
async fn settle_missing_keystore_is_usage_error() {
    let server = MockServer::start().await;
    let keys = fresh_keys();

    mount_task_get(&server, task_detail_json()).await;
    mount_readiness(&server).await;

    // Strip the keystore so the signer load fails. The settle command is
    // supposed to require it even in dry-run (bead acceptance spec).
    // `TEMPO_KEY_SOURCE` is only read via clap's `#[arg(env = ...)]`, which
    // never fires when Args is built manually — no env mutation needed.
    let mut args = base_args(&keys);
    args.keystore = None;

    let err = run(&ctx_for(&server, Some("test-key")), args)
        .await
        .expect_err("settle without keystore must fail");

    match err {
        CmdError::Usage(msg) => assert!(
            msg.contains("keystore") || msg.contains("TEMPO_KEY_SOURCE"),
            "expected keystore-usage message, got: {msg}"
        ),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn settle_deadline_override_binds_into_signature() {
    // Prove `--deadline-unix` actually lands in the signed digest rather than
    // being ignored in favor of the task's `settlement_deadline`. We sign in
    // dry-run (no HTTP needed) and verify against the override value.
    let server = MockServer::start().await;
    let keys = fresh_keys();

    mount_task_get(&server, task_detail_json()).await;
    mount_readiness(&server).await;

    let override_deadline: u64 = 1_234_567_890; // distinct from task.settlement_deadline
    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;
    let mut args = base_args(&keys);
    args.deadline_unix = Some(override_deadline);

    let envelope = run(&ctx, args).await.expect("dry-run override succeeds");
    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["deadline"].as_u64(), Some(override_deadline));

    let sig = v["data"]["signature"].as_str().unwrap();
    let domain = DistributionDomain::new(CHAIN_ID, Address::from_str(VERIFYING_CONTRACT).unwrap());
    let escrow_id = B256::from_str(ESCROW_ID).unwrap();

    // Verification with the override deadline succeeds...
    let ok = verify_distribution(
        sig,
        &domain,
        escrow_id,
        U256::from(override_deadline),
        keys.address,
    )
    .expect("verify override");
    assert!(ok, "override deadline must be the one signed");

    // ...and verification against the task's deadline (unix of
    // 2027-01-15T08:00:00Z = 1_799_467_200) fails, proving the override
    // really took effect.
    let task_deadline: u64 = 1_799_467_200;
    let mismatch = verify_distribution(
        sig,
        &domain,
        escrow_id,
        U256::from(task_deadline),
        keys.address,
    )
    .expect("verify task deadline");
    assert!(
        !mismatch,
        "signature must NOT verify under the task's settlement_deadline \
         when an override was passed"
    );
}

#[tokio::test]
async fn settle_wallet_address_mismatch_is_usage_error() {
    // Preflight parity with `taskfast post`: if the caller pins
    // --wallet-address but the keystore decrypts to a different key, fail
    // locally rather than eating the round-trip and a server 422.
    let server = MockServer::start().await;
    let keys = fresh_keys();

    mount_task_get(&server, task_detail_json()).await;
    mount_readiness(&server).await;

    let mut args = base_args(&keys);
    args.wallet_address = Some("0x0000000000000000000000000000000000000002".into());

    let err = run(&ctx_for(&server, Some("test-key")), args)
        .await
        .expect_err("mismatched wallet-address must fail locally");

    match err {
        CmdError::Usage(msg) => assert!(
            msg.contains("does not match"),
            "expected mismatch message, got: {msg}"
        ),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn settle_missing_settlement_domain_decodes_as_error() {
    // settlement_domain is now a required field on AgentReadiness — a server
    // omitting it is a contract violation, surfaced as a Decode error rather
    // than a friendly Usage hint. Verifies the decode-error path so a future
    // server regression that drops the field doesn't silently sign with a
    // zero-initialized domain.
    let server = MockServer::start().await;
    let keys = fresh_keys();

    mount_task_get(&server, task_detail_json()).await;
    Mock::given(method("GET"))
        .and(path("/agents/me/readiness"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ready_to_work": true,
            "checks": {
                "api_key": { "status": "complete" },
                "wallet": { "status": "complete" },
                "webhook": { "status": "complete" },
            },
            // settlement_domain deliberately omitted — server contract violation
        })))
        .mount(&server)
        .await;

    let err = run(&ctx_for(&server, Some("test-key")), base_args(&keys))
        .await
        .expect_err("missing settlement_domain must fail locally");

    match err {
        CmdError::Decode(msg) => assert!(
            msg.contains("settlement_domain"),
            "expected settlement_domain in decode message, got: {msg}"
        ),
        other => panic!("expected Decode, got {other:?}"),
    }
}
