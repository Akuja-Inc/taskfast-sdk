//! `taskfast` binary entry point.
//!
//! Global flag parsing + dispatch to [`cmd`] modules. Every subcommand
//! returns `Result<Envelope, CmdError>`; we print the envelope (unless
//! `--quiet`) and exit with the matching code from [`exit::ExitCode`].

// TODO: tighten doc coverage on public items + remove this allow.
#![allow(missing_docs)]

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use taskfast_cli::{cmd, config::Config, exit, Envelope, Environment};

/// `--log-format` selector. `Text` is the human-friendly default for
/// interactive use; `Json` emits `tracing_subscriber::fmt::json()` lines
/// so logs can be shipped to a structured sink without regex parsing.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

impl LogFormat {
    fn parse_str(s: &str) -> Option<Self> {
        match s {
            "json" => Some(Self::Json),
            "text" => Some(Self::Text),
            _ => None,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "taskfast",
    version,
    about = "TaskFast marketplace CLI — worker + poster hot loop."
)]
struct Cli {
    /// API key (overrides TASKFAST_API_KEY env).
    #[arg(long, global = true, env = "TASKFAST_API_KEY")]
    api_key: Option<String>,

    /// Target environment. When unset, falls back to the config file,
    /// then to `prod`.
    #[arg(long, global = true, env = "TASKFAST_ENV")]
    env: Option<Environment>,

    /// Ad-hoc override for the env-derived API base URL. Never persisted.
    /// Non-well-known values require `--allow-custom-endpoints`; the guard
    /// blocks a malicious config from silently redirecting traffic.
    #[arg(long, global = true, env = "TASKFAST_API")]
    api_base: Option<String>,

    /// Path to the JSON config file. Default: `./.taskfast/config.json`.
    /// Missing file is treated as empty (all fields come from flags /
    /// env vars / defaults).
    #[arg(long, global = true, env = "TASKFAST_CONFIG")]
    config: Option<PathBuf>,

    /// Short-circuit mutations; reads pass through.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Emit tracing logs to stderr. Accepts an `env_logger`-style filter
    /// (e.g. `--verbose=debug`, `--verbose=taskfast_client=trace`).
    #[arg(long, global = true, value_name = "LEVEL", num_args = 0..=1, default_missing_value = "info")]
    verbose: Option<String>,

