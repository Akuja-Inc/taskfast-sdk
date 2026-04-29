//! JSON config file for the `taskfast` CLI.
//!
//! Persistent state written by `taskfast init` (and edited via
//! `taskfast config set`) so subsequent subcommands work in a fresh
//! shell without sourcing anything.
//!
//! # Layout
//!
//! Default path: `./.taskfast/config.json` (project-local). Override via
//! the global `--config <path>` flag or `TASKFAST_CONFIG` env var.
//!
//! # Precedence
//!
//! `Ctx::from_cli_and_config` (see `cmd/mod.rs`) layers values as:
//!
//! ```text
//! clap flag > env var > config file > default
//! ```
//!
//! Clap already folds flag > env via `env = "..."`. The config file sits
//! one rung under that, so omitting a field in JSON never surprises a
//! caller who passed the flag.
//!
//! # Forward-compat
//!
//! `schema_version` is a `u32` that starts at `CURRENT_SCHEMA_VERSION`.
//! Unknown fields are tolerated (default serde behaviour). A file with a
//! newer `schema_version` logs a warning and loads what it recognises —
//! it does not fail, so an older CLI doesn't brick a newer config dir.
//!
//! # Secrets
//!
//! `api_key` lives in this file. The file is written mode `0600` on unix
//! (atomic temp + rename) and the containing `.taskfast/` directory is
//! intended to be git-ignored.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Environment;

/// Default project-local path, relative to the CWD.
pub const DEFAULT_CONFIG_PATH: &str = ".taskfast/config.json";

/// Current on-disk schema version. Bump when a field changes shape in a
/// way a reader needs to special-case; additive fields don't need a bump
/// because serde `#[serde(default)]` already handles missing keys.
///
/// v2: dropped `api_base` and `network` — both derived from `environment`
/// at runtime via [`crate::Environment::api_base`] and
/// [`crate::Environment::network`]. Files at v1 with either key present
/// hard-error in [`Config::load`] with a migration hint.
pub const CURRENT_SCHEMA_VERSION: u32 = 2;

/// Keys removed at schema v2. Loading a file that still carries any of
/// these triggers [`ConfigError::LegacyFields`] so a stale config never
/// silently misroutes traffic post-migration.
const LEGACY_REMOVED_KEYS: &[&str] = &["api_base", "network"];

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("I/O on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "config {path} carries removed key(s) {fields:?} — derived from `environment` since \
         schema v2. Run `taskfast config migrate` (or remove the keys + bump `schema_version` \
         to 2 manually) and re-run."
    )]
    LegacyFields { path: PathBuf, fields: Vec<String> },
}

/// On-disk config. Every runtime field is `Option` so the file stays
/// small and a partially-configured project can still round-trip.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    /// On-disk schema version (`CURRENT_SCHEMA_VERSION` for fresh
    /// writes). A `0` value means "not set" and is normalized to
    /// `CURRENT_SCHEMA_VERSION` on save.
    #[serde(skip_serializing_if = "is_zero")]
    pub schema_version: u32,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<Environment>,

    /// Agent API key. Secret — the file is written `0600`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet_address: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub keystore_path: Option<PathBuf>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_secret_path: Option<PathBuf>,

    /// Fail-closed confirmation gate for mutating commands. When the
    /// requested budget on `post` (or task budget on `settle`) exceeds
    /// this stablecoin-units threshold, the command refuses to proceed
    /// without an explicit `--yes`. Decimal string in the same units
    /// as `--budget` (e.g. `"1000"` = 1000 USDC). `None` = gate off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_above_budget: Option<String>,

    /// Default `--verbose` log encoding. Accepts `"json"` or `"text"`.
    /// `None` = `"text"`. CLI flag and env var (`TASKFAST_LOG_FORMAT`)
    /// still win over this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_format: Option<String>,

    /// Poster approval deadline for `escrow sign`. Human-readable
    /// duration string (e.g. `"7d"`, `"24h"`). `None` = built-in
    /// default (7 days). Flag `--approval-horizon` and env var
    /// `TASKFAST_APPROVAL_HORIZON` still win over this. Malformed
    /// values are rejected at CLI startup, not mid-escrow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_horizon: Option<String>,

    /// Receipt-polling ceiling for `escrow sign`. Human-readable
    /// duration. `None` = network-aware default (3min mainnet,
    /// 1min testnet). Flag `--receipt-timeout` and env var
    /// `TASKFAST_RECEIPT_TIMEOUT` still win over this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_timeout: Option<String>,
}

// `skip_serializing_if` is required by serde to take `&T`, so clippy's
// pass-by-value lint is a false positive here.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(v: &u32) -> bool {
    *v == 0
}

