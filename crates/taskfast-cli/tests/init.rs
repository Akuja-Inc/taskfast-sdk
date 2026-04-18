//! End-to-end tests for `taskfast init`.
//!
//! Covers the full command pipeline (api-key resolution, validate,
//! readiness, wallet provisioning, config persistence, final readiness)
//! against a wiremock server.

use std::fs;
use std::path::PathBuf;

use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::init::{run, Args, Network};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::config::Config;
use taskfast_cli::{Envelope, Environment};

const BYOW_ADDRESS: &str = "0xdEaDbEeF00000000000000000000000000000001";

fn ctx_for(server: &MockServer, key: Option<&str>, config_path: PathBuf, dry_run: bool) -> Ctx {
    Ctx {
        api_key: key.map(String::from),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path,
        dry_run,
        quiet: true,
        ..Default::default()
    }
}

fn base_args() -> Args {
    Args {
        wallet_address: None,
        generate_wallet: false,
        wallet_password_file: None,
        keystore_path: None,
        network: Network::Testnet,
        skip_wallet: false,
        fund: false,
        human_api_key: None,
        agent_name: "taskfast-agent".into(),
        agent_description: "Headless agent registered via taskfast init".into(),
        agent_capabilities: Vec::new(),
        webhook_url: None,
        webhook_secret_file: None,
        webhook_events: Vec::new(),
        no_default_events: false,
        no_interactive: true,
        inline_wallet_password: None,
    }
}

fn config_path_in(dir: &std::path::Path) -> PathBuf {
    dir.join(".taskfast").join("config.json")
}

fn envelope_value(env: &Envelope) -> serde_json::Value {
    serde_json::to_value(env).expect("serialize envelope")
}

async fn mount_profile_active(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "00000000-0000-0000-0000-000000000042",
            "name": "alice",
            "status": "active",
            "capabilities": ["coding"],
        })))
        .mount(server)
        .await;
}

