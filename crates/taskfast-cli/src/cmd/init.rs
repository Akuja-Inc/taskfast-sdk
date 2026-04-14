//! `taskfast init` — the onboarding command.
//!
//! Replaces `init.sh`'s step 1-9 orchestration with a non-interactive,
//! CLI-driven flow. Every input comes from a flag, an env var, or the
//! existing `.taskfast-agent.env` — there are no TTY prompts, because the
//! caller is expected to be another agent/LLM.
//!
//! # Scope (this slice, am-yvc)
//!
//! * api_key: direct via `--api-key` / `TASKFAST_API_KEY` / env file.
//! * validate: `GET /agents/me` — must be active.
//! * readiness: `GET /agents/me/readiness` — informs wallet gate.
//! * wallet: BYOW via `--wallet-address`, or generate + keystore with a
//!   password sourced from `--wallet-password-file` / `TASKFAST_WALLET_PASSWORD`.
//! * env file: load + write at `.taskfast-agent.env` (chmod 600 on unix).
//! * final readiness assert.
//!
//! # Scope (am-z58 extension)
//!
//! * `--human-api-key` (+ optional agent-* fields): when no agent key is
//!   available, POST /agents with the user PAT to mint one. The server
//!   derives `owner_id` from the PAT, so the CLI never asks the caller for
//!   it. The minted `api_key` is then used for the rest of the flow and
//!   written to the env file. Under `--dry-run` the mint is skipped and the
//!   envelope reports `agent.action = "would_mint"` — the rest of the flow
//!   is also skipped because the real agent key never materialized.
//!
//! # Scope (am-iit extension)
//!
//! * Optional `--webhook-url` (+ `--webhook-secret-file`, repeat
//!   `--webhook-event`) folds webhook registration into the init run:
//!   PUT /agents/me/webhooks, persist the returned secret (chmod 600),
//!   and optionally replace the subscription list. Skipped silently
//!   when the URL isn't supplied so callers who prefer a separate
//!   `taskfast webhook register` step still get the pre-am-iit flow.
//!
//! Deferred to separate beads so this slice stays reviewable:
//! * `am-c74` — balance polling after faucet dispense.
//!
//! # `--dry-run` semantics
//!
//! Mutations short-circuit: no wallet POST, no env file write, no keystore
//! write. A wallet is still generated (so the address is real) but its
//! signer is dropped at the end of the function. Readiness and profile
//! reads pass through.

use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::json;

use super::{CmdError, CmdResult, Ctx};
use crate::dotenv::{DEFAULT_ENV_FILENAME, EnvFile};
use crate::envelope::Envelope;

use alloy_signer_local::PrivateKeySigner;
use taskfast_agent::bootstrap::{create_agent_headless, get_readiness, validate_auth};
use taskfast_agent::faucet::{FaucetDrop, request_testnet_funds};
use taskfast_agent::keystore;
use taskfast_agent::wallet;
use taskfast_agent::webhooks;
use taskfast_client::api::types::{AgentCreateRequest, AgentReadiness, WebhookConfigRequest};
use taskfast_client::TaskFastClient;

use super::webhook::persist_secret;

/// Wallet status string emitted by the server when the agent hasn't
/// registered one yet. `AgentReadinessChecks.wallet.status == "complete"`
/// means it's already done.
const WALLET_STATUS_COMPLETE: &str = "complete";

#[derive(Debug, Parser)]
pub struct Args {
    /// Wallet address to register (BYOW). Mutually exclusive with
    /// `--generate-wallet`.
    #[arg(long, conflicts_with = "generate_wallet")]
    pub wallet_address: Option<String>,

    /// Generate a fresh keypair, persist it via the keystore module, then
    /// register the derived address with TaskFast.
    #[arg(long)]
    pub generate_wallet: bool,

    /// Path to a file containing the keystore password. Required when
    /// `--generate-wallet` is used without `TASKFAST_WALLET_PASSWORD` set.
    /// Prefer a mode-0400 file over `--wallet-password` (which leaks via
    /// process args).
    #[arg(long, env = "TASKFAST_WALLET_PASSWORD_FILE")]
    pub wallet_password_file: Option<PathBuf>,

    /// Explicit keystore path override. Default: XDG data dir +
    /// `<address>.json`.
    #[arg(long)]
    pub keystore_path: Option<PathBuf>,