impl Config {
    /// Parse the JSON at `path`. Missing file → `Config::default()`
    /// (callers treat absence the same as an empty config). Newer
    /// `schema_version` values log a warning via `tracing::warn!` and
    /// load what they recognise.
    ///
    /// Hard-errors with [`ConfigError::LegacyFields`] when the on-disk
    /// JSON still carries any key removed at the current schema version
    /// (currently `api_base` and `network`). Stale values would otherwise
    /// silently outrank the `Environment`-derived defaults.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let src = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        // Peek raw JSON before strict deserialization so the migration error
        // wins over a generic parse error and so removed-key detection does
        // not depend on `Config` carrying the field.
        let raw: serde_json::Value =
            serde_json::from_str(&src).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        if let Some(obj) = raw.as_object() {
            let legacy: Vec<String> = LEGACY_REMOVED_KEYS
                .iter()
                .filter(|k| obj.contains_key(**k))
                .map(|k| (*k).to_string())
                .collect();
            if !legacy.is_empty() {
                return Err(ConfigError::LegacyFields {
                    path: path.to_path_buf(),
                    fields: legacy,
                });
            }
        }
        let cfg: Config = serde_json::from_value(raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        if cfg.schema_version > CURRENT_SCHEMA_VERSION {
            tracing::warn!(
                file = %path.display(),
                file_version = cfg.schema_version,
                current_version = CURRENT_SCHEMA_VERSION,
                "config schema_version is newer than this CLI — loading recognised fields only"
            );
        }
        Ok(cfg)
    }

    /// Atomic(ish) write: JSON-serialize to a sibling `.tmp`, chmod
    /// `0600` on unix, rename into place. Creates the parent directory
    /// if it doesn't exist.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
                // F4: tighten the `.taskfast/` dir to 0700. `create_dir_all`
                // honors umask (typically 022 → 0755), leaving the keystore
                // + webhook secret + API key config readable by anyone on
                // the host. 0700 scopes them to the owning UID. Idempotent
                // on subsequent saves.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let perms = fs::Permissions::from_mode(0o700);
                    fs::set_permissions(parent, perms).map_err(|source| ConfigError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                }
            }
        }
        let mut to_write = self.clone();
        if to_write.schema_version == 0 {
            to_write.schema_version = CURRENT_SCHEMA_VERSION;
        }
        let body = serde_json::to_vec_pretty(&to_write).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

        let tmp = path.with_extension("json.tmp");
        {
            let mut f = fs::File::create(&tmp).map_err(|source| ConfigError::Io {
                path: tmp.clone(),
                source,
            })?;
            f.write_all(&body).map_err(|source| ConfigError::Io {
                path: tmp.clone(),
                source,
            })?;
            f.write_all(b"\n").map_err(|source| ConfigError::Io {
                path: tmp.clone(),
                source,
            })?;
            f.flush().map_err(|source| ConfigError::Io {
                path: tmp.clone(),
                source,
            })?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(&tmp, perms).map_err(|source| ConfigError::Io {
                path: tmp.clone(),
                source,
            })?;
        }
        fs::rename(&tmp, path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(())
    }

    /// Default file path, rooted in the caller's current working
    /// directory. Used when neither `--config` nor `TASKFAST_CONFIG`
    /// is supplied.
    pub fn default_path() -> PathBuf {
        PathBuf::from(DEFAULT_CONFIG_PATH)
    }
}

// Serde support for `Environment` — declared here (not in lib.rs) so the
// config module owns its serialization contract. If the enum ever grows
// a runtime variant that shouldn't be persisted, the mapping stays
// local.
impl Serialize for Environment {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(match self {
            Self::Prod => "prod",
            Self::Staging => "staging",
            Self::Local => "local",
        })
    }
}

