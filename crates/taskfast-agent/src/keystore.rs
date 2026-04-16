//! Encrypted wallet persistence.
//!
//! Default: Web3 Secret Storage v3 JSON (Foundry/Geth/MetaMask-interoperable),
//! resolved under `$XDG_DATA_HOME/taskfast/wallets/` so a wallet generated here
//! can be imported by `cast wallet import` without conversion.
//!
//! This module owns two concerns and nothing else:
//!
//! 1. Path resolution — XDG-compliant directory layout with `0700` perms on
//!    the containing dir and `0600` perms on the keyfile.
//! 2. Encrypt/decrypt — thin wrappers around
//!    `PrivateKeySigner::encrypt_keystore` / `decrypt_keystore`, which
//!    delegate to `eth-keystore` for the scrypt+AES-128-CTR pipeline.
//!
//! # Future: OS keychain
//!
//! `KeySource::Keychain` is reserved but not yet implemented. When it lands,
//! it'll go behind a `keyring` cargo feature so the default build has no
//! dbus/secret-service transitive deps. CLI sandboxes that need portable
//! persistence should use the file backend; keychain support is targeted at
//! interactive dev boxes.

use std::fs;
use std::path::{Path, PathBuf};

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use thiserror::Error as ThisError;

/// Where a wallet private key is (or will be) stored.
///
/// Kept as an enum rather than a boolean flag so the CLI's `--keystore` /
/// `--keychain` flags can desugar into a discriminated choice that the
/// lower layers (and tests) can match on. Adding a `Keychain` variant
/// later is additive: existing `File` users keep working.
#[derive(Debug, Clone)]
pub enum KeySource {
    /// Encrypted JSON v3 at an explicit path (or resolved via
    /// [`default_keyfile_path`]).
    File { path: PathBuf },
    // Keychain { service: String, account: String } — deferred behind a
    // future `keyring` cargo feature; see module docs.
}

#[derive(Debug, ThisError)]
pub enum KeystoreError {
    #[error("home directory not found; set $HOME or $XDG_DATA_HOME")]
    HomeNotFound,
    #[error("failed to create keystore dir {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("keystore file not found: {0}")]
    NotFound(PathBuf),
    #[error("signer/keystore error: {0}")]
    Signer(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Returns `$XDG_DATA_HOME/taskfast/wallets`, falling back to
/// `$HOME/.local/share/taskfast/wallets` when XDG is unset.
///
/// Creates the directory if it doesn't exist and tightens perms to `0700`
/// on unix. Keystore files are written `0600` after the fact by
/// [`save_signer`].
pub fn default_keystore_dir() -> Result<PathBuf, KeystoreError> {
    let base = match std::env::var("XDG_DATA_HOME") {
        Ok(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
        _ => home_fallback()?,
    };
    let dir = base.join("taskfast").join("wallets");
    ensure_dir(&dir)?;
    Ok(dir)
}

fn home_fallback() -> Result<PathBuf, KeystoreError> {
    // XDG spec §4: when $XDG_DATA_HOME is unset/empty, default to
    // $HOME/.local/share.
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(|home| PathBuf::from(home).join(".local").join("share"))
        .ok_or(KeystoreError::HomeNotFound)
}

fn ensure_dir(dir: &Path) -> Result<(), KeystoreError> {
    fs::create_dir_all(dir).map_err(|source| KeystoreError::CreateDir {
        path: dir.to_path_buf(),
        source,
    })?;
    // `0700` — private to the owner. No-op on non-unix; Windows ACLs are
    // the OS-native equivalent and out of scope here.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(dir)?.permissions();
        perms.set_mode(0o700);
        fs::set_permissions(dir, perms)?;
    }
    Ok(())
}

/// Canonical filename for a wallet keyed by its address: lowercase hex,
/// no `0x` prefix, `.json` suffix. Matches the shape `cast wallet import`
/// produces when given `--keystore-dir` without an explicit name.
pub fn default_keyfile_name(address: Address) -> String {
    let hex = hex::encode(address.as_slice());
    format!("{hex}.json")
}

/// Resolves the default keyfile path for a given address under the default
/// keystore dir.
pub fn default_keyfile_path(address: Address) -> Result<PathBuf, KeystoreError> {
    Ok(default_keystore_dir()?.join(default_keyfile_name(address)))
}

/// Encrypts `signer` with `password` and writes it to `path`, creating parent
/// directories as needed. Overwrites any existing file at the same path —
/// callers wanting idempotency should check [`Path::exists`] first.
///
/// On unix the final file is chmod'd to `0600`. Returns the absolute path
/// actually written.
pub fn save_signer(
    signer: &PrivateKeySigner,
    path: &Path,
    password: &str,
) -> Result<PathBuf, KeystoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| KeystoreError::NotFound(path.to_path_buf()))?;
    ensure_dir(parent)?;

    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| KeystoreError::NotFound(path.to_path_buf()))?;

    let mut rng = rand::thread_rng();
    // `encrypt_keystore` writes `{parent}/{name}` via eth-keystore's
    // scrypt + AES-128-CTR pipeline. We pass the already-generated key's
    // bytes so the saved file corresponds to an existing signer.
    let pk_bytes = signer.to_bytes();
    PrivateKeySigner::encrypt_keystore(parent, &mut rng, pk_bytes, password, Some(file_name))
        .map_err(|e| KeystoreError::Signer(e.to_string()))?;

    let written = parent.join(file_name);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&written)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&written, perms)?;
    }
    Ok(written)
}