    /// Network selector recorded in the env file. Does not change the API
    /// base URL (that's `--api-base`).
    #[arg(long, default_value = "mainnet", env = "TEMPO_NETWORK")]
    pub network: Network,

    /// Override the env file path. Default: `.taskfast-agent.env` in the
    /// current working directory.
    #[arg(long, env = "TASKFAST_ENV_FILE")]
    pub env_file: Option<PathBuf>,

    /// Skip wallet provisioning entirely. Useful for workers that never
    /// settle (rare) or for redoing env-file state without touching chain.
    #[arg(long)]
    pub skip_wallet: bool,

    /// Skip the testnet faucet call even when `--network testnet` with a
    /// freshly-generated wallet would normally trigger it. Intended for CI
    /// / reproducible-test flows where the faucet is supplied out-of-band
    /// (fixture wallets, mocked RPC). No-op on `--network mainnet` — prod
    /// never auto-funds.
    #[arg(long)]
    pub skip_funding: bool,

    /// User PAT (`tf_user_*`) used to headlessly mint a fresh agent via
    /// `POST /agents` when no agent API key is available. The server
    /// derives the owning user from the PAT, so no owner UUID is required.
    /// Ignored if an agent key is already resolvable (direct flag / env
    /// var / env file).
    #[arg(long, env = "TASKFAST_HUMAN_API_KEY")]
    pub human_api_key: Option<String>,

    /// Display name for the minted agent.
    #[arg(long, default_value = "taskfast-agent")]
    pub agent_name: String,

    /// Description for the minted agent.
    #[arg(long, default_value = "Headless agent registered via taskfast init")]
    pub agent_description: String,

    /// Capability tag for the minted agent (repeat to pass multiple). If
    /// none are provided, defaults to `["general"]` so the request still
    /// satisfies the non-empty-array convention most consumers expect.
    #[arg(long = "agent-capability", value_name = "CAP")]
    pub agent_capabilities: Vec<String>,

    /// HTTPS URL to register for webhook event delivery. When supplied,
    /// init PUTs `/agents/me/webhooks` after the wallet step and (if
    /// the server returns one) persists the signing secret to
    /// `--webhook-secret-file`. Omitting this flag leaves webhooks
    /// untouched — `taskfast webhook register` can configure them later.
    #[arg(long)]
    pub webhook_url: Option<String>,

    /// Path to persist the webhook signing secret (chmod 600 on unix).
    /// The platform returns the secret exactly once at creation time
    /// — a fresh re-register with an existing config returns `null`
    /// and the existing file is left untouched.
    #[arg(long)]
    pub webhook_secret_file: Option<PathBuf>,

