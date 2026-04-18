//! End-to-end tests for the interactive `taskfast init` branch.
//!
//! Uses a scripted [`Prompter`] so the PAT / wallet-mode / address /
//! password inputs arrive deterministically, and wiremock to stand in
//! for the platform (`GET /users/me`, `POST /agents`, `GET /agents/me`,
//! `GET /agents/me/readiness`, `POST /agents/me/wallet`).

use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::init::{run_with_prompter, Args, Network};
use taskfast_cli::cmd::init_tui::{Prompter, WalletMode};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::config::Config;
use taskfast_cli::{Envelope, Environment};

const BYOW_ADDRESS: &str = "0xdEaDbEeF00000000000000000000000000000001";
const MINTED_AGENT_ID: &str = "11111111-2222-3333-4444-555555555555";
const MINTED_KEY: &str = "am_live_minted_tui_abcdef";

/// Scripted prompter. Each method returns the queued value for that
/// question type; `None` in a slot means the test didn't expect that
/// prompt to fire and panics with a clear message when it does.
struct MockPrompter {
    pat: Mutex<Option<String>>,
    wallet_mode: Mutex<Option<WalletMode>>,
    wallet_address: Mutex<Option<String>>,
    wallet_password: Mutex<Option<String>>,
}

impl MockPrompter {
    fn new() -> Self {
        Self {
            pat: Mutex::new(None),
            wallet_mode: Mutex::new(None),
            wallet_address: Mutex::new(None),
            wallet_password: Mutex::new(None),
        }
    }

    fn with_pat(self, v: &str) -> Self {
        *self.pat.lock().unwrap() = Some(v.into());
        self
    }

    fn with_wallet_mode(self, m: WalletMode) -> Self {
        *self.wallet_mode.lock().unwrap() = Some(m);
        self
    }

    fn with_wallet_address(self, a: &str) -> Self {
        *self.wallet_address.lock().unwrap() = Some(a.into());
        self
    }

    fn with_wallet_password(self, p: &str) -> Self {
        *self.wallet_password.lock().unwrap() = Some(p.into());
        self
    }

    fn take(slot: &Mutex<Option<String>>, kind: &str) -> io::Result<String> {
        slot.lock()
            .unwrap()
            .take()
            .ok_or_else(|| io::Error::other(format!("unexpected {kind} prompt")))
    }
}

impl Prompter for MockPrompter {
    fn pat(&self, _accounts_url: &str) -> io::Result<String> {
        Self::take(&self.pat, "PAT")
    }

    fn wallet_mode(&self) -> io::Result<WalletMode> {
        self.wallet_mode
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| io::Error::other("unexpected wallet_mode prompt"))
    }

    fn wallet_address(&self) -> io::Result<String> {
        Self::take(&self.wallet_address, "wallet_address")
    }

    fn wallet_password(&self) -> io::Result<String> {
        Self::take(&self.wallet_password, "wallet_password")
    }
}

fn ctx_for(server: &MockServer, config_path: PathBuf) -> Ctx {
    Ctx {
        api_key: None,
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path,
        dry_run: false,
        quiet: true,
        ..Default::default()
    }
}

fn config_path_in(dir: &std::path::Path) -> PathBuf {
    dir.join(".taskfast").join("config.json")
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
        no_interactive: false,
        inline_wallet_password: None,
    }
}

fn envelope_value(envelope: &Envelope) -> serde_json::Value {
    serde_json::to_value(envelope).expect("serialize envelope")
}

async fn mount_users_me(server: &MockServer, name: &str, email: &str) {
    Mock::given(method("GET"))
        .and(path("/api/users/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": name,
            "email": email,
        })))
        .mount(server)
        .await;
}

async fn mount_users_me_404(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/users/me"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": "not_found",
            "message": "endpoint not deployed",
        })))
        .mount(server)
        .await;
}

async fn mount_post_agents(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/agents"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": MINTED_AGENT_ID,
            "api_key": MINTED_KEY,
            "name": "taskfast-agent",
            "status": "active",
        })))
        .mount(server)
        .await;
}

