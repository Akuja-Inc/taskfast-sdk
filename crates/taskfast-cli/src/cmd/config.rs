//! `taskfast config` — inspect / edit the project-local JSON config.
//!
//! Three operations, each shaped around the orchestrator JSON envelope:
//!
//! * `show` — dump the config with `api_key` redacted to its last 4 chars.
//!   `--reveal` prints the full value (e.g. for piping into an env var in
//!   a one-shot).
//! * `path` — print the resolved config path. Useful for `jq -r
//!   .data.path` in a shell wrapper that needs to mount the file.
//! * `set <key> <value>` — mutate a single field. Keys are allowlisted so
//!   a typo can't smuggle garbage into the JSON; values are type-checked
//!   against the field (enum for `environment`, path for
//!   `keystore_path`, etc.).
//!
//! `taskfast init` and future writers share the same `Config::save` path,
//! so edits from `set` interleave cleanly with a re-`init`.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use serde_json::json;

use super::{CmdError, CmdResult, Ctx};
use crate::config::Config;
use crate::envelope::Envelope;
use crate::Environment;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the current config as JSON. `api_key` is redacted to
    /// `***<last4>` unless `--reveal` is passed.
    Show(ShowArgs),
    /// Print the resolved config path (respects `--config` /
    /// `TASKFAST_CONFIG`).
    Path,
    /// Set a single field in the config file. Allowlisted keys only.
    Set(SetArgs),
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Print `api_key` in full instead of the `***<last4>` redaction.
    /// Consider piping to a file you `chmod 600` immediately rather than
    /// letting the key sit in your shell history.
    #[arg(long)]
    pub reveal: bool,
}

#[derive(Debug, Args)]
pub struct SetArgs {
    /// Field name. One of: `environment`, `api_base`, `api_key`,
    /// `network`, `wallet_address`, `keystore_path`, `agent_id`,
    /// `webhook_url`, `webhook_secret_path`.
    pub key: String,
    /// New value. Pass `--unset` (below) or an empty string to clear.
    pub value: Option<String>,
    /// Clear the field (equivalent to passing an empty value).
    #[arg(long, conflicts_with = "value")]
    pub unset: bool,
}

// Dispatch is sync (no HTTP / no I/O await) but the signature matches the
// rest of the subcommands so `main.rs` can call `.await` uniformly.
#[allow(clippy::unused_async)]
pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Show(args) => show(ctx, args),
        Command::Path => Ok(path(ctx)),
        Command::Set(args) => set(ctx, args),
    }
}

fn show(ctx: &Ctx, args: ShowArgs) -> CmdResult {
    let cfg = Config::load(&ctx.config_path).map_err(|e| CmdError::Usage(e.to_string()))?;
    let data = json!({
        "path": ctx.config_path.display().to_string(),
        "config": serialize_for_display(&cfg, args.reveal),
    });
    Ok(Envelope::success(ctx.environment, ctx.dry_run, data))
}

fn path(ctx: &Ctx) -> Envelope {
    let data = json!({
        "path": ctx.config_path.display().to_string(),
        "exists": ctx.config_path.exists(),
    });
    Envelope::success(ctx.environment, ctx.dry_run, data)
}

fn set(ctx: &Ctx, args: SetArgs) -> CmdResult {
    let value = if args.unset {
        None
    } else {
        args.value
            .as_deref()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };

    let mut cfg = Config::load(&ctx.config_path).map_err(|e| CmdError::Usage(e.to_string()))?;

    let before = field_summary(&cfg, &args.key);
    apply_set(&mut cfg, &args.key, value.as_deref())?;
    let after = field_summary(&cfg, &args.key);

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "key": args.key,
                "before": before,
                "after": after,
                "written": false,
                "would_write": true,
                "path": ctx.config_path.display().to_string(),
            }),
        ));
    }

    cfg.save(&ctx.config_path)
        .map_err(|e| CmdError::Usage(e.to_string()))?;
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "key": args.key,
            "before": before,
            "after": after,
            "written": true,
            "path": ctx.config_path.display().to_string(),
        }),
    ))
}