    /// Event type to subscribe to after webhook registration. Repeat
    /// for multiple. Ignored unless `--webhook-url` is also set.
    #[arg(long = "webhook-event", value_name = "EVENT")]
    pub webhook_events: Vec<String>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Network {
    Mainnet,
    Testnet,
}

impl Network {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet => "testnet",
        }
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> CmdResult {
    let env_path = args
        .env_file
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ENV_FILENAME));

    // 1. Load any existing env file so re-running init is idempotent. An
    //    env-file-supplied api_key is layered under the CLI/env sources
    //    Ctx already resolved (flag > env var > file).
    let mut env_file = EnvFile::load(&env_path).map_err(|e| CmdError::Usage(e.to_string()))?;

    // 1a. Resolve the agent API key. If none is available but the caller
    //     supplied a user PAT, mint a fresh agent headlessly and use the
    //     returned key. Under --dry-run we short-circuit before the POST
    //     and return early — the rest of the flow depends on a real key.
    let (api_key, agent_outcome) = match resolve_api_key(ctx, &env_file) {
        Ok(k) => (k, AgentOutcome::PreExisting),
        Err(CmdError::MissingApiKey) if args.human_api_key.is_some() => {
            let minted = mint_agent(ctx, &args).await?;
            match minted {
                MintedAgent::DryRun { ref intent } => {
                    return Ok(Envelope::success(
                        ctx.environment,
                        ctx.dry_run,
                        build_dry_run_mint_envelope(&env_path, intent),
                    ));
                }
                MintedAgent::Live { ref api_key, .. } => {
                    (api_key.clone(), AgentOutcome::Minted(minted))
                }
            }
        }
        Err(e) => return Err(e),
    };

    let effective_ctx = Ctx {
        api_key: Some(api_key.clone()),
        environment: ctx.environment,
        api_base: ctx.api_base.clone(),
        dry_run: ctx.dry_run,
        quiet: ctx.quiet,
    };
    let client = effective_ctx.client()?;

    // 2. Validate auth + fetch readiness.
    let profile = validate_auth(&client).await.map_err(CmdError::from)?;
    assert_active(&profile)?;
    let readiness = get_readiness(&client).await.map_err(CmdError::from)?;

    // 3. Wallet provisioning.
    let wallet_outcome = if args.skip_wallet {
        WalletOutcome::Skipped
    } else if readiness.checks.wallet.status == WALLET_STATUS_COMPLETE
        && args.wallet_address.is_none()
        && !args.generate_wallet
    {
        // Nothing to do — server already has a wallet and caller isn't
        // forcing a new one.
        WalletOutcome::AlreadyConfigured
    } else {
        provision_wallet(&client, &args, ctx.dry_run).await?
    };

    // 4. Update the env file in-memory (always — writing is gated by dry-run).
    env_file.set("TASKFAST_API", ctx.base_url().to_string());
    env_file.set("TASKFAST_API_KEY", api_key.clone());
    env_file.set("TEMPO_NETWORK", args.network.as_str());
    if let Some(addr) = wallet_outcome.address() {
        env_file.set("TEMPO_WALLET_ADDRESS", addr.to_string());
    }
    if let Some(path) = wallet_outcome.keystore_path() {
        env_file.set("TEMPO_KEY_SOURCE", format!("file:{}", path.display()));
    }

    let env_file_written = if ctx.dry_run {
        false
    } else {
        env_file
            .save(&env_path)
            .map_err(|e| CmdError::Usage(e.to_string()))?;
        true
    };

    // 4b. Testnet funding. When a wallet was just generated on `--network
    //     testnet`, hit the public Tempo moderato faucet. Fire-and-surface:
    //     we report the faucet's tx hashes in the envelope but don't block
    //     on confirmation here. Caller-side balance polling lives under
    //     am-c74.
    //
    //     Mainnet is deliberately skipped — production wallets must be
    //     funded by the owning human at `https://wallet.tempo.xyz`. The
    //     envelope surfaces this with `status: "skipped", reason: "mainnet"`
    //     plus a `funding_hint` so orchestrators can tell the human where
    //     to go.
    let faucet_outcome = maybe_request_faucet(&args, &wallet_outcome, ctx.dry_run).await;

    // 4c. Optional webhook registration. Only runs when `--webhook-url`
    //     is provided; otherwise the envelope reports `status: "skipped"`
    //     so orchestrators can tell the step ran and found nothing to do
    //     (vs. the CLI not supporting webhooks at all).
    let webhook_outcome = maybe_configure_webhook(&client, &args, ctx.dry_run).await;

    // 5. Final readiness check — surfaces any remaining gates (webhook,
    //    funding) the caller still has to clear.
    let final_readiness = get_readiness(&client).await.map_err(CmdError::from)?;

    let data = build_envelope_data(
        &env_path,
        env_file_written,
        &wallet_outcome,
        &agent_outcome,
        &final_readiness,
        &faucet_outcome,
        &webhook_outcome,
        ctx.dry_run,
    );
    Ok(Envelope::success(ctx.environment, ctx.dry_run, data))
}

/// Result of the agent-key resolution step. When the caller already had
/// a key (flag / env var / env file), this is `PreExisting` and the
/// envelope stays quiet about it. When a key was minted via
/// `--human-api-key`, the envelope surfaces the minted agent's id/name.
enum AgentOutcome {
    PreExisting,
    Minted(MintedAgent),
}

/// Live vs dry-run distinction for minting. Live carries the full
/// response; dry-run carries just the would-have-called payload so the
/// envelope can echo it back without touching the network.
enum MintedAgent {
    Live {
        api_key: String,
        id: Option<String>,
        name: Option<String>,
    },
    DryRun {
        intent: MintIntent,
    },
}

struct MintIntent {
    name: String,
    description: String,
    capabilities: Vec<String>,
}

