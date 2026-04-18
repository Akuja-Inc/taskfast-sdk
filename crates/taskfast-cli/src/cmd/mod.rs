//! Subcommand module tree + shared [`Ctx`] / [`CmdError`] types.
//!
//! The taxonomy here is the CLI's stable, orchestrator-visible surface:
//!
//!   * `CmdError` codes (the short strings in the JSON envelope)
//!   * `ExitCode` bucket per variant
//!
//! Both are covered by tests at the bottom of this file so a refactor that
//! silently re-homes a variant will break the build.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use rust_decimal::Decimal;
use thiserror::Error;

use crate::config::{Config, ConfigError};
use crate::envelope::Envelope;
use crate::exit::ExitCode;
use crate::Environment;

use taskfast_agent::keystore::KeystoreError;
use taskfast_chains::tempo::SigningError;
use taskfast_client::{Error as ClientError, TaskFastClient};

pub mod agent;
pub mod artifact;
pub mod bid;
pub mod config;
pub mod discover;
pub mod dispute;
pub mod escrow;
pub mod events;
pub mod init;
pub mod init_tui;
pub mod me;
pub mod message;
pub mod payment;
pub mod ping;
pub mod platform;
pub mod post;
pub mod review;
pub mod settle;
pub mod task;
pub mod wallet;
pub mod wallet_args;
pub mod webhook;

/// Shared invocation context threaded through every subcommand.
///
/// Built once in `main` from parsed global flags; subcommands only read.
#[derive(Clone)]
pub struct Ctx {
    pub api_key: Option<String>,
    pub environment: Environment,
    /// Explicit `--api-base` / `TASKFAST_API` override. Wins over
    /// [`Environment::default_base_url`] when set.
    pub api_base: Option<String>,
    /// Resolved path to the JSON config file (default
    /// `./.taskfast/config.json`, override via `--config` /
    /// `TASKFAST_CONFIG`). Subcommands that persist state (init,
    /// `config set`, etc.) write here; tests construct this directly.
    pub config_path: PathBuf,
    /// Poster wallet address from config — fallback when no flag/env set
    /// on `post`/`settle`/`escrow sign`. Persisted by `taskfast init`.
    pub wallet_address: Option<String>,
    /// Keystore path from config — fallback for the same trio.
    pub keystore_path: Option<PathBuf>,
    /// Stablecoin-units threshold above which mutating commands
    /// (`post`, `settle`) require an explicit `--yes`. `None` = gate
    /// disabled. Set via `confirm_above_budget` in the JSON config.
    pub confirm_above_budget: Option<String>,
    /// Default log format for `--verbose` output. `None` = text. Set
    /// via `log_format` in the JSON config or `TASKFAST_LOG_FORMAT` env.
    pub log_format: Option<String>,
    /// Poster approval-deadline horizon for `escrow sign`. Parsed from
    /// the config file's `approval_horizon` at startup (fail-fast on
    /// malformed values). `None` = built-in default (7 days). The
    /// `--approval-horizon` flag / `TASKFAST_APPROVAL_HORIZON` env
    /// still win over this.
    pub approval_horizon: Option<Duration>,
    /// Receipt-polling ceiling for `escrow sign`. Parsed from the
    /// config file's `receipt_timeout` at startup. `None` = network-aware
    /// default (3min mainnet, 1min testnet). `--receipt-timeout` /
    /// `TASKFAST_RECEIPT_TIMEOUT` still win over this.
    pub receipt_timeout: Option<Duration>,
    pub dry_run: bool,
    pub quiet: bool,
}

/// Default environment when neither a flag nor the config file pins one.
pub const DEFAULT_ENVIRONMENT: Environment = Environment::Prod;

