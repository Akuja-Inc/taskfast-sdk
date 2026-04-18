//! Interactive first-run helpers for `taskfast init`.
//!
//! The non-interactive path (flag / env / config driven) is the default
//! and unchanged — see `cmd/init.rs`. This module gates the TTY-only
//! branch: when a human runs `taskfast init` with no credentials
//! resolvable, we prompt for a PAT, greet via `GET /users/me`, and walk
//! wallet provisioning.
//!
//! All interactive surface goes through [`Prompter`] so tests can inject
//! a scripted implementation without driving a real TTY.

use std::io::{self, IsTerminal};

use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, Password, Select};
use taskfast_client::UserProfile;

/// Wallet-provisioning choice surfaced by the interactive prompt.
/// Mirrors the three mutually-exclusive flag paths (`--wallet-address`,
/// `--generate-wallet`, `--skip-wallet`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletMode {
    /// Bring-your-own-wallet: caller supplies an existing address.
    Byow,
    /// Generate a fresh keypair and persist it via the keystore module.
    Generate,
    /// Defer wallet provisioning — agent runs without settlement.
    Skip,
}

/// Test-seam trait covering every TTY interaction the init flow needs.
/// The real implementation ([`DialoguerPrompter`]) reads from the
/// terminal; tests inject a scripted double.
pub trait Prompter {
    /// Prompt for a user PAT (`tf_user_*`). `accounts_url` is the page
    /// where the human can mint/copy one.
    fn pat(&self, accounts_url: &str) -> io::Result<String>;
    /// Prompt for a wallet-mode choice (BYOW / generate / skip).
    fn wallet_mode(&self) -> io::Result<WalletMode>;
    /// Prompt for a `0x…`-prefixed wallet address (BYOW path).
    fn wallet_address(&self) -> io::Result<String>;
    /// Prompt for a keystore password, asking for confirmation (generate path).
    fn wallet_password(&self) -> io::Result<String>;
}

/// Real [`Prompter`] backed by `dialoguer`. Uses the crate's default
/// colorful theme so the PAT prompt, select list, etc. match standard
/// CLI ergonomics.
pub struct DialoguerPrompter;

impl Prompter for DialoguerPrompter {
    fn pat(&self, accounts_url: &str) -> io::Result<String> {
        // Printed to stderr so the envelope on stdout stays machine-parseable
        // in pipelines that happen to be TTY-attached.
        eprintln!("Get a PAT at: {accounts_url}");
        Password::with_theme(&ColorfulTheme::default())
            .with_prompt("Paste your PAT")
            .interact()
            .map_err(dialoguer_err)
    }

    fn wallet_mode(&self) -> io::Result<WalletMode> {
        let choices = [
            "Use existing wallet (BYOW)",
            "Generate new wallet",
            "Skip for now",
        ];
        let idx = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Wallet setup")
            .items(&choices)
            .default(0)
            .interact()
            .map_err(dialoguer_err)?;
        Ok(match idx {
            0 => WalletMode::Byow,
            1 => WalletMode::Generate,
            _ => WalletMode::Skip,
        })
    }

    fn wallet_address(&self) -> io::Result<String> {
        Input::<String>::with_theme(&ColorfulTheme::default())
            .with_prompt("Wallet address")
            .validate_with(|s: &String| validate_wallet_address(s))
            .interact_text()
            .map_err(dialoguer_err)
    }

    fn wallet_password(&self) -> io::Result<String> {
        Password::with_theme(&ColorfulTheme::default())
            .with_prompt("Keystore password")
            .with_confirmation("Confirm password", "Passwords don't match")
            .interact()
            .map_err(dialoguer_err)
    }
}

fn dialoguer_err(e: dialoguer::Error) -> io::Error {
    match e {
        dialoguer::Error::IO(io) => io,
    }
}

/// `0x` + 40 hex chars. Trims surrounding whitespace before checking so
/// copy-paste with a trailing newline is tolerated.
fn validate_wallet_address(s: &str) -> Result<(), &'static str> {
    let trimmed = s.trim();
    let rest = trimmed
        .strip_prefix("0x")
        .ok_or("address must start with 0x")?;
    if rest.len() != 40 {
        return Err("address must be 0x + 40 hex characters");
    }
    if !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("address must be hexadecimal");
    }
    Ok(())
}

/// True when both stdin and stdout are attached to a terminal. False in
/// pipes, redirects, CI, and under the test harness — callers use this
/// to decide whether prompting is safe.
pub fn is_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Format the post-auth greeting. With a profile: `"Hi Name <email>"`.
/// Without one (server lacks `/users/me`, returned 404): a generic
/// authenticated message so the user still gets visible confirmation.
pub fn greeting(profile: Option<&UserProfile>) -> String {
    match profile {
        Some(p) => format!("Hi {} <{}>", p.name, p.email),
        None => "Hi there — authenticated.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_with_profile() {
        let p = UserProfile {
            name: "Alice Smith".into(),
            email: "alice@example.com".into(),
        };
        assert_eq!(greeting(Some(&p)), "Hi Alice Smith <alice@example.com>");
    }

    #[test]
    fn greeting_without_profile_is_neutral() {
        assert_eq!(greeting(None), "Hi there — authenticated.");
    }

    #[test]
    fn valid_wallet_address_accepted() {
        assert!(validate_wallet_address("0x0123456789abcdef0123456789ABCDEF01234567").is_ok());
    }

    #[test]
    fn wallet_address_missing_0x_rejected() {
        assert!(validate_wallet_address("0123456789abcdef0123456789ABCDEF01234567").is_err());
    }

    #[test]
    fn wallet_address_wrong_length_rejected() {
        assert!(validate_wallet_address("0xabc").is_err());
    }

    #[test]
    fn wallet_address_non_hex_rejected() {
        assert!(validate_wallet_address("0xGGGG456789abcdef0123456789ABCDEF01234567").is_err());
    }

    #[test]
    fn wallet_address_trims_whitespace() {
        assert!(validate_wallet_address("  0x0123456789abcdef0123456789ABCDEF01234567\n").is_ok());
    }
}