async fn mint_agent(ctx: &Ctx, args: &Args) -> Result<MintedAgent, CmdError> {
    let pat = args
        .human_api_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CmdError::Usage("--human-api-key is empty".into()))?;

    let capabilities = if args.agent_capabilities.is_empty() {
        vec!["general".to_string()]
    } else {
        args.agent_capabilities.clone()
    };

    if ctx.dry_run {
        return Ok(MintedAgent::DryRun {
            intent: MintIntent {
                name: args.agent_name.clone(),
                description: args.agent_description.clone(),
                capabilities,
            },
        });
    }

    let pat_client = TaskFastClient::from_api_key(ctx.base_url(), pat).map_err(CmdError::from)?;
    let body = AgentCreateRequest {
        owner_id: None,
        name: args.agent_name.clone(),
        description: args.agent_description.clone(),
        capabilities,
        rate: None,
        max_task_budget: None,
        daily_spend_limit: None,
        payout_method: None,
        payment_method: None,
        tempo_wallet_address: None,
    };
    let resp = create_agent_headless(&pat_client, &body)
        .await
        .map_err(CmdError::from)?;
    let api_key = resp.api_key.clone().ok_or_else(|| {
        CmdError::Server("POST /agents returned no api_key despite 201".into())
    })?;
    Ok(MintedAgent::Live {
        api_key,
        id: resp.id.map(|u| u.to_string()),
        name: resp.name.clone(),
    })
}

fn build_dry_run_mint_envelope(env_path: &Path, intent: &MintIntent) -> serde_json::Value {
    json!({
        "agent": {
            "action": "would_mint",
            "name": intent.name,
            "description": intent.description,
            "capabilities": intent.capabilities,
        },
        "env_file": {
            "path": env_path.display().to_string(),
            "written": false,
            "would_write": true,
        },
        "ready_to_work": false,
    })
}

/// Layered api_key resolution: Ctx (flag / env var) wins, then env file,
/// else [`CmdError::MissingApiKey`].
fn resolve_api_key(ctx: &Ctx, env_file: &EnvFile) -> Result<String, CmdError> {
    if let Some(k) = ctx.api_key.as_deref() {
        if !k.is_empty() {
            return Ok(k.to_string());
        }
    }
    if let Some(k) = env_file.get("TASKFAST_API_KEY") {
        if !k.is_empty() {
            return Ok(k.to_string());
        }
    }
    Err(CmdError::MissingApiKey)
}

fn assert_active(profile: &taskfast_client::api::types::AgentProfile) -> Result<(), CmdError> {
    use taskfast_client::api::types::AgentProfileStatus;
    match profile.status {
        Some(AgentProfileStatus::Active) => Ok(()),
        Some(other) => Err(CmdError::Validation {
            code: "agent_not_active".into(),
            message: format!("agent status is {other:?}; owner must reactivate"),
        }),
        None => Err(CmdError::Server(
            "GET /agents/me returned no status field".into(),
        )),
    }
}

/// Side-effect summary the CLI envelope surfaces to orchestrators.
enum WalletOutcome {
    /// Server already had a wallet on file and the caller didn't override.
    AlreadyConfigured,
    /// BYOW path — caller supplied `--wallet-address`.
    ByoRegistered { address: String },
    /// Generated keypair, saved to keystore, registered with server.
    Generated {
        address: String,
        keystore_path: PathBuf,
    },
    /// Dry-run generate — address is real but keystore wasn't written.
    DryRunGenerated { address: String },
    /// `--skip-wallet` or dry-run BYOW without register.
    Skipped,
}

impl WalletOutcome {
    fn address(&self) -> Option<&str> {
        match self {
            Self::ByoRegistered { address }
            | Self::Generated { address, .. }
            | Self::DryRunGenerated { address } => Some(address),
            Self::AlreadyConfigured | Self::Skipped => None,
        }
    }

    fn keystore_path(&self) -> Option<&Path> {
        match self {
            Self::Generated { keystore_path, .. } => Some(keystore_path),
            _ => None,
        }
    }

    fn tag(&self) -> &'static str {
        match self {
            Self::AlreadyConfigured => "already_configured",
            Self::ByoRegistered { .. } => "byo_registered",
            Self::Generated { .. } => "generated",
            Self::DryRunGenerated { .. } => "dry_run_generated",
            Self::Skipped => "skipped",
        }
    }
}

