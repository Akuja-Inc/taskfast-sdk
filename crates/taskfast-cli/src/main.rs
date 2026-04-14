//! `taskfast` binary entry point.
//!
//! Global flag parsing + dispatch to [`cmd`] modules. Every subcommand
//! returns `Result<Envelope, CmdError>`; we print the envelope (unless
//! `--quiet`) and exit with the matching code from [`exit::ExitCode`].

// TODO: tighten doc coverage on public items + remove this allow.
#![allow(missing_docs)]

use clap::{Parser, Subcommand};

use taskfast_cli::{cmd, exit, Envelope, Environment};

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

    /// Target environment.
    #[arg(long, global = true, default_value = "prod", env = "TASKFAST_ENV")]
    env: Environment,

    /// Override the resolved base URL (bypasses env → URL mapping). Useful
    /// for pointing at a local dev server without touching prod defaults.
    #[arg(long, global = true, env = "TASKFAST_API")]
    api_base: Option<String>,

    /// Short-circuit mutations; reads pass through.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Emit tracing logs to stderr. Accepts an `env_logger`-style filter
    /// (e.g. `--verbose=debug`, `--verbose=taskfast_client=trace`).
    #[arg(long, global = true, value_name = "LEVEL", num_args = 0..=1, default_missing_value = "info")]
    verbose: Option<String>,

    /// Suppress even the error envelope (exit code still conveys outcome).
    #[arg(long, global = true, conflicts_with = "verbose")]
    quiet: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Bootstrap an agent: deps, wallet, webhook, funding.
    Init(cmd::init::Args),
    /// Profile + readiness (GET /agents/me + /agents/me/readiness).
    Me(cmd::me::Args),
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
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    if let Some(level) = cli.verbose.as_deref() {
        // Fallible init: tracing subscriber can only be set once per
        // process. Ignore re-init errors so tests that call main()
        // multiple times in-process don't trip the global-default trap.
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(level)
            .try_init();
    }

    let ctx = cmd::Ctx {
        api_key: cli.api_key,
        environment: cli.env,
        api_base: cli.api_base,
        dry_run: cli.dry_run,
        quiet: cli.quiet,
    };

    let result = match cli.command {
        Command::Init(a) => cmd::init::run(&ctx, a).await,
        Command::Me(a) => cmd::me::run(&ctx, a).await,
        Command::Task(c) => cmd::task::run(&ctx, c).await,
        Command::Bid(c) => cmd::bid::run(&ctx, c).await,
        Command::Post(a) => cmd::post::run(&ctx, a).await,
        Command::Settle(a) => cmd::settle::run(&ctx, a).await,
        Command::Escrow(c) => cmd::escrow::run(&ctx, c).await,
        Command::Events(c) => cmd::events::run(&ctx, c).await,
        Command::Webhook(c) => cmd::webhook::run(&ctx, c).await,
    };

    match result {
        Ok(env) => {
            if !ctx.quiet {
                env.emit();
            }
            exit::ExitCode::Success.into()
        }
        Err(e) => {
            if !ctx.quiet {
                Envelope::error(ctx.environment, ctx.dry_run, &e).emit();
            }
            e.exit_code().into()
        }
    }
}