/// Supported field names for `config set`. Kept as a constant so the
/// error message lists exactly what's allowed without drifting from the
/// match arm below.
const ALLOWED_KEYS: &[&str] = &[
    "environment",
    "api_base",
    "api_key",
    "network",
    "wallet_address",
    "keystore_path",
    "agent_id",
    "webhook_url",
    "webhook_secret_path",
];

fn apply_set(cfg: &mut Config, key: &str, value: Option<&str>) -> Result<(), CmdError> {
    match key {
        "environment" => {
            cfg.environment = match value {
                None => None,
                Some(v) => Some(parse_environment(v)?),
            };
        }
        "api_base" => cfg.api_base = value.map(str::to_string),
        "api_key" => cfg.api_key = value.map(str::to_string),
        "network" => cfg.network = value.map(str::to_string),
        "wallet_address" => cfg.wallet_address = value.map(str::to_string),
        "keystore_path" => cfg.keystore_path = value.map(PathBuf::from),
        "agent_id" => cfg.agent_id = value.map(str::to_string),
        "webhook_url" => cfg.webhook_url = value.map(str::to_string),
        "webhook_secret_path" => cfg.webhook_secret_path = value.map(PathBuf::from),
        _ => {
            return Err(CmdError::Usage(format!(
                "unknown config key `{key}`; allowed: {}",
                ALLOWED_KEYS.join(", ")
            )));
        }
    }
    Ok(())
}

fn parse_environment(s: &str) -> Result<Environment, CmdError> {
    match s {
        "prod" | "production" => Ok(Environment::Prod),
        "staging" => Ok(Environment::Staging),
        "local" => Ok(Environment::Local),
        other => Err(CmdError::Usage(format!(
            "unknown environment `{other}`; expected prod | staging | local"
        ))),
    }
}

/// JSON projection of a single field — used by `set` to surface the
/// before/after diff. Uses [`serialize_for_display`] under the hood so
/// an `api_key` summary stays redacted.
fn field_summary(cfg: &Config, key: &str) -> serde_json::Value {
    let body = serialize_for_display(cfg, false);
    body.get(key).cloned().unwrap_or(serde_json::Value::Null)
}

fn serialize_for_display(cfg: &Config, reveal: bool) -> serde_json::Value {
    let mut v = serde_json::to_value(cfg).unwrap_or_else(|_| json!({}));
    if !reveal {
        if let Some(obj) = v.as_object_mut() {
            if let Some(key) = obj.get_mut("api_key") {
                if let Some(s) = key.as_str() {
                    *key = serde_json::Value::String(redact(s));
                }
            }
        }
    }
    v
}