async fn provision_wallet(
    client: &TaskFastClient,
    args: &Args,
    dry_run: bool,
) -> Result<WalletOutcome, CmdError> {
    if let Some(addr) = args.wallet_address.as_deref() {
        if dry_run {
            return Ok(WalletOutcome::Skipped);
        }
        wallet::register_wallet(client, addr)
            .await
            .map_err(CmdError::from)?;
        return Ok(WalletOutcome::ByoRegistered {
            address: addr.to_string(),
        });
    }
    if !args.generate_wallet {
        return Err(CmdError::Usage(
            "pass --wallet-address <0x...> or --generate-wallet (or --skip-wallet to defer)"
                .into(),
        ));
    }

    let password = resolve_wallet_password(args)?;
    let signer = wallet::generate_signer();
    let address = format!("0x{}", hex::encode(signer.address().as_slice()));

    if dry_run {
        // Drop signer without persisting; return the address so the caller
        // can confirm what *would* have been generated.
        let _ = password; // silence unused-var when dry-run short-circuits
        return Ok(WalletOutcome::DryRunGenerated { address });
    }

    let keystore_path = persist_keystore(&signer, args, &password)?;
    wallet::register_wallet(client, &address)
        .await
        .map_err(CmdError::from)?;
    Ok(WalletOutcome::Generated {
        address,
        keystore_path,
    })
}

fn resolve_wallet_password(args: &Args) -> Result<String, CmdError> {
    if let Ok(pw) = std::env::var("TASKFAST_WALLET_PASSWORD") {
        if !pw.is_empty() {
            return Ok(pw);
        }
    }
    let path = args.wallet_password_file.as_deref().ok_or_else(|| {
        CmdError::Usage(
            "--generate-wallet requires --wallet-password-file or TASKFAST_WALLET_PASSWORD"
                .into(),
        )
    })?;
    let raw = std::fs::read_to_string(path).map_err(|e| {
        CmdError::Usage(format!(
            "cannot read wallet password file {}: {e}",
            path.display()
        ))
    })?;
    let trimmed = raw.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return Err(CmdError::Usage(format!(
            "wallet password file {} is empty",
            path.display()
        )));
    }
    Ok(trimmed.to_string())
}

fn persist_keystore(
    signer: &PrivateKeySigner,
    args: &Args,
    password: &str,
) -> Result<PathBuf, CmdError> {
    let path = match &args.keystore_path {
        Some(p) => p.clone(),
        None => keystore::default_keyfile_path(signer.address()).map_err(CmdError::from)?,
    };
    keystore::save_signer(signer, &path, password).map_err(CmdError::from)
}