/// Generates a fresh random signer, encrypts it, writes it to the default
/// path for its address, and returns `(signer, path)`. The most common init
/// flow: `taskfast init --generate-wallet`.
pub fn generate_and_save(password: &str) -> Result<(PrivateKeySigner, PathBuf), KeystoreError> {
    let signer = PrivateKeySigner::random();
    let path = default_keyfile_path(signer.address())?;
    save_signer(&signer, &path, password)?;
    Ok((signer, path))
}

/// Decrypts the keystore at `path` with `password` and returns the signer.
pub fn load_signer(path: &Path, password: &str) -> Result<PrivateKeySigner, KeystoreError> {
    if !path.exists() {
        return Err(KeystoreError::NotFound(path.to_path_buf()));
    }
    PrivateKeySigner::decrypt_keystore(path, password)
        .map_err(|e| KeystoreError::Signer(e.to_string()))
}

/// Load a signer by [`KeySource`] — dispatches to the file backend today.
/// Keychain support will slot in here under a cargo feature.
pub fn load(source: &KeySource, password: &str) -> Result<PrivateKeySigner, KeystoreError> {
    match source {
        KeySource::File { path } => load_signer(path, password),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_keyfile_name_is_lowercase_hex_without_prefix() {
        // Deterministic check: bytes 0x01..0x14 → "0102..14.json".
        let mut bytes = [0u8; 20];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i + 1) as u8;
        }
        let addr = Address::from(bytes);
        let name = default_keyfile_name(addr);
        assert_eq!(name, "0102030405060708090a0b0c0d0e0f1011121314.json");
    }

    #[test]
    fn save_then_load_roundtrips_signer() {
        let tmp = TempDir::new().unwrap();
        let signer = PrivateKeySigner::random();
        let expected = signer.address();

        let path = tmp.path().join("wallet.json");
        save_signer(&signer, &path, "s3kret").expect("save");
        assert!(path.exists(), "keystore file must exist after save");

        let loaded = load_signer(&path, "s3kret").expect("load");
        assert_eq!(loaded.address(), expected);
    }

    #[test]
    fn load_with_wrong_password_errors() {
        let tmp = TempDir::new().unwrap();
        let signer = PrivateKeySigner::random();
        let path = tmp.path().join("wallet.json");
        save_signer(&signer, &path, "correct").expect("save");

        let err = load_signer(&path, "wrong").unwrap_err();
        assert!(matches!(err, KeystoreError::Signer(_)));
    }

    #[test]
    fn load_missing_file_reports_not_found() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope.json");
        let err = load_signer(&missing, "pw").unwrap_err();
        assert!(matches!(err, KeystoreError::NotFound(_)));
    }

    #[test]
    fn save_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a").join("b").join("wallet.json");
        let signer = PrivateKeySigner::random();
        save_signer(&signer, &nested, "pw").expect("save");
        assert!(nested.exists());
    }

    #[cfg(unix)]
    #[test]
    fn saved_keyfile_has_0600_perms() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("wallet.json");
        let signer = PrivateKeySigner::random();
        save_signer(&signer, &path, "pw").expect("save");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "keyfile must be owner-only readable/writable");
    }

    #[test]
    fn key_source_file_dispatches_to_file_backend() {
        let tmp = TempDir::new().unwrap();
        let signer = PrivateKeySigner::random();
        let expected = signer.address();
        let path = tmp.path().join("wallet.json");
        save_signer(&signer, &path, "pw").expect("save");

        let loaded = load(&KeySource::File { path: path.clone() }, "pw").expect("load");
        assert_eq!(loaded.address(), expected);
    }
}