async fn mount_profile_active(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": MINTED_AGENT_ID,
            "name": "taskfast-agent",
            "status": "active",
            "capabilities": ["general"],
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

async fn mount_post_wallet(server: &MockServer, address: &str) {
    Mock::given(method("POST"))
        .and(path("/api/agents/me/wallet"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tempo_wallet_address": address,
            "payout_method": "tempo_wallet",
            "payment_method": "tempo",
            "ready_to_work": true,
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn interactive_byow_path_mints_and_registers_wallet() {
    let server = MockServer::start().await;
    mount_users_me(&server, "Alice Smith", "alice@example.com").await;
    mount_post_agents(&server).await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "not_configured", false).await;
    mount_post_wallet(&server, BYOW_ADDRESS).await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let prompter = MockPrompter::new()
        .with_pat("tf_user_interactive_abc")
        .with_wallet_mode(WalletMode::Byow)
        .with_wallet_address(BYOW_ADDRESS);

    let envelope = run_with_prompter(
        &ctx_for(&server, cfg_path.clone()),
        base_args(),
        &prompter,
        true,
    )
    .await
    .expect("interactive BYOW path should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["agent"]["action"], "minted");
    assert_eq!(v["data"]["agent"]["id"], MINTED_AGENT_ID);
    assert_eq!(v["data"]["wallet"]["status"], "byo_registered");
    assert_eq!(v["data"]["wallet"]["address"], BYOW_ADDRESS);

    let loaded = Config::load(&cfg_path).unwrap();
    assert_eq!(loaded.api_key.as_deref(), Some(MINTED_KEY));
    assert_eq!(loaded.wallet_address.as_deref(), Some(BYOW_ADDRESS));
}

#[tokio::test]
async fn interactive_skip_wallet_records_skipped_outcome() {
    let server = MockServer::start().await;
    mount_users_me(&server, "Bob", "bob@example.com").await;
    mount_post_agents(&server).await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "not_configured", false).await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let prompter = MockPrompter::new()
        .with_pat("tf_user_skipper")
        .with_wallet_mode(WalletMode::Skip);

    let envelope = run_with_prompter(
        &ctx_for(&server, cfg_path.clone()),
        base_args(),
        &prompter,
        true,
    )
    .await
    .expect("skip-wallet path should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["wallet"]["status"], "skipped");
    assert!(
        v["data"]["wallet"].get("address").is_none(),
        "skip path must not surface an address"
    );
}

#[tokio::test]
async fn interactive_users_me_404_falls_back_to_neutral_greeting() {
    // Greeting must not be load-bearing — if the server has no /users/me
    // endpoint yet, init still proceeds through mint + wallet.
    let server = MockServer::start().await;
    mount_users_me_404(&server).await;
    mount_post_agents(&server).await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "not_configured", false).await;
    mount_post_wallet(&server, BYOW_ADDRESS).await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let prompter = MockPrompter::new()
        .with_pat("tf_user_old_server")
        .with_wallet_mode(WalletMode::Byow)
        .with_wallet_address(BYOW_ADDRESS);

    let envelope = run_with_prompter(
        &ctx_for(&server, cfg_path.clone()),
        base_args(),
        &prompter,
        true,
    )
    .await
    .expect("404 on /users/me must not abort init");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["agent"]["action"], "minted");
}

#[tokio::test]
async fn no_interactive_flag_disables_tui_and_surfaces_missing_key() {
    // With no api_key, no PAT, and the TUI gate forced off, init must
    // fall back to the original `MissingApiKey` error — no prompting.
    let server = MockServer::start().await;
    // Deliberately no HTTP mocks — a silent prompt fire would still try
    // to fetch /users/me and error out differently.

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let prompter = MockPrompter::new(); // empty — panics if touched

    let err = run_with_prompter(&ctx_for(&server, cfg_path), base_args(), &prompter, false)
        .await
        .expect_err("non-interactive + no key must error");

    assert!(
        matches!(err, CmdError::MissingApiKey),
        "expected MissingApiKey, got {err:?}"
    );
}

#[tokio::test]
async fn interactive_generate_wallet_uses_prompted_password() {
    // Generate path must pick up the prompted password via
    // `inline_wallet_password` (the only route that doesn't require a
    // file or env var). Keystore write + wallet register both land.
    let server = MockServer::start().await;
    mount_users_me(&server, "Gen", "gen@example.com").await;
    mount_post_agents(&server).await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "missing", false).await;
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
    let keystore_path = tmp.path().join("wallet.json");

    let mut args = base_args();
    args.keystore_path = Some(keystore_path.clone());

    let prompter = MockPrompter::new()
        .with_pat("tf_user_gen")
        .with_wallet_mode(WalletMode::Generate)
        .with_wallet_password("s3kret-prompt");

    let envelope = run_with_prompter(&ctx_for(&server, cfg_path.clone()), args, &prompter, true)
        .await
        .expect("generate path should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["wallet"]["status"], "generated");
    assert!(keystore_path.exists(), "keystore must be written");
}

#[tokio::test]
async fn interactive_skipped_when_api_key_already_in_ctx() {
    // Idempotent re-init: ctx already carries an agent key, so even
    // though interactive=true, neither the PAT nor the wallet-mode
    // prompt should fire. The MockPrompter has no queued values — any
    // accidental prompt call would error.
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", true).await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let ctx = Ctx {
        api_key: Some(MINTED_KEY.into()),
        ..ctx_for(&server, cfg_path.clone())
    };
    let prompter = MockPrompter::new();

    let envelope = run_with_prompter(&ctx, base_args(), &prompter, true)
        .await
        .expect("pre-existing key path must not prompt");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    // Wallet already complete on server + no flag + interactive gate
    // would normally prompt; but the guard only prompts on FRESH key
    // resolution, not when ctx supplied one. Confirm status.
    assert_eq!(v["data"]["wallet"]["status"], "already_configured");
}