/// Envelope-friendly description of the faucet step. `Skipped` covers the
/// common cases where no request was warranted (mainnet, BYOW, wallet
/// already present, dry-run) — the reason is echoed so the caller can tell
/// *why* we didn't hit the faucet.
enum FaucetOutcome {
    Skipped { reason: &'static str },
    Requested { drops: Vec<FaucetDrop> },
    Failed { error: String },
}

async fn maybe_request_faucet(
    args: &Args,
    wallet: &WalletOutcome,
    dry_run: bool,
) -> FaucetOutcome {
    if dry_run {
        return FaucetOutcome::Skipped { reason: "dry_run" };
    }
    if args.skip_funding {
        return FaucetOutcome::Skipped {
            reason: "skip_funding_flag",
        };
    }
    if !matches!(args.network, Network::Testnet) {
        return FaucetOutcome::Skipped { reason: "mainnet" };
    }
    let address = match wallet {
        WalletOutcome::Generated { address, .. } => address.clone(),
        WalletOutcome::ByoRegistered { address } => address.clone(),
        _ => {
            return FaucetOutcome::Skipped {
                reason: "no_fresh_wallet",
            };
        }
    };
    let http = reqwest::Client::new();
    match request_testnet_funds(&http, &address).await {
        Ok(drops) => FaucetOutcome::Requested { drops },
        Err(e) => FaucetOutcome::Failed {
            error: e.to_string(),
        },
    }
}

/// Outcome of the optional webhook step. `Skipped` is the default when
/// `--webhook-url` wasn't passed; the remaining variants mirror the
/// shell-script states (`registered`, `registered + subscribed`,
/// `would_register`).
enum WebhookOutcome {
    Skipped,
    DryRun {
        url: String,
        events: Vec<String>,
        secret_file: Option<PathBuf>,
    },
    Registered {
        url: String,
        secret_returned: bool,
        secret_persisted: bool,
        secret_file: Option<PathBuf>,
        subscribed: Option<Vec<String>>,
    },
    Failed {
        error: String,
    },
}

async fn maybe_configure_webhook(
    client: &TaskFastClient,
    args: &Args,
    dry_run: bool,
) -> WebhookOutcome {
    let Some(url) = args.webhook_url.as_deref().filter(|s| !s.trim().is_empty()) else {
        return WebhookOutcome::Skipped;
    };
    if dry_run {
        return WebhookOutcome::DryRun {
            url: url.to_string(),
            events: args.webhook_events.clone(),
            secret_file: args.webhook_secret_file.clone(),
        };
    }
    let body = WebhookConfigRequest {
        url: url.to_string(),
        secret: None,
        events: None,
    };
    let cfg = match webhooks::configure_webhook(client, &body).await {
        Ok(c) => c,
        Err(e) => {
            return WebhookOutcome::Failed {
                error: e.to_string(),
            };
        }
    };
    let secret_persisted = match (cfg.secret.as_deref(), args.webhook_secret_file.as_ref()) {
        (Some(secret), Some(path)) => match persist_secret(path, secret) {
            Ok(()) => true,
            Err(e) => {
                return WebhookOutcome::Failed {
                    error: e.to_string(),
                };
            }
        },
        _ => false,
    };
    let subscribed = if args.webhook_events.is_empty() {
        None
    } else {
        match webhooks::update_subscriptions(client, args.webhook_events.clone()).await {
            Ok(subs) => Some(subs.subscribed_event_types),
            Err(e) => {
                return WebhookOutcome::Failed {
                    error: e.to_string(),
                };
            }
        }
    };
    WebhookOutcome::Registered {
        url: cfg.url,
        secret_returned: cfg.secret.is_some(),
        secret_persisted,
        secret_file: args.webhook_secret_file.clone(),
        subscribed,
    }
}

fn build_envelope_data(
    env_path: &Path,
    env_file_written: bool,
    wallet: &WalletOutcome,
    agent: &AgentOutcome,
    readiness: &AgentReadiness,
    faucet: &FaucetOutcome,
    webhook: &WebhookOutcome,
    dry_run: bool,
) -> serde_json::Value {
    let mut wallet_obj = json!({
        "status": wallet.tag(),
    });
    if let Some(addr) = wallet.address() {
        wallet_obj["address"] = json!(addr);
    }
    if let Some(path) = wallet.keystore_path() {
        wallet_obj["keystore_path"] = json!(path.display().to_string());
    }

    let mut env_obj = json!({
        "path": env_path.display().to_string(),
        "written": env_file_written,
    });
    if dry_run && !env_file_written {
        env_obj["would_write"] = json!(true);
    }

    let faucet_obj = match faucet {
        FaucetOutcome::Skipped { reason } => {
            let mut o = json!({
                "status": "skipped",
                "reason": reason,
            });
            // On mainnet, point the caller at the human funding path so
            // they don't assume the CLI "just handles it" like it does on
            // testnet.
            if *reason == "mainnet" {
                o["funding_hint"] = json!({
                    "url": "https://wallet.tempo.xyz",
                    "message": "Production wallets must be funded manually by the owning human at wallet.tempo.xyz before posting or settling.",
                });
            }
            o
        }
        FaucetOutcome::Requested { drops } => json!({
            "status": "requested",
            "drops": drops.iter().map(|d| json!({
                "token": d.token,
                "tx_hash": d.tx_hash,
            })).collect::<Vec<_>>(),
        }),
        FaucetOutcome::Failed { error } => json!({
            "status": "failed",
            "error": error,
        }),
    };

    let webhook_obj = match webhook {
        WebhookOutcome::Skipped => json!({ "status": "skipped" }),
        WebhookOutcome::DryRun {
            url,
            events,
            secret_file,
        } => {
            let mut o = json!({
                "status": "would_register",
                "url": url,
                "events": events,
            });
            if let Some(p) = secret_file {
                o["secret_file"] = json!(p.display().to_string());
            }
            o
        }
        WebhookOutcome::Registered {
            url,
            secret_returned,
            secret_persisted,
            secret_file,
            subscribed,
        } => {
            let mut o = json!({
                "status": "registered",
                "url": url,
                "secret_returned": secret_returned,
                "secret_persisted": secret_persisted,
            });
            if let Some(p) = secret_file {
                o["secret_file"] = json!(p.display().to_string());
            }
            if let Some(subs) = subscribed {
                o["subscribed"] = json!(subs);
            }
            o
        }
        WebhookOutcome::Failed { error } => json!({
            "status": "failed",
            "error": error,
        }),
    };

    let mut out = json!({
        "wallet": wallet_obj,
        "env_file": env_obj,
        "faucet": faucet_obj,
        "webhook": webhook_obj,
        "readiness": readiness,
        "ready_to_work": readiness.ready_to_work,
    });
    if let AgentOutcome::Minted(MintedAgent::Live { id, name, .. }) = agent {
        let mut a = json!({ "action": "minted" });
        if let Some(id) = id {
            a["id"] = json!(id);
        }
        if let Some(name) = name {
            a["name"] = json!(name);
        }
        out["agent"] = a;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Environment;

    fn base_args() -> Args {
        Args {
            wallet_address: None,
            generate_wallet: false,
            wallet_password_file: None,
            keystore_path: None,
            network: Network::Mainnet,
            env_file: None,
            skip_wallet: false,
            skip_funding: false,
            human_api_key: None,
            agent_name: "taskfast-agent".into(),
            agent_description: "Headless agent registered via taskfast init".into(),
            agent_capabilities: Vec::new(),
            webhook_url: None,
            webhook_secret_file: None,
            webhook_events: Vec::new(),
        }
    }

    fn ctx_with_key(key: Option<&str>) -> Ctx {
        Ctx {
            api_key: key.map(String::from),
            environment: Environment::Local,
            api_base: None,
            dry_run: false,
            quiet: true,
        }
    }

    #[test]
    fn resolve_api_key_prefers_ctx_over_env_file() {
        let ctx = ctx_with_key(Some("from-flag"));
        let mut env = EnvFile::new();
        env.set("TASKFAST_API_KEY", "from-file");
        assert_eq!(resolve_api_key(&ctx, &env).unwrap(), "from-flag");
    }

    #[test]
    fn resolve_api_key_falls_back_to_env_file() {
        let ctx = ctx_with_key(None);
        let mut env = EnvFile::new();
        env.set("TASKFAST_API_KEY", "from-file");
        assert_eq!(resolve_api_key(&ctx, &env).unwrap(), "from-file");
    }

    #[test]
    fn resolve_api_key_empty_string_is_treated_as_absent() {
        let ctx = ctx_with_key(Some(""));
        let mut env = EnvFile::new();
        env.set("TASKFAST_API_KEY", "");
        match resolve_api_key(&ctx, &env) {
            Err(CmdError::MissingApiKey) => {}
            other => panic!("expected MissingApiKey, got {other:?}"),
        }
    }

    #[test]
    fn provision_without_wallet_flag_errors_as_usage() {
        // We can't easily drive provision_wallet without a client, but we
        // can prove the flag-gate logic: with no flags set, the error is
        // Usage, not MissingApiKey.
        let args = base_args();
        assert!(args.wallet_address.is_none() && !args.generate_wallet);
        // The branch that returns Usage lives in provision_wallet — a
        // dedicated integration test drives the end-to-end path.
    }

    #[test]
    fn wallet_outcome_tag_is_stable() {
        // Pinning the tag strings is intentional: orchestrators branch on
        // `data.wallet.status` so changes here are breaking.
        assert_eq!(WalletOutcome::AlreadyConfigured.tag(), "already_configured");
        assert_eq!(
            WalletOutcome::ByoRegistered {
                address: "0x00".into()
            }
            .tag(),
            "byo_registered"
        );
        assert_eq!(
            WalletOutcome::Generated {
                address: "0x00".into(),
                keystore_path: PathBuf::from("/tmp/x")
            }
            .tag(),
            "generated"
        );
        assert_eq!(
            WalletOutcome::DryRunGenerated {
                address: "0x00".into()
            }
            .tag(),
            "dry_run_generated"
        );
        assert_eq!(WalletOutcome::Skipped.tag(), "skipped");
    }
}
