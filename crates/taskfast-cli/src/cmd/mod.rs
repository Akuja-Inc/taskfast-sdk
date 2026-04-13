//! Subcommand module tree + shared [`Ctx`] / [`CmdError`] types.
//!
//! The taxonomy here is the CLI's stable, orchestrator-visible surface:
//!
//!   * `CmdError` codes (the short strings in the JSON envelope)
//!   * `ExitCode` bucket per variant
//!
//! Both are covered by tests at the bottom of this file so a refactor that
//! silently re-homes a variant will break the build.

use std::time::Duration;

use thiserror::Error;

use crate::Environment;
use crate::envelope::Envelope;
use crate::exit::ExitCode;

use taskfast_agent::keystore::KeystoreError;
use taskfast_agent::signing::SigningError;
use taskfast_client::{Error as ClientError, TaskFastClient};

pub mod bid;
pub mod events;
pub mod init;
pub mod me;
pub mod post;
pub mod settle;
pub mod task;

/// Shared invocation context threaded through every subcommand.
///
/// Built once in `main` from parsed global flags; subcommands only read.
pub struct Ctx {
    pub api_key: Option<String>,
    pub environment: Environment,
    /// Explicit `--api-base` / `TASKFAST_API` override. Wins over
    /// [`Environment::default_base_url`] when set.
    pub api_base: Option<String>,
    pub dry_run: bool,
    pub quiet: bool,
}

impl Ctx {
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
        }
    }

    /// Stable exit-code bucket — see [`ExitCode`] docstring for the taxonomy.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::MissingApiKey | Self::Usage(_) => ExitCode::Usage,
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
            dry_run: false,
            quiet: false,
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
                dry_run: false,
                quiet: false,
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
            dry_run: false,
            quiet: false,
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
            dry_run: false,
            quiet: false,
        };
        ctx.client().expect("client should build with a valid key");
    }
}