    /// Log encoding for `--verbose` output. `text` is human-friendly;
    /// `json` ships structured lines for Datadog/Loki ingest. Falls back
    /// to `TASKFAST_LOG_FORMAT`, then `log_format` in the config file,
    /// then `text`.
    #[arg(
        long,
        global = true,
        value_name = "FORMAT",
        env = "TASKFAST_LOG_FORMAT"
    )]
    log_format: Option<LogFormat>,

    /// Suppress even the error envelope (exit code still conveys outcome).
    #[arg(long, global = true, conflicts_with = "verbose")]
    quiet: bool,

    /// Opt into a custom `api_base` or `tempo_rpc_url` that isn't one of
    /// the well-known TaskFast defaults. Off by default: a malicious
    /// `.taskfast/config.json` in a cloned repo would otherwise silently
    /// redirect API traffic and ERC-20 fee transfers to attacker infra.
    /// Accepts `TASKFAST_ALLOW_CUSTOM_ENDPOINTS=1`.
    #[arg(long, global = true, env = "TASKFAST_ALLOW_CUSTOM_ENDPOINTS")]
    allow_custom_endpoints: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Bootstrap an agent: deps, wallet, webhook, funding.
    Init(cmd::init::Args),
    /// Profile + readiness (GET /agents/me + /agents/me/readiness).
    Me(cmd::me::Args),

    /// Liveness probe: single-attempt GET /agents/me with latency.
    Ping(cmd::ping::Args),
    /// Task operations (list, get, submit, approve, dispute, cancel).
    #[command(subcommand)]
    Task(cmd::task::Command),
    /// Bid operations (list, create, cancel, accept, reject).
    #[command(subcommand)]
    Bid(cmd::bid::Command),
    /// Poster: create a task (two-phase draft + sign + submit).
    Post(cmd::post::Args),
    /// Poster: sign a DistributionApproval and settle a task.
    Settle(cmd::settle::Args),
    /// Poster: headless escrow signing for deferred-accept bids.
    #[command(subcommand)]
    Escrow(cmd::escrow::Command),
    /// Event polling (stream as JSON-lines).
    #[command(subcommand)]
    Events(cmd::events::Command),
    /// Webhook configuration, subscriptions, and delivery test.
    #[command(subcommand)]
    Webhook(cmd::webhook::Command),
    /// Worker: browse open-market tasks (GET /tasks).
    Discover(cmd::discover::Args),
    /// Artifacts: list / get / upload / delete on a task.
    #[command(subcommand)]
    Artifact(cmd::artifact::Command),
    /// Messages: send + thread listing on a task.
    #[command(subcommand)]
    Message(cmd::message::Command),
    /// Reviews: create + list by task or by agent.
    #[command(subcommand)]
    Review(cmd::review::Command),
    /// Payments: task escrow breakdown + agent earnings ledger.
    #[command(subcommand)]
    Payment(cmd::payment::Command),
    /// Dispute detail on a task.
    Dispute(cmd::dispute::Args),
    /// Agent directory: list / get / update-me.
    #[command(subcommand)]
    Agent(cmd::agent::Command),
    /// Platform: global config snapshot.
    #[command(subcommand)]
    Platform(cmd::platform::Command),
    /// Wallet: on-chain balance for the caller's agent.
    #[command(subcommand)]
    Wallet(cmd::wallet::Command),
    /// Inspect or edit the project-local JSON config.
    #[command(subcommand)]
    Config(cmd::config::Command),
    /// Install the bundled TaskFast agent skill into local agent folders.
    Skills(cmd::skills::Args),
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    let cfg_path = cli.config.clone().unwrap_or_else(Config::default_path);
    let cfg = match Config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            // Config load failure is fatal and happens before we have
            // a Ctx — fall back to defaults for the error envelope.
            if !cli.quiet {
                let err = cmd::CmdError::Usage(format!("config: {e}"));
                let env = cli.env.unwrap_or(cmd::DEFAULT_ENVIRONMENT);
                Envelope::error(env, cli.dry_run, &err).emit();
            }
            return exit::ExitCode::Usage.into();
        }
    };

    let ctx = match cmd::Ctx::from_parts(
        cli.api_key,
        cli.env,
        cli.api_base,
        Some(cfg_path),
        cli.dry_run,
        cli.quiet,
        cli.allow_custom_endpoints,
        &cfg,
    ) {
        Ok(c) => c,
        Err(e) => {
            // Malformed duration strings in config surface here, before any
            // subcommand runs. Emit the same Usage envelope as a bad flag.
            if !cli.quiet {
                let env = cli.env.unwrap_or(cmd::DEFAULT_ENVIRONMENT);
                Envelope::error(env, cli.dry_run, &e).emit();
            }
            return exit::ExitCode::Usage.into();
        }
    };

    if let Some(level) = cli.verbose.as_deref() {
        let format = cli
            .log_format
            .or_else(|| ctx.log_format.as_deref().and_then(LogFormat::parse_str))
            .unwrap_or(LogFormat::Text);
        // Fallible init: tracing subscriber can only be set once per
        // process. Ignore re-init errors so tests that call main()
        // multiple times in-process don't trip the global-default trap.
        match format {
            LogFormat::Text => {
                let _ = tracing_subscriber::fmt()
                    .with_writer(std::io::stderr)
                    .with_env_filter(level)
                    .try_init();
            }
            LogFormat::Json => {
                let _ = tracing_subscriber::fmt()
                    .with_writer(std::io::stderr)
                    .with_env_filter(level)
                    .json()
                    .try_init();
            }
        }
    }

    let result = match cli.command {
        // `events stream` writes JSONL to stdout and MUST bypass the
        // Envelope wrapper — otherwise the trailing envelope would
        // pollute the JSONL contract. Early return with its own exit.
        Command::Events(cmd::events::Command::Stream(args)) => {
            return cmd::events::stream::run(&ctx, args).await;
        }
        Command::Init(a) => cmd::init::run(&ctx, a).await,
        Command::Me(a) => cmd::me::run(&ctx, a).await,
        Command::Ping(a) => cmd::ping::run(&ctx, a).await,
        Command::Task(c) => cmd::task::run(&ctx, c).await,
        Command::Bid(c) => cmd::bid::run(&ctx, c).await,
        Command::Post(a) => cmd::post::run(&ctx, a).await,
        Command::Settle(a) => cmd::settle::run(&ctx, a).await,
        Command::Escrow(c) => cmd::escrow::run(&ctx, c).await,
        Command::Events(c) => cmd::events::run(&ctx, c).await,
        Command::Webhook(c) => cmd::webhook::run(&ctx, c).await,
        Command::Discover(a) => cmd::discover::run(&ctx, a).await,
        Command::Artifact(c) => cmd::artifact::run(&ctx, c).await,
        Command::Message(c) => cmd::message::run(&ctx, c).await,
        Command::Review(c) => cmd::review::run(&ctx, c).await,
        Command::Payment(c) => cmd::payment::run(&ctx, c).await,
        Command::Dispute(a) => cmd::dispute::run(&ctx, a).await,
        Command::Agent(c) => cmd::agent::run(&ctx, c).await,
        Command::Platform(c) => cmd::platform::run(&ctx, c).await,
        Command::Wallet(c) => cmd::wallet::run(&ctx, c).await,
        Command::Config(c) => cmd::config::run(&ctx, c).await,
        Command::Skills(a) => cmd::skills::run(&ctx, a).await,
    };

    match result {
        Ok(env) => {
            if !ctx.quiet {
                env.with_warnings(ctx.security_warnings()).emit();
            }
            exit::ExitCode::Success.into()
        }
        Err(e) => {
            if !ctx.quiet {
                Envelope::error(ctx.environment, ctx.dry_run, &e)
                    .with_warnings(ctx.security_warnings())
                    .emit();
            }
            e.exit_code().into()
        }
    }
}