impl Default for Ctx {
    fn default() -> Self {
        Self {
            api_key: None,
            environment: DEFAULT_ENVIRONMENT,
            api_base: None,
            config_path: PathBuf::from("/dev/null"),
            wallet_address: None,
            keystore_path: None,
            confirm_above_budget: None,
            log_format: None,
            approval_horizon: None,
            receipt_timeout: None,
            dry_run: false,
            quiet: false,
        }
    }
}

impl Ctx {
    /// Build a [`Ctx`] by layering CLI flags (including clap's
    /// env-var folding) over the on-disk [`Config`]. Precedence:
    ///
    /// ```text
    /// flag > env var > config file > default
    /// ```
    ///
    /// Clap already folds `flag > env var` for each field (via
    /// `#[arg(env = "…")]`), so the merge here is just "cli wins, else
    /// config, else default". `dry_run` and `quiet` are never
    /// persisted — they're invocation-scoped.
    pub fn from_parts(
        cli_api_key: Option<String>,
        cli_env: Option<Environment>,
        cli_api_base: Option<String>,
        cli_config_path: Option<PathBuf>,
        cli_dry_run: bool,
        cli_quiet: bool,
        cfg: &Config,
    ) -> Result<Self, CmdError> {
        let approval_horizon =
            parse_duration_cfg(cfg.approval_horizon.as_deref(), "approval_horizon")?;
        let receipt_timeout =
            parse_duration_cfg(cfg.receipt_timeout.as_deref(), "receipt_timeout")?;
        Ok(Self {
            api_key: cli_api_key.or_else(|| cfg.api_key.clone()),
            environment: cli_env.or(cfg.environment).unwrap_or(DEFAULT_ENVIRONMENT),
            api_base: cli_api_base.or_else(|| cfg.api_base.clone()),
            config_path: cli_config_path.unwrap_or_else(Config::default_path),
            wallet_address: cfg.wallet_address.clone(),
            keystore_path: cfg.keystore_path.clone(),
            confirm_above_budget: cfg.confirm_above_budget.clone(),
            log_format: cfg.log_format.clone(),
            approval_horizon,
            receipt_timeout,
            dry_run: cli_dry_run,
            quiet: cli_quiet,
        })
    }

    /// Resolved API base URL: override if set, else env default.
    pub fn base_url(&self) -> &str {
        match self.api_base.as_deref() {
            Some(u) => u,
            None => self.environment.default_base_url(),
        }
    }

    /// Build an authenticated client, or fail with [`CmdError::MissingApiKey`]
    /// if no key was supplied (via `--api-key` or `TASKFAST_API_KEY`).
    pub fn client(&self) -> Result<TaskFastClient, CmdError> {
        let key = self.api_key.as_deref().ok_or(CmdError::MissingApiKey)?;
        TaskFastClient::from_api_key(self.base_url(), key).map_err(CmdError::from)
    }

    /// Fail-closed budget gate. When `confirm_above_budget` is set in the
    /// config, any mutation whose budget exceeds it must be opted into via
    /// `--yes`. By design there's no TTY prompt — automation-first stays
    /// intact; the gate just stops a fat-finger script before it broadcasts
    /// an oversized ERC-20 approve. `verb` is the action ("post a task",
    /// "settle this task") for the error message.
    pub fn enforce_budget_gate(
        &self,
        budget: Option<&str>,
        yes: bool,
        verb: &str,
    ) -> Result<(), CmdError> {
        let threshold_str = match self.confirm_above_budget.as_deref() {
            Some(t) => t,
            None => return Ok(()),
        };
        let budget_str = match budget {
            Some(b) => b,
            None => return Ok(()),
        };
        // Decimal (not f64): budget values are decimal strings like "0.1"
        // that f64 cannot represent exactly. An f64 compare could flip the
        // gate around an exact-boundary threshold, which defeats the whole
        // point of a safety rail against oversized approvals.
        let threshold = Decimal::from_str(threshold_str).map_err(|_| {
            CmdError::Usage(format!(
                "config `confirm_above_budget` is not a decimal: {threshold_str:?}"
            ))
        })?;
        let amount = Decimal::from_str(budget_str)
            .map_err(|_| CmdError::Usage(format!("budget {budget_str:?} is not a decimal")))?;
        if amount > threshold && !yes {
            return Err(CmdError::Usage(format!(
                "refusing to {verb}: budget {amount} exceeds confirm_above_budget \
                 {threshold} (pass --yes to override)"
            )));
        }
        Ok(())
    }
}