impl<'de> Deserialize<'de> for Environment {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        match s.as_str() {
            "prod" | "production" => Ok(Self::Prod),
            "staging" => Ok(Self::Staging),
            "local" => Ok(Self::Local),
            other => Err(serde::de::Error::custom(format!(
                "unknown environment {other:?}; expected prod | staging | local"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample() -> Config {
        Config {
            schema_version: CURRENT_SCHEMA_VERSION,
            environment: Some(Environment::Local),
            api_key: Some("am_live_secret".into()),
            wallet_address: Some("0xabc".into()),
            keystore_path: Some(PathBuf::from("/tmp/keystore.json")),
            agent_id: Some("agent_123".into()),
            webhook_url: Some("https://example.com/hook".into()),
            webhook_secret_path: Some(PathBuf::from("/tmp/hook.secret")),
            confirm_above_budget: Some("1000".into()),
            log_format: Some("json".into()),
            approval_horizon: Some("7d".into()),
            receipt_timeout: Some("3min".into()),
        }
    }

    #[test]
    fn load_missing_file_returns_default() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope.json");
        let cfg = Config::load(&missing).expect("missing file is default, not error");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn roundtrip_preserves_every_field() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sub").join("config.json");
        let original = sample();
        original.save(&path).expect("save");
        let loaded = Config::load(&path).expect("load");
        assert_eq!(loaded, original);
    }

    #[test]
    fn save_zero_schema_version_is_normalized_to_current() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        let cfg = Config {
            wallet_address: Some("0xabc".into()),
            ..Config::default()
        };
        assert_eq!(cfg.schema_version, 0);
        cfg.save(&path).expect("save");
        let loaded = Config::load(&path).expect("load");
        assert_eq!(loaded.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn save_creates_missing_parent_dir() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a").join("b").join("config.json");
        sample().save(&nested).expect("save into nested path");
        assert!(nested.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_tightens_parent_directory_to_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let cfg_dir = tmp.path().join(".taskfast");
        let path = cfg_dir.join("config.json");
        sample().save(&path).expect("save");
        let mode = fs::metadata(&cfg_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            ".taskfast/ must be 0700 so other users on the host can't \
             list the keystore + webhook secret + API key"
        );
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        let body = r#"{
            "schema_version": 2,
            "wallet_address": "0xabc",
            "future_field": "ignored",
            "another": {"nested": true}
        }"#;
        fs::write(&path, body).unwrap();
        let cfg = Config::load(&path).expect("unknown fields should not fail load");
        assert_eq!(cfg.wallet_address.as_deref(), Some("0xabc"));
    }

    #[test]
    fn newer_schema_version_loads_with_warning() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        let body = format!(
            r#"{{ "schema_version": {}, "wallet_address": "0xabc" }}"#,
            CURRENT_SCHEMA_VERSION + 5
        );
        fs::write(&path, body).unwrap();
        let cfg = Config::load(&path).expect("newer version still loads");
        assert_eq!(cfg.schema_version, CURRENT_SCHEMA_VERSION + 5);
        assert_eq!(cfg.wallet_address.as_deref(), Some("0xabc"));
    }

    #[test]
    fn legacy_api_base_field_hard_errors() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        let body = r#"{ "schema_version": 1, "api_base": "https://attacker.example" }"#;
        fs::write(&path, body).unwrap();
        let err = Config::load(&path).expect_err("legacy api_base must be rejected");
        let ConfigError::LegacyFields { fields, .. } = err else {
            panic!("expected LegacyFields, got {err:?}");
        };
        assert_eq!(fields, vec!["api_base".to_string()]);
    }

    #[test]
    fn legacy_network_field_hard_errors() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        let body = r#"{ "schema_version": 1, "network": "mainnet" }"#;
        fs::write(&path, body).unwrap();
        let err = Config::load(&path).expect_err("legacy network must be rejected");
        let ConfigError::LegacyFields { fields, .. } = err else {
            panic!("expected LegacyFields, got {err:?}");
        };
        assert_eq!(fields, vec!["network".to_string()]);
    }

    #[test]
    fn legacy_error_lists_both_fields_when_present() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        let body = r#"{ "api_base": "https://x", "network": "testnet" }"#;
        fs::write(&path, body).unwrap();
        let err = Config::load(&path).expect_err("both legacy fields must be flagged");
        let ConfigError::LegacyFields { fields, .. } = err else {
            panic!("expected LegacyFields, got {err:?}");
        };
        assert!(fields.contains(&"api_base".to_string()));
        assert!(fields.contains(&"network".to_string()));
    }

    #[test]
    fn legacy_error_message_names_migration_command() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        fs::write(&path, r#"{ "api_base": "https://x" }"#).unwrap();
        let msg = Config::load(&path).unwrap_err().to_string();
        assert!(
            msg.contains("taskfast config migrate"),
            "remediation hint must name the migrate command: {msg}"
        );
    }

    #[test]
    fn empty_config_serializes_compactly() {
        // A default config has no runtime fields set — the JSON should
        // be close to `{}` (just an empty object after skip_if).
        let body = serde_json::to_string(&Config::default()).unwrap();
        assert_eq!(body, "{}", "default config should serialize to {{}}");
    }

    #[test]
    fn environment_roundtrip() {
        for env in [Environment::Prod, Environment::Staging, Environment::Local] {
            let cfg = Config {
                environment: Some(env),
                ..Config::default()
            };
            let body = serde_json::to_string(&cfg).unwrap();
            let back: Config = serde_json::from_str(&body).unwrap();
            assert_eq!(back.environment.map(|e| e.as_str()), Some(env.as_str()));
        }
    }

    #[test]
    fn environment_accepts_production_alias() {
        let body = r#"{ "environment": "production" }"#;
        let cfg: Config = serde_json::from_str(body).unwrap();
        assert!(matches!(cfg.environment, Some(Environment::Prod)));
    }

    #[test]
    fn environment_rejects_unknown() {
        let body = r#"{ "environment": "moon" }"#;
        let err = serde_json::from_str::<Config>(body).unwrap_err();
        assert!(err.to_string().contains("moon"));
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_mode_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        sample().save(&path).expect("save");
        let meta = fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected mode 0600, got {mode:o}");
    }

    #[test]
    fn parse_error_includes_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.json");
        fs::write(&path, "{ not json").unwrap();
        let err = Config::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bad.json"), "error mentions path: {msg}");
    }
}
