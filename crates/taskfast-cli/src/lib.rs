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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum Environment {
    Prod,
    Staging,
    Local,
}

/// Tempo network selector. Derived from [`Environment`] — never persisted,
/// never accepted as a CLI flag. The compile-time mapping in
/// [`Environment::network`] is the single source of truth; the runtime
/// invariant in `cmd::enforce_server_network_invariant` cross-checks it
/// against `/config/network`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Mainnet,
    Testnet,
}

impl Network {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet => "testnet",
        }
    }
}

impl Environment {
    /// Every variant. Drives table-driven tests and the well-known-base
    /// iterator; touching [`Environment`] without updating this array
    /// fails the `all_covers_every_variant` test.
    pub const ALL: &'static [Environment] = &[Self::Prod, Self::Staging, Self::Local];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prod => "production",
            Self::Staging => "staging",
            Self::Local => "local",
        }
    }

    /// Sole TaskFast API base URL for this environment. Total function;
    /// the F2 endpoint-override guard treats anything else as adversarial.
    ///
    /// `Local` uses the IPv4 literal `127.0.0.1` rather than `localhost`:
    /// the well-known-base allowlist matches the bound string verbatim, and
    /// some dev hosts resolve `localhost` to `::1` first, which then misses
    /// an IPv4-only `mix phx.server` and confuses the endpoint guard.
    pub fn api_base(self) -> &'static str {
        match self {
            Self::Prod => "https://api.taskfast.app",
            Self::Staging => "https://staging.api.taskfast.app",
            Self::Local => "http://127.0.0.1:4000",
        }
    }

    /// Sole Tempo network for this environment. Prod runs mainnet; staging
    /// and local both run testnet. The runtime invariant verifies the
    /// deployment at [`api_base`](Self::api_base) advertises exactly this
    /// network and no other.
    pub fn network(self) -> Network {
        match self {
            Self::Prod => Network::Mainnet,
            Self::Staging => Network::Testnet,
            Self::Local => Network::Testnet,
        }
    }
}

/// Iterator over the well-known TaskFast API base URLs — one per
/// [`Environment`] variant, derived from [`Environment::api_base`].
///
/// Used by the F2 endpoint-override guard: an `api_base` supplied via the
/// `--api-base` flag is rejected unless it matches one of these or the
/// caller passed `--allow-custom-endpoints`.
pub fn well_known_api_bases() -> impl Iterator<Item = &'static str> {
    Environment::ALL.iter().map(|e| e.api_base())
}

/// True when `url` exactly matches a known-good default from
/// [`well_known_api_bases`]. Trailing `/` is tolerated so a value
/// `https://api.taskfast.app/` isn't flagged as custom.
pub fn is_well_known_api_base(url: &str) -> bool {
    let trimmed = url.trim_end_matches('/');
    well_known_api_bases().any(|w| w.trim_end_matches('/') == trimmed)
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
    use super::{accounts_url, is_well_known_api_base, well_known_api_bases, Environment, Network};
    use std::collections::HashSet;

    #[test]
    fn all_covers_every_variant() {
        // Drift trap: if someone adds an Environment variant without
        // appending it to ::ALL, this test catches the gap that compile-
        // time exhaustiveness can't (a new variant compiles fine if it's
        // never read; ::ALL is a const array, not a match).
        let n = Environment::ALL.len();
        let unique: HashSet<_> = Environment::ALL.iter().copied().collect();
        assert_eq!(unique.len(), n, "Environment::ALL has duplicates");
        for env in Environment::ALL {
            // Touch every method so adding a variant without updating one
            // of these breaks the build via match exhaustiveness.
            let _ = env.as_str();
            let _ = env.api_base();
            let _ = env.network();
        }
    }

    #[test]
    fn each_env_has_unique_api_base() {
        let bases: HashSet<_> = Environment::ALL.iter().map(|e| e.api_base()).collect();
        assert_eq!(
            bases.len(),
            Environment::ALL.len(),
            "two envs share an api_base"
        );
    }

    #[test]
    fn env_api_base_table_is_frozen() {
        // Pin the exact mapping. Touching this test = a deliberate decision.
        assert_eq!(Environment::Prod.api_base(), "https://api.taskfast.app");
        assert_eq!(
            Environment::Staging.api_base(),
            "https://staging.api.taskfast.app"
        );
        assert_eq!(Environment::Local.api_base(), "http://127.0.0.1:4000");
    }

    #[test]
    fn env_network_table_is_frozen() {
        assert_eq!(Environment::Prod.network(), Network::Mainnet);
        assert_eq!(Environment::Staging.network(), Network::Testnet);
        assert_eq!(Environment::Local.network(), Network::Testnet);
    }

    #[test]
    fn well_known_api_bases_cover_every_environment_default() {
        for env in Environment::ALL {
            let url = env.api_base();
            assert!(
                is_well_known_api_base(url),
                "api_base for {env:?} ({url}) must be in well_known_api_bases"
            );
        }
    }

    #[test]
    fn is_well_known_api_base_accepts_exact_defaults() {
        for url in well_known_api_bases() {
            assert!(is_well_known_api_base(url), "expected well-known: {url}");
        }
    }

    #[test]
    fn is_well_known_api_base_tolerates_trailing_slash() {
        assert!(is_well_known_api_base("https://api.taskfast.app/"));
        assert!(is_well_known_api_base("http://127.0.0.1:4000/"));
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
            accounts_url("http://127.0.0.1:4000"),
            "http://127.0.0.1:4000/account/tokens"
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
