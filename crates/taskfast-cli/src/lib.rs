//! `taskfast-cli` library surface.
//!
//! The crate ships primarily as the `taskfast` binary (see `src/main.rs`),
//! but every subcommand's `run` function and the shared envelope/exit/error
//! types are re-exported here so integration tests (and, later, embedded
//! callers) can drive the pipeline without spawning a process.

// TODO: tighten doc coverage on public items + remove this allow.
// Tracked under the rust-best-practices follow-up.
#![allow(missing_docs)]

pub mod cmd;
pub mod config;
pub mod envelope;
pub mod exit;
pub mod wallet_lock;

pub use config::Config;
pub use envelope::{Envelope, ErrorPayload};
pub use exit::ExitCode;

/// Re-exported from `main.rs` so tests can construct a [`cmd::Ctx`] with a
/// named [`Environment`] without depending on the binary entry point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Environment {
    Prod,
    Staging,
    Local,
}

impl Environment {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prod => "production",
            Self::Staging => "staging",
            Self::Local => "local",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Prod => "https://api.taskfast.app",
            Self::Staging => "https://staging.api.taskfast.app",
            Self::Local => "http://localhost:4000",
        }
    }
}

/// Well-known TaskFast API base URLs that match an [`Environment`] default.
///
/// Used by the F2 endpoint-override guard: an `api_base` loaded from a
/// CWD-local config file is rejected unless it either matches one of these
/// or the caller passed `--allow-custom-endpoints` (or env
/// `TASKFAST_ALLOW_CUSTOM_ENDPOINTS=1`). A malicious cloned repo shipping
/// a `.taskfast/config.json` pointing `api_base` at attacker infra would
/// otherwise silently exfiltrate the PAT on the first request.
pub const WELL_KNOWN_API_BASES: &[&str] = &[
    "https://api.taskfast.app",
    "https://staging.api.taskfast.app",
    "http://localhost:4000",
];

/// True when `url` exactly matches a known-good default from
/// [`WELL_KNOWN_API_BASES`]. Trailing `/` is tolerated so a config-file
/// value `https://api.taskfast.app/` isn't flagged as custom.
pub fn is_well_known_api_base(url: &str) -> bool {
    let trimmed = url.trim_end_matches('/');
    WELL_KNOWN_API_BASES
        .iter()
        .any(|w| w.trim_end_matches('/') == trimmed)
}

/// Derive the human-facing account-tokens URL from an API base URL.
///
/// Strips a leading `api.` from the host (so `api.taskfast.app` →
/// `taskfast.app`) and appends `/account/tokens`. Hosts without the
/// `api.` prefix (localhost, bare domains, IPs) are passed through.
/// Scheme and port are preserved.
pub fn accounts_url(api_base: &str) -> String {
    match url::Url::parse(api_base) {
        Ok(mut u) => {
            let rewritten = u.host_str().and_then(strip_api_label);
            if let Some(h) = rewritten {
                let _ = u.set_host(Some(&h));
            }
            u.set_path("/account/tokens");
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        // Fallback for a malformed base: best-effort string concat.
        Err(_) => format!("{}/account/tokens", api_base.trim_end_matches('/')),
    }
}

/// Drop the `api` label from a host so the URL points at the human-facing
/// dashboard. `api.taskfast.app` → `taskfast.app`;
/// `staging.api.taskfast.app` → `staging.taskfast.app`. Returns `None`
/// when the host has no `api` label to strip.
fn strip_api_label(host: &str) -> Option<String> {
    if let Some(rest) = host.strip_prefix("api.") {
        return Some(rest.to_owned());
    }
    // Middle label: collapse `.api.` into a single `.`.
    if host.contains(".api.") {
        return Some(host.replacen(".api.", ".", 1));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{accounts_url, is_well_known_api_base, Environment, WELL_KNOWN_API_BASES};

    #[test]
    fn well_known_api_bases_cover_every_environment_default() {
        // Guard against drift: if someone adds a new Environment variant and
        // forgets to register its default URL, the F2 guard would reject the
        // CLI's own baked-in default.
        for env in [Environment::Prod, Environment::Staging, Environment::Local] {
            let url = env.default_base_url();
            assert!(
                is_well_known_api_base(url),
                "default URL for {env:?} ({url}) must be in WELL_KNOWN_API_BASES"
            );
        }
    }

    #[test]
    fn is_well_known_api_base_accepts_exact_defaults() {
        for url in WELL_KNOWN_API_BASES {
            assert!(is_well_known_api_base(url), "expected well-known: {url}");
        }
    }

    #[test]
    fn is_well_known_api_base_tolerates_trailing_slash() {
        assert!(is_well_known_api_base("https://api.taskfast.app/"));
        assert!(is_well_known_api_base("http://localhost:4000/"));
    }

    #[test]
    fn is_well_known_api_base_rejects_attacker_hosts() {
        assert!(!is_well_known_api_base("https://evil.example"));
        assert!(!is_well_known_api_base(
            "https://api.taskfast.app.evil.example"
        ));
        assert!(!is_well_known_api_base("http://api.taskfast.app"));
        assert!(!is_well_known_api_base("https://staging.taskfast.app"));
    }

    #[test]
    fn accounts_url_strips_api_prefix_prod() {
        assert_eq!(
            accounts_url("https://api.taskfast.app"),
            "https://taskfast.app/account/tokens"
        );
    }

    #[test]
    fn accounts_url_strips_api_prefix_staging() {
        assert_eq!(
            accounts_url("https://staging.api.taskfast.app"),
            "https://staging.taskfast.app/account/tokens"
        );
    }

    #[test]
    fn accounts_url_passthrough_localhost_with_port() {
        assert_eq!(
            accounts_url("http://localhost:4000"),
            "http://localhost:4000/account/tokens"
        );
    }

    #[test]
    fn accounts_url_passthrough_bare_domain() {
        assert_eq!(
            accounts_url("https://taskfast.app"),
            "https://taskfast.app/account/tokens"
        );
    }

    #[test]
    fn accounts_url_drops_existing_path_and_query() {
        assert_eq!(
            accounts_url("https://api.taskfast.app/v1?x=1"),
            "https://taskfast.app/account/tokens"
        );
    }

    #[test]
    fn accounts_url_malformed_falls_back_to_concat() {
        assert_eq!(accounts_url("not a url"), "not a url/account/tokens");
    }
}
