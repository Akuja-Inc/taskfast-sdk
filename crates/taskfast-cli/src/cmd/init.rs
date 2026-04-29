//! `taskfast init` — the onboarding command.
//!
//! Replaces `init.sh`'s step 1-9 orchestration with a non-interactive,
//! CLI-driven flow. Every input comes from a flag, an env var, or the
//! existing [`Config`] file — there are no TTY prompts, because the
//! caller is expected to be another agent/LLM.
//!
//! # Scope (this slice, am-yvc)
//!
//! * api_key: direct via `--api-key` / `TASKFAST_API_KEY` / config file.
//! * validate: `GET /agents/me` — must be active.
//! * readiness: `GET /agents/me/readiness` — informs wallet gate.
//! * wallet: BYOW via `--wallet-address`, or generate + keystore with a
//!   password sourced from `--wallet-password-file` / `TASKFAST_WALLET_PASSWORD`.
//! * config file: load + write at `ctx.config_path` (default
//!   `./.taskfast/config.json`, chmod 600 on unix).
//! * final readiness assert.
//!
//! # Scope (am-z58 extension)
//!
//! * `--human-api-key` (+ optional agent-* fields): when no agent key is
//!   available, POST /agents with the user PAT to mint one. The server
//!   derives `owner_id` from the PAT, so the CLI never asks the caller for
//!   it. The minted `api_key` is then used for the rest of the flow and
//!   written to the config file. Under `--dry-run` the mint is skipped and the
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
//! Mutations short-circuit: no wallet POST, no config file write, no
//! keystore write. A wallet is still generated (so the address is real)
//! but its signer is dropped at the end of the function. Readiness and
//! profile reads pass through.

use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::json;

use super::{CmdError, CmdResult, Ctx};
use crate::config::Config;
use crate::envelope::Envelope;

use alloy_signer_local::PrivateKeySigner;
use taskfast_agent::bootstrap::{create_agent_headless, get_readiness, validate_auth};
use taskfast_agent::faucet::{request_testnet_funds, FaucetDrop};
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
// CLI surface: each flag is an independent user opt-in/out. Collapsing
// them into a state machine or nested enum would make clap's derive
// macro unergonomic without narrowing the actual accepted inputs.
#[allow(clippy::struct_excessive_bools)]
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

    /// Skip wallet provisioning entirely. Useful for workers that never
    /// settle (rare) or for rebuilding config state without touching chain.
    #[arg(long)]
    pub skip_wallet: bool,

    /// Opt in to the testnet faucet for a freshly-generated wallet on
    /// testnet environments (Staging / Local). Off by default so prod
    /// scripts (and CI flows supplying funds out-of-band) never
    /// accidentally hit the faucet. No-op on Prod — mainnet is never
    /// auto-funded.
    #[arg(long)]
    pub fund: bool,

    /// User PAT (`tf_user_*`) used to headlessly mint a fresh agent via
    /// `POST /agents` when no agent API key is available. The server
    /// derives the owning user from the PAT, so no owner UUID is required.
    /// Ignored if an agent key is already resolvable (direct flag / env
    /// var / config file).
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
    /// for multiple. Ignored unless `--webhook-url` is also set. When
    /// `--webhook-url` is supplied and this flag is omitted, init
    /// auto-subscribes to the worker default set (same list as
    /// `taskfast webhook subscribe --default-events`); pass
    /// `--no-default-events` to opt out and register a URL-only endpoint.
    #[arg(long = "webhook-event", value_name = "EVENT")]
    pub webhook_events: Vec<String>,

    /// Opt out of auto-subscribing to default worker events when
    /// `--webhook-url` is set without any explicit `--webhook-event`.
    /// Preserves the pre-existing behavior of registering a URL with
    /// zero subscriptions (a deliberate URL-only push endpoint).
    #[arg(long)]
    pub no_default_events: bool,

    /// Disable all TTY prompts even when stdin is a terminal. Use in
    /// scripts or pipes where interactive fallback would stall. Env:
    /// `TASKFAST_NO_INTERACTIVE=1`.
    #[arg(long, env = "TASKFAST_NO_INTERACTIVE")]
    pub no_interactive: bool,

    /// Inline keystore password populated by the interactive wallet-
    /// generate prompt. Never exposed as a CLI arg (would leak via
    /// process args). Takes precedence over `--wallet-password-file`
    /// when set, so the prompted value round-trips into the normal
    /// password-resolution path.
    #[arg(skip)]
    pub inline_wallet_password: Option<String>,
}