/// Parse a human-readable duration string from the config file,
/// wrapping a malformed value in [`CmdError::Usage`] that names the
/// field. Startup-time parsing lets a bad `approval_horizon: "7xyz"`
/// surface before any on-chain operation instead of mid-`escrow sign`.
fn parse_duration_cfg(
    raw: Option<&str>,
    field: &'static str,
) -> Result<Option<Duration>, CmdError> {
    match raw {
        None => Ok(None),
        Some(s) => humantime::parse_duration(s).map(Some).map_err(|e| {
            CmdError::Usage(format!("config `{field}` is not a duration: {s:?} ({e})"))
        }),
    }
}

/// Resolve a duration with the CLI precedence: flag > Ctx (config) > default.
/// Pulled out so #9 (`approval_horizon`) and #10 (`receipt_timeout`) share one
/// pinning path — and tests exercise the precedence once, not per command.
pub fn resolve_duration(
    flag: Option<Duration>,
    ctx_field: Option<Duration>,
    default: Duration,
) -> Duration {
    flag.or(ctx_field).unwrap_or(default)
}

pub type CmdResult = Result<Envelope, CmdError>;

/// CLI-layer error. Every variant maps to a stable `code` string (in the
/// envelope) and a stable [`ExitCode`] bucket — both are part of the
/// orchestrator contract.
#[derive(Debug, Error)]
pub enum CmdError {
    #[error("missing API key: set --api-key or TASKFAST_API_KEY")]
    MissingApiKey,

    #[error("usage: {0}")]
    Usage(String),

    #[error("auth: {0}")]
    Auth(String),

    #[error("rate limited (retry in {retry_after:?})")]
    RateLimited { retry_after: Duration },

    #[error("validation [{code}]: {message}")]
    Validation { code: String, message: String },

    #[error("server: {0}")]
    Server(String),

    #[error("network: {0}")]
    Network(String),

    #[error("decode: {0}")]
    Decode(String),

    #[error("keystore: {0}")]
    Keystore(String),

    #[error("signing: {0}")]
    Signing(String),

    #[error("not yet implemented: {0}")]
    Unimplemented(&'static str),

    #[error(transparent)]
    Config(#[from] ConfigError),
}

impl CmdError {
    /// Short, stable code string for the JSON envelope's `error.code` field.
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingApiKey => "missing_api_key",
            Self::Usage(_) => "usage",
            Self::Auth(_) => "auth",
            Self::RateLimited { .. } => "rate_limited",
            Self::Validation { .. } => "validation",
            Self::Server(_) => "server",
            Self::Network(_) => "network",
            Self::Decode(_) => "decode",
            Self::Keystore(_) => "keystore",
            Self::Signing(_) => "signing",
            Self::Unimplemented(_) => "unimplemented",
            Self::Config(_) => "config",
        }
    }

    /// Stable exit-code bucket — see [`ExitCode`] docstring for the taxonomy.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::MissingApiKey | Self::Usage(_) | Self::Config(_) => ExitCode::Usage,
            Self::Auth(_) => ExitCode::Auth,
            Self::RateLimited { .. } => ExitCode::RateLimited,
            Self::Validation { .. } => ExitCode::Validation,
            Self::Server(_) | Self::Network(_) | Self::Decode(_) => ExitCode::Server,
            Self::Keystore(_) | Self::Signing(_) => ExitCode::Wallet,
            Self::Unimplemented(_) => ExitCode::Unimplemented,
        }
    }

    /// Server-directed sleep hint, if any. Populated only for
    /// [`Self::RateLimited`] so orchestrators can read it directly from the
    /// envelope instead of parsing the message.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => Some(*retry_after),
            _ => None,
        }
    }
}

