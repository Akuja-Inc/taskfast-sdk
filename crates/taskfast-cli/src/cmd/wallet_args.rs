//! Shared keystore + password resolution for `post` / `settle` / `escrow`.
//!
//! All three subcommands accept the same triad (`--keystore`,
//! `--wallet-password-file`, `--wallet-address`) and resolve the keystore via
//! the same two-step process:
//!
//!   1. Password: `TASKFAST_WALLET_PASSWORD` env wins (preserves CI); else
//!      read from `--wallet-password-file`, trim trailing newline, reject
//!      empty.
//!   2. Keystore: strip the optional `file:` scheme prefix (`taskfast init`
//!      writes that form to `TEMPO_KEY_SOURCE`), then
//!      `keystore::load(File { path }, password)`.
//!
//! Exposed as free functions (not a flattened clap struct) so each caller
//! keeps its existing `Args` struct — renaming those would churn wiremock
//! tests that import them by name.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use alloy_signer_local::PrivateKeySigner;
use zeroize::Zeroizing;

use taskfast_agent::keystore::{self, KeySource};

use super::CmdError;

/// One-shot guard so the env-var deprecation warning fires at most once
/// per process. Streaming subcommands could otherwise spam stderr.
static PWD_WARNING_EMITTED: AtomicBool = AtomicBool::new(false);

/// Decrypt the keystore at `keystore_ref` using the resolved password.
///
/// `purpose` is interpolated into the "missing --keystore" usage error so the
/// operator sees *which* flow demanded a signer (e.g. "submission fee",
/// "settlement approval", "escrow approval").
pub fn load_signer(
    keystore_ref: Option<&str>,
    password_file: Option<&Path>,
    purpose: &str,
) -> Result<PrivateKeySigner, CmdError> {
    let raw = keystore_ref.ok_or_else(|| {
        CmdError::Usage(format!(
            "--keystore (or TEMPO_KEY_SOURCE) is required to sign the {purpose}"
        ))
    })?;
    let path_str = raw.strip_prefix("file:").unwrap_or(raw);
    let password = resolve_password(password_file)?;
    let path = PathBuf::from(path_str);
    // `&*password` narrows `Zeroizing<String>` → `&str` only at the
    // keystore boundary; the buffer zeroizes on drop at end of scope.
    keystore::load(&KeySource::File { path }, &password).map_err(CmdError::from)
}

/// Resolve the keystore password: env var wins over file. Trims trailing
/// `\r`/`\n` but rejects a file that is otherwise empty.
///
/// Wraps the secret in [`Zeroizing`] so the backing allocation is scrubbed
/// on drop — reduces the in-memory window of the plaintext password within
/// *our* address space. The scrypt derivation inside
/// `alloy_signer_local::decrypt_keystore` copies the bytes into its own
/// buffer we cannot control; this fix shrinks the CLI's copy scope.
pub fn resolve_password(password_file: Option<&Path>) -> Result<Zeroizing<String>, CmdError> {
    if let Ok(pw) = std::env::var("TASKFAST_WALLET_PASSWORD") {
        if !pw.is_empty() {
            warn_pwd_env_once();
            return Ok(Zeroizing::new(pw));
        }
    }
    let path = password_file.ok_or_else(|| {
        CmdError::Usage(
            "TASKFAST_WALLET_PASSWORD or --wallet-password-file required to unlock keystore".into(),
        )
    })?;
    let raw = Zeroizing::new(std::fs::read_to_string(path).map_err(|e| {
        CmdError::Usage(format!("read wallet password file {}: {e}", path.display()))
    })?);
    let trimmed = raw.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return Err(CmdError::Usage(format!(
            "wallet password file {} is empty",
            path.display()
        )));
    }
    // F13: a trailing newline is fine (editors add one), but a newline
    // *inside* the trimmed body almost always means the operator pasted
    // extra content (two passwords, a commented-out line, a stray label).
    // Refuse rather than silently taking the first line as the password.
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return Err(CmdError::Usage(format!(
            "wallet password file {} must contain a single line (found an interior newline)",
            path.display()
        )));
    }
    Ok(Zeroizing::new(trimmed.to_string()))
}

/// Print the `TASKFAST_WALLET_PASSWORD` deprecation nudge to stderr once
/// per process. Suppressed by `TASKFAST_SUPPRESS_PWD_WARNING=1` so CI
/// pipelines that intentionally use the env var can quiet the noise.
fn warn_pwd_env_once() {
    if std::env::var("TASKFAST_SUPPRESS_PWD_WARNING").is_ok_and(|v| v == "1") {
        return;
    }
    if PWD_WARNING_EMITTED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "warning: TASKFAST_WALLET_PASSWORD is readable via /proc/<pid>/environ; \
         prefer --wallet-password-file (TASKFAST_WALLET_PASSWORD_FILE) or set \
         TASKFAST_SUPPRESS_PWD_WARNING=1 to silence."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(contents.as_bytes()).unwrap();
        f
    }

    // Serialize env-var-touching tests — parallel cargo test workers share
    // process env and would clobber each other's assertions otherwise.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn password_env_wins_over_file() {
        let _g = ENV_LOCK.lock().unwrap();
        let f = write_temp("from-file\n");
        std::env::set_var("TASKFAST_WALLET_PASSWORD", "from-env");
        let pw = resolve_password(Some(f.path())).expect("ok");
        assert_eq!(pw.as_str(), "from-env");
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
    }

    #[test]
    fn password_file_trimmed_when_env_absent() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
        let f = write_temp("secret\r\n");
        let pw = resolve_password(Some(f.path())).expect("ok");
        assert_eq!(pw.as_str(), "secret");
    }

    #[test]
    fn password_rejects_interior_newline() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
        let f = write_temp("first-line\nsecond-line\n");
        let err = resolve_password(Some(f.path())).expect_err("multi-line must fail");
        match err {
            CmdError::Usage(m) => assert!(
                m.contains("single line"),
                "message must name the constraint: {m}"
            ),
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn password_rejects_interior_carriage_return() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
        // CRLF in the middle (Windows-edited file with a stray prefix line).
        let f = write_temp("annotation\r\nsecret\n");
        let err = resolve_password(Some(f.path())).expect_err("must fail");
        assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
    }

    #[test]
    fn password_is_zeroizing_wrapped() {
        // Compile-time proof: return type is Zeroizing<String>, i.e. drop
        // scrubs the backing allocation. Sanity-check via the type name.
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
        let f = write_temp("scrubme\n");
        let pw = resolve_password(Some(f.path())).expect("ok");
        let name = std::any::type_name_of_val(&pw);
        assert!(
            name.contains("Zeroizing"),
            "password must be Zeroizing: {name}"
        );
    }

    #[test]
    fn password_rejects_empty_file() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
        let f = write_temp("\n\n");
        let err = resolve_password(Some(f.path())).expect_err("empty must fail");
        assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
    }

    #[test]
    fn password_requires_file_when_env_absent() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
        let err = resolve_password(None).expect_err("no source → Usage");
        assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
    }

    #[test]
    fn load_signer_missing_keystore_surfaces_purpose() {
        let err = load_signer(None, None, "escrow approval").expect_err("no keystore → Usage");
        match err {
            CmdError::Usage(m) => {
                assert!(m.contains("escrow approval"), "purpose must appear: {m}")
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }
}