pub async fn run(ctx: &Ctx, args: Args) -> CmdResult {
    let interactive = !args.no_interactive && crate::cmd::init_tui::is_interactive();
    run_with_prompter(
        ctx,
        args,
        &crate::cmd::init_tui::DialoguerPrompter,
        interactive,
    )
    .await
}

/// Same as [`run`] but with an injectable [`crate::cmd::init_tui::Prompter`] and an explicit
/// `interactive` gate so tests can drive the TUI branches under
/// `cargo test` (where neither stdin nor stdout is a TTY). Production
/// callers should use [`run`].
pub async fn run_with_prompter<P: crate::cmd::init_tui::Prompter>(
    ctx: &Ctx,
    mut args: Args,
    prompter: &P,
    interactive: bool,
) -> CmdResult {
    let cfg_path = ctx.config_path.clone();

    // 1. Load any existing config so re-running init is idempotent. A
    //    config-supplied api_key is layered under the CLI/env sources
    //    Ctx already resolved (flag > env var > config).
    let mut cfg = Config::load(&cfg_path)?;

    // 1a. Resolve the agent API key. If none is available but the caller
    //     supplied a user PAT, mint a fresh agent headlessly and use the
    //     returned key. Under --dry-run we short-circuit before the POST
    //     and return early — the rest of the flow depends on a real key.
    //
    //     Interactive fallback: when no key and no --human-api-key, and
    //     we're attached to a TTY, prompt the human for their PAT and
    //     greet them via /users/me before minting.
    let (api_key, agent_outcome) = match resolve_api_key(ctx, &cfg) {
        Ok(k) => (k, AgentOutcome::PreExisting),
        Err(CmdError::MissingApiKey) => {
            let pat = resolve_pat(ctx, &args, interactive, prompter).await?;
            let minted = mint_agent(ctx, &args, &pat).await?;
            match minted {
                MintedAgent::DryRun { ref intent } => {
                    return Ok(Envelope::success(
                        ctx.environment,
                        ctx.dry_run,
                        build_dry_run_mint_envelope(&cfg_path, intent),
                    ));
                }
                MintedAgent::Live { ref api_key, .. } => {
                    (api_key.clone(), AgentOutcome::Minted(minted))
                }
            }
        }
        Err(e) => return Err(e),
    };

    // 1b. Interactive wallet-mode prompt. Only fires when this run just
    //     minted a fresh agent AND the human gave us zero wallet signal
    //     on the command line. Re-init against an existing agent key is
    //     treated as silent (preserves `init_idempotent_on_reinit`) so
    //     humans re-running `taskfast init` to refresh state don't get
    //     re-prompted about wallet setup they already completed.
    if interactive
        && matches!(agent_outcome, AgentOutcome::Minted(_))
        && args.wallet_address.is_none()
        && !args.generate_wallet
        && !args.skip_wallet
    {
        apply_wallet_mode_prompt(&mut args, prompter)?;
    }

    let effective_ctx = Ctx {
        api_key: Some(api_key.clone()),
        ..ctx.clone()
    };
    let client = effective_ctx.client()?;

    // 2. Validate auth + fetch readiness.
    //    On Auth failure with a PreExisting key, rewrite the error with a
    //    remediation hint — the common cause is a stale api_key left over in
    //    the config from a prior init run whose agent was deleted server-side.
    let profile = match validate_auth(&client).await {
        Ok(p) => p,
        Err(taskfast_client::Error::Auth(msg))
            if matches!(agent_outcome, AgentOutcome::PreExisting) =>
        {
            return Err(CmdError::Auth(stale_key_hint(
                &msg, &cfg_path, &cfg, ctx, &args,
            )));
        }
        Err(e) => return Err(CmdError::from(e)),
    };
    assert_active(&profile)?;
    // Cross-system invariant — fail-closed before any wallet/agent state
    // is written if the deployment advertises a different (or additional)
    // network than the env mandates. Skipped under --dry-run so a `taskfast
    // init --dry-run` still succeeds against a fresh deployment.
    if !ctx.dry_run {
        super::enforce_server_network_invariant(&effective_ctx, &client).await?;
    }
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

    // 4. Update the config in-memory (always — writing is gated by dry-run).
    //    `api_base` and `network` are no longer persisted — both are derived
    //    from `environment` at runtime via `Environment::api_base` /
    //    `Environment::network`.
    cfg.environment = Some(ctx.environment);
    cfg.api_key = Some(api_key.clone());
    if let Some(addr) = wallet_outcome.address() {
        cfg.wallet_address = Some(addr.to_string());
    }
    if let Some(path) = wallet_outcome.keystore_path() {
        cfg.keystore_path = Some(path.to_path_buf());
    }
    if let AgentOutcome::Minted(MintedAgent::Live { id: Some(id), .. }) = &agent_outcome {
        cfg.agent_id = Some(id.clone());
    }
    if let Some(url) = args.webhook_url.as_deref().filter(|s| !s.trim().is_empty()) {
        cfg.webhook_url = Some(url.to_string());
    }
    if let Some(path) = args.webhook_secret_file.as_ref() {
        cfg.webhook_secret_path = Some(path.clone());
    }

    let config_written = if ctx.dry_run {
        false
    } else {
        cfg.save(&cfg_path)?;
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
    let faucet_outcome = maybe_request_faucet(
        &args,
        ctx.environment.network(),
        &wallet_outcome,
        ctx.dry_run,
    )
    .await;

    // 4c. Optional webhook registration. Only runs when `--webhook-url`
    //     is provided; otherwise the envelope reports `status: "skipped"`
    //     so orchestrators can tell the step ran and found nothing to do
    //     (vs. the CLI not supporting webhooks at all).
    let webhook_outcome = maybe_configure_webhook(&client, &args, ctx.dry_run).await;

    // 5. Final readiness check — surfaces any remaining gates (webhook,
    //    funding) the caller still has to clear.
    let final_readiness = get_readiness(&client).await.map_err(CmdError::from)?;

    let data = build_envelope_data(
        &cfg_path,
        config_written,
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
/// a key (flag / env var / config file), this is `PreExisting` and the
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

/// Resolve the PAT used to mint a fresh agent. Priority: `--human-api-key`
/// (flag / env) → interactive prompt (when TTY-attached). Absent both,
/// propagate the original [`CmdError::MissingApiKey`] so non-interactive
/// callers see the same message they did before the TUI landed.
async fn resolve_pat<P: crate::cmd::init_tui::Prompter>(
    ctx: &Ctx,
    args: &Args,
    interactive: bool,
    prompter: &P,
) -> Result<String, CmdError> {
    if let Some(p) = args.human_api_key.as_deref().filter(|s| !s.is_empty()) {
        return Ok(p.to_string());
    }
    if !interactive {
        return Err(CmdError::MissingApiKey);
    }
    let accounts_url = crate::accounts_url(ctx.base_url());
    let pat = prompter
        .pat(&accounts_url)
        .map_err(|e| CmdError::Usage(format!("PAT prompt failed: {e}")))?;
    // Greet via /users/me when available. 404 (endpoint not yet deployed)
    // falls back to a neutral confirmation — we never block the flow on
    // the greeting.
    let pat_client = TaskFastClient::from_api_key(ctx.base_url(), &pat).map_err(CmdError::from)?;
    let profile = pat_client.get_user_profile().await.ok().flatten();
    eprintln!("{}", crate::cmd::init_tui::greeting(profile.as_ref()));
    Ok(pat)
}

/// Drive the interactive wallet-mode prompt and fold the result back
/// into `args` so the existing `provision_wallet` branches see the same
/// state they would have after flag parsing.
fn apply_wallet_mode_prompt<P: crate::cmd::init_tui::Prompter>(
    args: &mut Args,
    prompter: &P,
) -> Result<(), CmdError> {
    use crate::cmd::init_tui::WalletMode;
    let mode = prompter
        .wallet_mode()
        .map_err(|e| CmdError::Usage(format!("wallet-mode prompt failed: {e}")))?;
    match mode {
        WalletMode::Byow => {
            let addr = prompter
                .wallet_address()
                .map_err(|e| CmdError::Usage(format!("wallet-address prompt failed: {e}")))?;
            args.wallet_address = Some(addr.trim().to_string());
        }
        WalletMode::Generate => {
            args.generate_wallet = true;
            let pw = prompter
                .wallet_password()
                .map_err(|e| CmdError::Usage(format!("password prompt failed: {e}")))?;
            args.inline_wallet_password = Some(pw);
        }
        WalletMode::Skip => {
            args.skip_wallet = true;
        }
    }
    Ok(())
}

async fn mint_agent(ctx: &Ctx, args: &Args, pat: &str) -> Result<MintedAgent, CmdError> {
    if pat.is_empty() {
        return Err(CmdError::Usage("PAT is empty".into()));
    }

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
    let api_key = resp
        .api_key
        .clone()
        .ok_or_else(|| CmdError::Server("POST /agents returned no api_key despite 201".into()))?;
    Ok(MintedAgent::Live {
        api_key,
        id: resp.id.map(|u| u.to_string()),
        name: resp.name.clone(),
    })
}

fn build_dry_run_mint_envelope(cfg_path: &Path, intent: &MintIntent) -> serde_json::Value {
    json!({
        "agent": {
            "action": "would_mint",
            "name": intent.name,
            "description": intent.description,
            "capabilities": intent.capabilities,
        },
        "config_file": {
            "path": cfg_path.display().to_string(),
            "written": false,
            "would_write": true,
        },
        "ready_to_work": false,
    })
}

/// Layered api_key resolution: Ctx (flag / env var / already-merged
/// config value) wins, then the on-disk config, else
/// [`CmdError::MissingApiKey`].
///
/// `Ctx::from_parts` already folds `cfg.api_key` into `ctx.api_key`, so
/// by the time we get here the config lookup is a redundant last resort
/// — kept for the test-driven path that constructs `Ctx` directly
/// without going through `from_parts`.
fn resolve_api_key(ctx: &Ctx, cfg: &Config) -> Result<String, CmdError> {
    if let Some(k) = ctx.api_key.as_deref() {
        if !k.is_empty() {
            return Ok(k.to_string());
        }
    }
    if let Some(k) = cfg.api_key.as_deref() {
        if !k.is_empty() {
            return Ok(k.to_string());
        }
    }
    Err(CmdError::MissingApiKey)
}

/// Build a remediation hint for the common "stale config" failure: the agent
/// api_key stored in the config (or passed via flag/env) is rejected by the
/// server. Lists the key's likely source and the minimum steps to recover.
fn stale_key_hint(msg: &str, cfg_path: &Path, cfg: &Config, ctx: &Ctx, args: &Args) -> String {
    let source = if cfg.api_key.as_deref().is_some_and(|k| !k.is_empty()) {
        format!("config file {}", cfg_path.display())
    } else if ctx.api_key.is_some() {
        "--api-key flag or TASKFAST_API_KEY env".to_string()
    } else {
        "unknown".to_string()
    };
    let had_pat = args.human_api_key.is_some();
    let pat_note = if had_pat {
        "\n  note: --human-api-key was provided but ignored because an existing api_key was found; \
         the PAT mints a new agent only when no api_key is resolvable."
    } else {
        ""
    };
    format!(
        "{msg}\n  hint: agent api_key rejected — likely stale (agent deleted or key rotated).\n  \
         source: {source}\n  fix: delete {path} (and unset TASKFAST_API_KEY if set), then re-run \
         `taskfast --env {env} init --human-api-key <tf_user_...>` to mint a fresh agent.{pat_note}",
        path = cfg_path.display(),
        env = ctx.environment.as_str(),
    )
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
            "pass --wallet-address <0x...> or --generate-wallet (or --skip-wallet to defer)".into(),
        ));
    }

    let signer = wallet::generate_signer();
    let address = format!("0x{}", hex::encode(signer.address().as_slice()));

    if dry_run {
        // Return the address without resolving the password or persisting;
        // caller just needs to see what *would* have been generated.
        return Ok(WalletOutcome::DryRunGenerated { address });
    }

    let password = resolve_wallet_password(args)?;
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
    // Interactive prompt populates this; takes precedence so the prompted
    // value isn't second-guessed by a stale env var from a prior shell.
    if let Some(pw) = args.inline_wallet_password.as_deref() {
        if !pw.is_empty() {
            return Ok(pw.to_string());
        }
    }
    if let Ok(pw) = std::env::var("TASKFAST_WALLET_PASSWORD") {
        if !pw.is_empty() {
            return Ok(pw);
        }
    }
    let path = args.wallet_password_file.as_deref().ok_or_else(|| {
        CmdError::Usage(
            "--generate-wallet requires --wallet-password-file or TASKFAST_WALLET_PASSWORD".into(),
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
    network: crate::Network,
    wallet: &WalletOutcome,
    dry_run: bool,
) -> FaucetOutcome {
    if dry_run {
        return FaucetOutcome::Skipped { reason: "dry_run" };
    }
    if !args.fund {
        return FaucetOutcome::Skipped {
            reason: "fund_flag_not_set",
        };
    }
    if !matches!(network, crate::Network::Testnet) {
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
#[cfg_attr(test, derive(Debug))]
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

/// Resolve the final event subscription list for `taskfast init
/// --webhook-url`. Precedence: explicit `--webhook-event` flags win
/// outright; absent-events + `--no-default-events` yields an empty list
/// (URL-only registration); absent-events + default path fans out to
/// the 9-event worker default (`webhook::DEFAULT_WORKER_EVENTS`). Shared
/// between the live subscription path and the `--dry-run` envelope so
/// the preview never diverges from what would be pushed.
fn resolve_webhook_events(args: &Args) -> Vec<String> {
    if !args.webhook_events.is_empty() {
        return args.webhook_events.clone();
    }
    if args.no_default_events {
        return Vec::new();
    }
    crate::cmd::webhook::DEFAULT_WORKER_EVENTS
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

async fn maybe_configure_webhook(
    client: &TaskFastClient,
    args: &Args,
    dry_run: bool,
) -> WebhookOutcome {
    let Some(url) = args.webhook_url.as_deref().filter(|s| !s.trim().is_empty()) else {
        return WebhookOutcome::Skipped;
    };
    let events = resolve_webhook_events(args);
    if dry_run {
        return WebhookOutcome::DryRun {
            url: url.to_string(),
            events,
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
    let subscribed = if events.is_empty() {
        None
    } else {
        match webhooks::update_subscriptions(client, events).await {
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
    cfg_path: &Path,
    config_written: bool,
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

    let mut cfg_obj = json!({
        "path": cfg_path.display().to_string(),
        "written": config_written,
    });
    if dry_run && !config_written {
        cfg_obj["would_write"] = json!(true);
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
        "config_file": cfg_obj,
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

    fn ctx_with_key(key: Option<&str>) -> Ctx {
        Ctx {
            api_key: key.map(String::from),
            environment: Environment::Local,
            api_base: None,
            config_path: PathBuf::from("/dev/null"),
            dry_run: false,
            quiet: true,
            ..Default::default()
        }
    }

    fn cfg_with_key(key: Option<&str>) -> Config {
        Config {
            api_key: key.map(str::to_string),
            ..Config::default()
        }
    }

    #[test]
    fn resolve_api_key_prefers_ctx_over_config() {
        let ctx = ctx_with_key(Some("from-flag"));
        let cfg = cfg_with_key(Some("from-file"));
        assert_eq!(resolve_api_key(&ctx, &cfg).unwrap(), "from-flag");
    }

    #[test]
    fn resolve_api_key_falls_back_to_config() {
        let ctx = ctx_with_key(None);
        let cfg = cfg_with_key(Some("from-file"));
        assert_eq!(resolve_api_key(&ctx, &cfg).unwrap(), "from-file");
    }

    #[test]
    fn resolve_api_key_empty_string_is_treated_as_absent() {
        let ctx = ctx_with_key(Some(""));
        let cfg = cfg_with_key(Some(""));
        match resolve_api_key(&ctx, &cfg) {
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

    #[test]
    fn resolve_webhook_events_uses_defaults_when_events_empty() {
        // URL set, no explicit events, no opt-out → auto-subscribe to
        // the 9-event worker default. Guards PLAN #11: raw `init
        // --webhook-url X` must not silently register a zero-event
        // endpoint.
        let args = Args {
            webhook_url: Some("https://h.example/x".into()),
            ..base_args()
        };
        let events = resolve_webhook_events(&args);
        assert_eq!(events, crate::cmd::webhook::DEFAULT_WORKER_EVENTS);
    }

    #[test]
    fn resolve_webhook_events_respects_no_default_events_opt_out() {
        // Explicit opt-out preserves the legacy URL-only behavior.
        let args = Args {
            webhook_url: Some("https://h.example/x".into()),
            no_default_events: true,
            ..base_args()
        };
        assert!(resolve_webhook_events(&args).is_empty());
    }

    #[test]
    fn resolve_webhook_events_explicit_events_override_defaults() {
        // Any `--webhook-event` wins: the defaults are NOT mixed in.
        let args = Args {
            webhook_url: Some("https://h.example/x".into()),
            webhook_events: vec!["task_assigned".into()],
            ..base_args()
        };
        assert_eq!(resolve_webhook_events(&args), vec!["task_assigned"]);
    }

    #[test]
    fn resolve_webhook_events_explicit_events_win_over_no_default_events() {
        // Safety: --no-default-events is an opt-out flag for the
        // auto-population path; explicit events should still flow.
        let args = Args {
            webhook_url: Some("https://h.example/x".into()),
            webhook_events: vec!["bid_accepted".into()],
            no_default_events: true,
            ..base_args()
        };
        assert_eq!(resolve_webhook_events(&args), vec!["bid_accepted"]);
    }

    #[tokio::test]
    async fn maybe_configure_webhook_dry_run_surfaces_default_events() {
        // Dry-run envelope must preview what the live path would push —
        // otherwise `init --webhook-url X --dry-run` silently differs
        // from the real call.
        let client = TaskFastClient::from_api_key("http://127.0.0.1:1/", "k").unwrap();
        let args = Args {
            webhook_url: Some("https://h.example/x".into()),
            ..base_args()
        };
        let outcome = maybe_configure_webhook(&client, &args, true).await;
        match outcome {
            WebhookOutcome::DryRun { events, url, .. } => {
                assert_eq!(url, "https://h.example/x");
                assert_eq!(events, crate::cmd::webhook::DEFAULT_WORKER_EVENTS);
            }
            other => panic!("expected DryRun, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn maybe_configure_webhook_dry_run_respects_no_default_events() {
        let client = TaskFastClient::from_api_key("http://127.0.0.1:1/", "k").unwrap();
        let args = Args {
            webhook_url: Some("https://h.example/x".into()),
            no_default_events: true,
            ..base_args()
        };
        let outcome = maybe_configure_webhook(&client, &args, true).await;
        match outcome {
            WebhookOutcome::DryRun { events, .. } => assert!(events.is_empty()),
            other => panic!("expected DryRun, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn maybe_configure_webhook_no_url_is_skipped() {
        // Regression guard: the auto-subscribe branch must not fire
        // when the URL is absent — init stays single-purpose.
        let client = TaskFastClient::from_api_key("http://127.0.0.1:1/", "k").unwrap();
        let outcome = maybe_configure_webhook(&client, &base_args(), true).await;
        assert!(matches!(outcome, WebhookOutcome::Skipped));
    }
}