async fn mount_readiness(server: &MockServer, wallet_status: &str, ready_to_work: bool) {
    Mock::given(method("GET"))
        .and(path("/api/agents/me/readiness"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ready_to_work": ready_to_work,
            "checks": {
                "api_key": {"status": "complete"},
                "wallet": {"status": wallet_status},
                "webhook": {"status": "not_configured", "required": false},
            },
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn byow_happy_path_registers_wallet_and_writes_env_file() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "missing", false).await;

    Mock::given(method("POST"))
        .and(path("/api/agents/me/wallet"))
        .and(body_partial_json(json!({
            "tempo_wallet_address": BYOW_ADDRESS,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tempo_wallet_address": BYOW_ADDRESS,
            "payout_method": "tempo_wallet",
            "payment_method": "tempo",
            "ready_to_work": true,
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let mut args = base_args();
    args.wallet_address = Some(BYOW_ADDRESS.to_string());

    let envelope = run(
        &ctx_for(&server, Some("test-key"), cfg_path.clone(), false),
        args,
    )
    .await
    .expect("init should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["wallet"]["status"], "byo_registered");
    assert_eq!(v["data"]["wallet"]["address"], BYOW_ADDRESS);
    assert_eq!(v["data"]["config_file"]["written"], true);

    // Config exists and carries the registered address + api key.
    let loaded = Config::load(&cfg_path).unwrap();
    assert_eq!(loaded.api_key.as_deref(), Some("test-key"));
    assert_eq!(loaded.wallet_address.as_deref(), Some(BYOW_ADDRESS));
    assert_eq!(loaded.network.as_deref(), Some("testnet"));
    assert_eq!(loaded.api_base.as_deref(), Some(server.uri().as_str()));
}

#[tokio::test]
async fn skips_wallet_when_server_already_has_one() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", true).await;

    let tmp = TempDir::new().unwrap();
    let args = base_args();

    let envelope = run(
        &ctx_for(&server, Some("test-key"), config_path_in(tmp.path()), false),
        args,
    )
    .await
    .expect("init should succeed without wallet flags");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["wallet"]["status"], "already_configured");
    assert_eq!(v["data"]["ready_to_work"], true);
}

#[tokio::test]
async fn skip_wallet_flag_bypasses_provisioning_even_when_missing() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "missing", false).await;

    let tmp = TempDir::new().unwrap();
    let mut args = base_args();
    args.skip_wallet = true;

    let envelope = run(
        &ctx_for(&server, Some("test-key"), config_path_in(tmp.path()), false),
        args,
    )
    .await
    .expect("--skip-wallet should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["wallet"]["status"], "skipped");
    // Final readiness reflects server state, not caller intent.
    assert_eq!(v["data"]["ready_to_work"], false);
}

#[tokio::test]
async fn dry_run_byow_skips_registration_and_env_write() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "missing", false).await;
    // Deliberately no /agents/me/wallet mock — a hit would 404 and fail the test.

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let mut args = base_args();
    args.wallet_address = Some(BYOW_ADDRESS.to_string());

    let envelope = run(
        &ctx_for(&server, Some("test-key"), cfg_path.clone(), true),
        args,
    )
    .await
    .expect("dry-run should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["wallet"]["status"], "skipped");
    assert_eq!(v["data"]["config_file"]["written"], false);
    assert_eq!(v["data"]["config_file"]["would_write"], true);
    assert!(!cfg_path.exists(), "dry-run must not write config file");
}

#[tokio::test]
async fn inactive_agent_status_surfaces_validation_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "alice",
            "status": "suspended",
            "capabilities": [],
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let args = base_args();

    let err = run(
        &ctx_for(&server, Some("test-key"), config_path_in(tmp.path()), false),
        args,
    )
    .await
    .expect_err("suspended → Validation");

    match err {
        CmdError::Validation { code, .. } => assert_eq!(code, "agent_not_active"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn api_key_falls_back_to_config_when_ctx_is_empty() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", true).await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let seed = Config {
        api_key: Some("from-config".into()),
        ..Config::default()
    };
    seed.save(&cfg_path).unwrap();

    let args = base_args();
    let envelope = run(&ctx_for(&server, None, cfg_path.clone(), false), args)
        .await
        .expect("should pick up api_key from config");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);

    // Re-saved config still carries that key.
    let loaded = Config::load(&cfg_path).unwrap();
    assert_eq!(loaded.api_key.as_deref(), Some("from-config"));
}

#[tokio::test]
async fn missing_api_key_everywhere_errors_cleanly() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let args = base_args();

    let err = run(
        &ctx_for(&server, None, config_path_in(tmp.path()), false),
        args,
    )
    .await
    .expect_err("no key anywhere → MissingApiKey");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
}