impl From<ClientError> for CmdError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Auth(m) => Self::Auth(m),
            ClientError::Validation { code, message } => Self::Validation { code, message },
            ClientError::RateLimited { retry_after } => Self::RateLimited { retry_after },
            ClientError::Server(m) => Self::Server(m),
            ClientError::Network(e) => Self::Network(e.to_string()),
            ClientError::Decode(e) => Self::Decode(e.to_string()),
        }
    }
}

impl From<KeystoreError> for CmdError {
    fn from(e: KeystoreError) -> Self {
        Self::Keystore(e.to_string())
    }
}

impl From<SigningError> for CmdError {
    fn from(e: SigningError) -> Self {
        Self::Signing(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn sample(variant: &str) -> CmdError {
        match variant {
            "missing_api_key" => CmdError::MissingApiKey,
            "usage" => CmdError::Usage("bad flag".into()),
            "auth" => CmdError::Auth("401".into()),
            "rate_limited" => CmdError::RateLimited {
                retry_after: Duration::from_secs(30),
            },
            "validation" => CmdError::Validation {
                code: "bad_field".into(),
                message: "x".into(),
            },
            "server" => CmdError::Server("500".into()),
            "network" => CmdError::Network("dns".into()),
            "decode" => CmdError::Decode("json".into()),
            "keystore" => CmdError::Keystore("bad pw".into()),
            "signing" => CmdError::Signing("hsm".into()),
            "unimplemented" => CmdError::Unimplemented("soon"),
            "config" => CmdError::Config(ConfigError::Io {
                path: PathBuf::from("/x"),
                source: std::io::Error::other("boom"),
            }),
            _ => unreachable!(),
        }
    }

    const ALL: &[&str] = &[
        "missing_api_key",
        "usage",
        "auth",
        "rate_limited",
        "validation",
        "server",
        "network",
        "decode",
        "keystore",
        "signing",
        "unimplemented",
        "config",
    ];

    #[test]
    fn every_variant_has_distinct_code() {
        let codes: HashSet<&'static str> = ALL.iter().map(|v| sample(v).code()).collect();
        assert_eq!(codes.len(), ALL.len(), "codes must be unique per variant");
        for v in ALL {
            assert_eq!(sample(v).code(), *v, "code() for {v} must match the label");
        }
    }

    #[test]
    fn exit_code_taxonomy_matches_plan() {
        // Pinning here is intentional: changing any of these is a breaking
        // change to the orchestrator contract.
        assert_eq!(CmdError::MissingApiKey.exit_code(), ExitCode::Usage);
        assert_eq!(sample("usage").exit_code(), ExitCode::Usage);
        assert_eq!(sample("auth").exit_code(), ExitCode::Auth);
        assert_eq!(sample("rate_limited").exit_code(), ExitCode::RateLimited);
        assert_eq!(sample("validation").exit_code(), ExitCode::Validation);
        assert_eq!(sample("server").exit_code(), ExitCode::Server);
        assert_eq!(sample("network").exit_code(), ExitCode::Server);
        assert_eq!(sample("decode").exit_code(), ExitCode::Server);
        assert_eq!(sample("keystore").exit_code(), ExitCode::Wallet);
        assert_eq!(sample("signing").exit_code(), ExitCode::Wallet);
        assert_eq!(sample("unimplemented").exit_code(), ExitCode::Unimplemented);
        assert_eq!(sample("config").exit_code(), ExitCode::Usage);
    }

    #[test]
    fn client_error_folds_retry_after_into_cmd_error() {
        let ce = ClientError::RateLimited {
            retry_after: Duration::from_secs(42),
        };
        let cmd: CmdError = ce.into();
        match cmd {
            CmdError::RateLimited { retry_after } => {
                assert_eq!(retry_after, Duration::from_secs(42));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        // And the hint is available via the convenience accessor.
        assert_eq!(
            sample("rate_limited").retry_after(),
            Some(Duration::from_secs(30))
        );
        assert!(sample("auth").retry_after().is_none());
    }

    #[test]
    fn ctx_base_url_override_wins_over_environment_default() {
        let ctx = Ctx {
            api_key: None,
            environment: Environment::Prod,
            api_base: Some("http://localhost:9999".into()),
            config_path: PathBuf::from("/dev/null"),
            dry_run: false,
            quiet: false,
            ..Default::default()
        };
        assert_eq!(ctx.base_url(), "http://localhost:9999");
    }

    #[test]
    fn ctx_base_url_falls_back_to_environment_default() {
        for (env, expected) in [
            (Environment::Prod, "https://api.taskfast.app"),
            (Environment::Staging, "https://staging.api.taskfast.app"),
            (Environment::Local, "http://localhost:4000"),
        ] {
            let ctx = Ctx {
                api_key: None,
                environment: env,
                api_base: None,
                config_path: PathBuf::from("/dev/null"),
                dry_run: false,
                quiet: false,
                ..Default::default()
            };
            assert_eq!(ctx.base_url(), expected);
        }
    }

    #[test]
    fn ctx_client_errors_when_api_key_missing() {
        let ctx = Ctx {
            api_key: None,
            environment: Environment::Local,
            api_base: None,
            config_path: PathBuf::from("/dev/null"),
            dry_run: false,
            quiet: false,
            ..Default::default()
        };
        match ctx.client() {
            Err(CmdError::MissingApiKey) => {}
            Err(other) => panic!("expected MissingApiKey, got {other:?}"),
            Ok(_) => panic!("expected MissingApiKey, got Ok(client)"),
        }
    }

    #[test]
    fn ctx_client_builds_when_api_key_present() {
        let ctx = Ctx {
            api_key: Some("tk_test_abc".into()),
            environment: Environment::Local,
            api_base: None,
            config_path: PathBuf::from("/dev/null"),
            dry_run: false,
            quiet: false,
            ..Default::default()
        };
        ctx.client().expect("client should build with a valid key");
    }

    fn cfg_with(
        api_key: Option<&str>,
        environment: Option<Environment>,
        api_base: Option<&str>,
    ) -> Config {
        Config {
            api_key: api_key.map(str::to_string),
            environment,
            api_base: api_base.map(str::to_string),
            ..Config::default()
        }
    }

    #[test]
    fn from_parts_threads_wallet_keystore_confirm_log_format_from_config() {
        let cfg = Config {
            wallet_address: Some("0xfeed".into()),
            keystore_path: Some(PathBuf::from("/tmp/k.json")),
            confirm_above_budget: Some("500".into()),
            log_format: Some("json".into()),
            ..Config::default()
        };
        let ctx = Ctx::from_parts(None, None, None, None, false, false, &cfg).expect("valid cfg");
        assert_eq!(ctx.wallet_address.as_deref(), Some("0xfeed"));
        assert_eq!(
            ctx.keystore_path.as_deref(),
            Some(std::path::Path::new("/tmp/k.json"))
        );
        assert_eq!(ctx.confirm_above_budget.as_deref(), Some("500"));
        assert_eq!(ctx.log_format.as_deref(), Some("json"));
    }

    #[test]
    fn from_parts_parses_approval_horizon_from_config() {
        let cfg = Config {
            approval_horizon: Some("7d".into()),
            ..Config::default()
        };
        let ctx = Ctx::from_parts(None, None, None, None, false, false, &cfg).expect("parse 7d");
        assert_eq!(ctx.approval_horizon, Some(Duration::from_hours(24 * 7)));
    }

    #[test]
    fn from_parts_parses_receipt_timeout_from_config() {
        let cfg = Config {
            receipt_timeout: Some("3min".into()),
            ..Config::default()
        };
        let ctx = Ctx::from_parts(None, None, None, None, false, false, &cfg).expect("parse 3min");
        assert_eq!(ctx.receipt_timeout, Some(Duration::from_mins(3)));
    }

    #[test]
    fn from_parts_rejects_malformed_approval_horizon() {
        let cfg = Config {
            approval_horizon: Some("7xyz".into()),
            ..Config::default()
        };
        let err = Ctx::from_parts(None, None, None, None, false, false, &cfg)
            .err()
            .expect("malformed config must fail at startup");
        match err {
            CmdError::Usage(msg) => {
                assert!(msg.contains("approval_horizon"), "msg: {msg}");
                assert!(msg.contains("7xyz"), "msg: {msg}");
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn from_parts_rejects_malformed_receipt_timeout() {
        let cfg = Config {
            receipt_timeout: Some("nope".into()),
            ..Config::default()
        };
        let err = Ctx::from_parts(None, None, None, None, false, false, &cfg)
            .err()
            .expect("malformed timeout = startup error");
        assert!(matches!(err, CmdError::Usage(msg) if msg.contains("receipt_timeout")));
    }

    #[test]
    fn from_parts_duration_fields_none_when_config_absent() {
        let ctx = Ctx::from_parts(None, None, None, None, false, false, &Config::default())
            .expect("default cfg is valid");
        assert!(ctx.approval_horizon.is_none());
        assert!(ctx.receipt_timeout.is_none());
    }

    #[test]
    fn resolve_duration_flag_wins_over_ctx_and_default() {
        let out = resolve_duration(
            Some(Duration::from_secs(10)),
            Some(Duration::from_secs(20)),
            Duration::from_secs(30),
        );
        assert_eq!(out, Duration::from_secs(10));
    }

    #[test]
    fn resolve_duration_ctx_wins_over_default_when_no_flag() {
        let out = resolve_duration(None, Some(Duration::from_secs(20)), Duration::from_secs(30));
        assert_eq!(out, Duration::from_secs(20));
    }

    #[test]
    fn resolve_duration_falls_back_to_default() {
        let out = resolve_duration(None, None, Duration::from_secs(30));
        assert_eq!(out, Duration::from_secs(30));
    }

    fn ctx_with_threshold(threshold: Option<&str>) -> Ctx {
        Ctx {
            confirm_above_budget: threshold.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn budget_gate_no_op_when_threshold_unset() {
        let ctx = ctx_with_threshold(None);
        ctx.enforce_budget_gate(Some("99999"), false, "post a task")
            .expect("no threshold = no gate");
    }

    #[test]
    fn budget_gate_no_op_when_budget_absent() {
        let ctx = ctx_with_threshold(Some("100"));
        ctx.enforce_budget_gate(None, false, "post a task")
            .expect("no budget = nothing to compare");
    }

    #[test]
    fn budget_gate_passes_when_under_threshold() {
        let ctx = ctx_with_threshold(Some("100"));
        ctx.enforce_budget_gate(Some("50"), false, "post a task")
            .expect("under threshold passes without --yes");
    }

    #[test]
    fn budget_gate_passes_at_threshold_boundary() {
        let ctx = ctx_with_threshold(Some("100"));
        ctx.enforce_budget_gate(Some("100"), false, "post a task")
            .expect("equal-to-threshold passes (gate is strict >)");
    }

    #[test]
    fn budget_gate_blocks_above_threshold_without_yes() {
        let ctx = ctx_with_threshold(Some("100"));
        let err = ctx
            .enforce_budget_gate(Some("100.01"), false, "post a task")
            .expect_err("over threshold without --yes must fail");
        match err {
            CmdError::Usage(msg) => {
                assert!(msg.contains("--yes"), "msg: {msg}");
                assert!(msg.contains("post a task"), "msg: {msg}");
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn budget_gate_passes_above_threshold_with_yes() {
        let ctx = ctx_with_threshold(Some("100"));
        ctx.enforce_budget_gate(Some("9999"), true, "post a task")
            .expect("--yes overrides the gate");
    }

    #[test]
    fn budget_gate_uses_decimal_not_float_precision() {
        // f64 parsing would round "0.30000000000000004" to 0.3 and let it
        // slip past a "0.3" threshold. Decimal preserves every digit so
        // the gate correctly blocks.
        let ctx = ctx_with_threshold(Some("0.3"));
        ctx.enforce_budget_gate(Some("0.30000000000000004"), false, "post a task")
            .expect_err("decimal compare must detect trailing precision");
    }

    #[test]
    fn budget_gate_rejects_non_decimal_threshold() {
        let ctx = ctx_with_threshold(Some("not-a-number"));
        let err = ctx
            .enforce_budget_gate(Some("50"), false, "post a task")
            .expect_err("garbage threshold = usage error");
        assert!(matches!(err, CmdError::Usage(_)));
    }

    #[test]
    fn from_parts_flag_wins_over_config() {
        let cfg = cfg_with(
            Some("cfg_key"),
            Some(Environment::Staging),
            Some("http://cfg"),
        );
        let ctx = Ctx::from_parts(
            Some("flag_key".into()),
            Some(Environment::Local),
            Some("http://flag".into()),
            None,
            false,
            false,
            &cfg,
        )
        .expect("valid cfg");
        assert_eq!(ctx.api_key.as_deref(), Some("flag_key"));
        assert_eq!(ctx.environment, Environment::Local);
        assert_eq!(ctx.api_base.as_deref(), Some("http://flag"));
    }

    #[test]
    fn from_parts_config_fills_when_flags_absent() {
        let cfg = cfg_with(
            Some("cfg_key"),
            Some(Environment::Staging),
            Some("http://cfg"),
        );
        let ctx = Ctx::from_parts(None, None, None, None, false, false, &cfg).expect("valid cfg");
        assert_eq!(ctx.api_key.as_deref(), Some("cfg_key"));
        assert_eq!(ctx.environment, Environment::Staging);
        assert_eq!(ctx.api_base.as_deref(), Some("http://cfg"));
    }

    #[test]
    fn from_parts_defaults_when_nothing_set() {
        let ctx = Ctx::from_parts(None, None, None, None, false, false, &Config::default())
            .expect("default cfg is valid");
        assert!(ctx.api_key.is_none());
        assert_eq!(ctx.environment, DEFAULT_ENVIRONMENT);
        assert!(ctx.api_base.is_none());
        assert!(!ctx.dry_run);
        assert!(!ctx.quiet);
    }

    #[test]
    fn from_parts_flag_partial_overrides_preserve_other_config_fields() {
        // Only `api_key` passed on the CLI — environment + api_base
        // should still come from the config file.
        let cfg = cfg_with(
            Some("cfg_key"),
            Some(Environment::Staging),
            Some("http://cfg"),
        );
        let ctx = Ctx::from_parts(
            Some("flag_key".into()),
            None,
            None,
            None,
            false,
            false,
            &cfg,
        )
        .expect("valid cfg");
        assert_eq!(ctx.api_key.as_deref(), Some("flag_key"));
        assert_eq!(ctx.environment, Environment::Staging);
        assert_eq!(ctx.api_base.as_deref(), Some("http://cfg"));
    }

    #[test]
    fn from_parts_dry_run_and_quiet_are_invocation_scoped() {
        // These never come from config, only from the CLI.
        let ctx = Ctx::from_parts(None, None, None, None, true, true, &Config::default())
            .expect("default cfg is valid");
        assert!(ctx.dry_run);
        assert!(ctx.quiet);
    }
}