/// Mask all but the last 4 chars: `am_live_abcd1234` → `***1234`. Short
/// strings (≤4 chars) are fully masked so we don't leak the whole key.
fn redact(s: &str) -> String {
    let n = s.chars().count();
    if n <= 4 {
        return "***".to_string();
    }
    let tail: String = s.chars().skip(n - 4).collect();
    format!("***{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn ctx_with(path: PathBuf, dry_run: bool) -> Ctx {
        Ctx {
            api_key: None,
            environment: Environment::Local,
            api_base: None,
            config_path: path,
            dry_run,
            quiet: true,
            ..Default::default()
        }
    }

    #[test]
    fn redact_masks_all_but_last_4() {
        assert_eq!(redact("am_live_abcd1234"), "***1234");
        assert_eq!(redact("12345"), "***2345");
    }

    #[test]
    fn redact_short_strings_fully_masked() {
        assert_eq!(redact("abcd"), "***");
        assert_eq!(redact("xx"), "***");
        assert_eq!(redact(""), "***");
    }

    #[test]
    fn apply_set_unknown_key_errors_as_usage() {
        let mut cfg = Config::default();
        let err = apply_set(&mut cfg, "nope", Some("x")).unwrap_err();
        match err {
            CmdError::Usage(msg) => assert!(msg.contains("nope"), "{msg}"),
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn apply_set_environment_enum_is_validated() {
        let mut cfg = Config::default();
        assert!(apply_set(&mut cfg, "environment", Some("local")).is_ok());
        assert_eq!(cfg.environment, Some(Environment::Local));
        let err = apply_set(&mut cfg, "environment", Some("moon")).unwrap_err();
        assert!(matches!(err, CmdError::Usage(_)));
    }

    #[test]
    fn apply_set_empty_value_clears_field() {
        let mut cfg = Config {
            api_base: Some("http://x".into()),
            ..Config::default()
        };
        apply_set(&mut cfg, "api_base", None).unwrap();
        assert!(cfg.api_base.is_none());
    }

    #[test]
    fn apply_set_keystore_path_accepts_path() {
        let mut cfg = Config::default();
        apply_set(&mut cfg, "keystore_path", Some("/tmp/k.json")).unwrap();
        assert_eq!(cfg.keystore_path, Some(PathBuf::from("/tmp/k.json")));
    }

    #[tokio::test]
    async fn show_redacts_api_key_by_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        Config {
            api_key: Some("am_live_secret0123".into()),
            ..Config::default()
        }
        .save(&path)
        .unwrap();

        let env = run(
            &ctx_with(path.clone(), false),
            Command::Show(ShowArgs { reveal: false }),
        )
        .await
        .unwrap();
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["data"]["config"]["api_key"], "***0123");
    }

    #[tokio::test]
    async fn show_reveal_prints_api_key_in_full() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        Config {
            api_key: Some("am_live_secret0123".into()),
            ..Config::default()
        }
        .save(&path)
        .unwrap();

        let env = run(
            &ctx_with(path, false),
            Command::Show(ShowArgs { reveal: true }),
        )
        .await
        .unwrap();
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["data"]["config"]["api_key"], "am_live_secret0123");
    }

    #[tokio::test]
    async fn path_reports_existence() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("config.json");
        let env = run(&ctx_with(missing.clone(), false), Command::Path)
            .await
            .unwrap();
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["data"]["exists"], false);

        Config::default().save(&missing).unwrap();
        let env = run(&ctx_with(missing, false), Command::Path).await.unwrap();
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["data"]["exists"], true);
    }

    #[tokio::test]
    async fn set_writes_and_persists() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".taskfast").join("config.json");
        let env = run(
            &ctx_with(path.clone(), false),
            Command::Set(SetArgs {
                key: "network".into(),
                value: Some("testnet".into()),
                unset: false,
            }),
        )
        .await
        .unwrap();
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["data"]["written"], true);
        assert_eq!(v["data"]["after"], "testnet");

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.network.as_deref(), Some("testnet"));
    }

    #[tokio::test]
    async fn set_dry_run_does_not_persist() {
        let tmp = TempDir::new().unwrap();
        // Nest under `.taskfast/` so the migration-path grandparent
        // lookup lands inside tmp (not `/tmp` on the host).
        let path = tmp.path().join(".taskfast").join("config.json");
        let env = run(
            &ctx_with(path.clone(), true),
            Command::Set(SetArgs {
                key: "network".into(),
                value: Some("testnet".into()),
                unset: false,
            }),
        )
        .await
        .unwrap();
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["data"]["written"], false);
        assert_eq!(v["data"]["would_write"], true);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn set_unset_clears_the_field() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        Config {
            network: Some("mainnet".into()),
            ..Config::default()
        }
        .save(&path)
        .unwrap();

        run(
            &ctx_with(path.clone(), false),
            Command::Set(SetArgs {
                key: "network".into(),
                value: None,
                unset: true,
            }),
        )
        .await
        .unwrap();

        let loaded = Config::load(&path).unwrap();
        assert!(loaded.network.is_none());
    }

    #[tokio::test]
    async fn set_api_key_shows_redacted_after_in_envelope() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        let env = run(
            &ctx_with(path, false),
            Command::Set(SetArgs {
                key: "api_key".into(),
                value: Some("am_live_supersecret1234".into()),
                unset: false,
            }),
        )
        .await
        .unwrap();
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["data"]["after"], "***1234");
    }
}