#[tokio::test]
async fn generate_wallet_without_password_errors_as_usage() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "missing", false).await;

    let tmp = TempDir::new().unwrap();
    let mut args = base_args();
    args.generate_wallet = true;
    // No wallet_password_file; no TASKFAST_WALLET_PASSWORD env var.
    // (Tests run with whatever env the harness has; the env var is unset
    // in CI. If a developer has it set locally, clear it or skip.)
    let _ = std::env::var("TASKFAST_WALLET_PASSWORD").map(|_| {
        eprintln!("note: TASKFAST_WALLET_PASSWORD is set in the harness env; skipping");
    });
    if std::env::var("TASKFAST_WALLET_PASSWORD").is_ok() {
        return;
    }

    let err = run(
        &ctx_for(&server, Some("test-key"), config_path_in(tmp.path()), false),
        args,
    )
    .await
    .expect_err("missing password → Usage");
    match err {
        CmdError::Usage(msg) => {
            assert!(
                msg.contains("--wallet-password-file"),
                "message should mention the flag: {msg}"
            );
        }
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn generate_wallet_with_password_file_persists_keystore_and_registers() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "missing", false).await;

    // Accept any POST /agents/me/wallet (address is dynamic — freshly
    // generated signer) and echo it back.
    Mock::given(method("POST"))
        .and(path("/api/agents/me/wallet"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tempo_wallet_address": "0x0000000000000000000000000000000000000000",
            "payout_method": "tempo_wallet",
            "payment_method": "tempo",
            "ready_to_work": true,
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let pw_path = tmp.path().join("pw");
    fs::write(&pw_path, "s3kret\n").unwrap();
    let keystore_path = tmp.path().join("wallet.json");

    let mut args = base_args();
    args.generate_wallet = true;
    args.wallet_password_file = Some(pw_path);
    args.keystore_path = Some(keystore_path.clone());

    let envelope = run(
        &ctx_for(&server, Some("test-key"), cfg_path.clone(), false),
        args,
    )
    .await
    .expect("generate path should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["wallet"]["status"], "generated");
    let addr = v["data"]["wallet"]["address"]
        .as_str()
        .expect("address string");
    assert!(addr.starts_with("0x") && addr.len() == 42, "addr: {addr}");
    assert_eq!(
        v["data"]["wallet"]["keystore_path"],
        keystore_path.display().to_string()
    );

    assert!(keystore_path.exists(), "keystore must be written");

    let loaded = Config::load(&cfg_path).unwrap();
    assert_eq!(
        loaded.keystore_path.as_deref(),
        Some(keystore_path.as_path())
    );
    assert_eq!(
        loaded.wallet_address.as_deref().map(str::to_lowercase),
        Some(addr.to_lowercase())
    );
}

// ---- am-z58: --human-api-key headless mint path ----

const MINTED_AGENT_ID: &str = "11111111-1111-1111-1111-111111111111";
const MINTED_KEY: &str = "am_live_minted_0123456789abcdef";

#[tokio::test]
async fn human_api_key_mints_agent_then_continues_with_returned_key() {
    let server = MockServer::start().await;

    // POST /agents — PAT-authed mint. Body carries the required fields;
    // owner_id is derived server-side from the PAT, so the CLI omits it.
    Mock::given(method("POST"))
        .and(path("/api/agents"))
        .and(body_partial_json(json!({
            "name": "taskfast-agent",
            "capabilities": ["general"],
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": MINTED_AGENT_ID,
            "api_key": MINTED_KEY,
            "name": "taskfast-agent",
            "status": "active",
        })))
        .mount(&server)
        .await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", true).await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let mut args = base_args();
    args.human_api_key = Some("tf_user_test".into());

    // Ctx carries no agent key — forces the mint branch.
    let envelope = run(&ctx_for(&server, None, cfg_path.clone(), false), args)
        .await
        .expect("mint path should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["agent"]["action"], "minted");
    assert_eq!(v["data"]["agent"]["id"], MINTED_AGENT_ID);

    // Config carries the minted agent key, not the PAT.
    let loaded = Config::load(&cfg_path).unwrap();
    assert_eq!(loaded.api_key.as_deref(), Some(MINTED_KEY));
    assert_eq!(loaded.agent_id.as_deref(), Some(MINTED_AGENT_ID));
}

#[tokio::test]
async fn dry_run_human_api_key_reports_would_mint_and_skips_post() {
    let server = MockServer::start().await;
    // Any HTTP hit would 404 and the envelope would be an error, not ok.
    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let mut args = base_args();
    args.human_api_key = Some("tf_user_test".into());

    let envelope = run(&ctx_for(&server, None, cfg_path.clone(), true), args)
        .await
        .expect("dry-run mint should succeed without HTTP");

    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["agent"]["action"], "would_mint");
    assert!(
        v["data"]["agent"].get("owner_id").is_none(),
        "dry-run envelope should not carry owner_id — it is server-derived"
    );
    assert_eq!(v["data"]["config_file"]["written"], false);
    assert_eq!(v["data"]["config_file"]["would_write"], true);
    assert!(!cfg_path.exists(), "dry-run must not write config file");
}

#[tokio::test]
async fn existing_config_key_takes_precedence_over_human_api_key() {
    // Idempotency: re-running init with --human-api-key but a config-file
    // key must NOT mint a second agent.
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", true).await;
    // Deliberately no POST /agents mock — a hit would 404.

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let seed = Config {
        api_key: Some("am_live_already_have_one".into()),
        ..Config::default()
    };
    seed.save(&cfg_path).unwrap();

    let mut args = base_args();
    args.human_api_key = Some("tf_user_should_be_ignored".into());

    let envelope = run(&ctx_for(&server, None, cfg_path.clone(), false), args)
        .await
        .expect("idempotent path");
    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    // agent field is only set on mint — absence means we didn't mint.
    assert!(v["data"].get("agent").is_none(), "should not have minted");
}
