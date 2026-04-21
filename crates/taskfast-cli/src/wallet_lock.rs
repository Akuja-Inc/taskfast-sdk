//! F9: exclusive file-lock guarding wallet-signing operations against
//! concurrent `taskfast post` invocations sharing the same keystore.
//!
//! # What this prevents
//!
//! Tempo (like any EVM network) uses a monotonic per-address nonce. If
//! two processes both call `eth_getTransactionCount(addr, "latest")` at
//! the same moment, both get the same value, both sign a tx with that
//! nonce, and the RPC drops whichever arrives second. The scenario is
//! realistic for a multi-worker agent farm pointed at one wallet.
//!
//! # Scope
//!
//! Advisory, best-effort. We hold an OS-level exclusive lock on a file
//! adjacent to the keystore from just before `eth_getTransactionCount`
//! until after the raw tx has been submitted. A crash between those
//! points releases the lock cleanly (the kernel drops it on fd close),
//! so there's no stale-lock problem. The lock does **not** serialize
//! wallets across different hosts — operators with multi-host setups
//! must still keep per-host nonce coordination in mind.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

use crate::cmd::CmdError;

/// RAII handle releasing the lock on drop. Held for the entire
/// sign+broadcast critical section.
pub struct WalletGuard(#[allow(dead_code)] File);

/// Acquire an exclusive file-lock keyed off the keystore path.
///
/// Blocks until the lock is available. The lock file lives at
/// `<keystore_dir>/.taskfast-wallet.lock`; same-directory placement
/// means two processes with different `--keystore` inputs that resolve
/// to the same canonical path end up on the same lock.
pub fn acquire(keystore_path: &Path) -> Result<WalletGuard, CmdError> {
    let lock_path = lock_path_for(keystore_path);
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| CmdError::Usage(format!("open wallet lock {}: {e}", lock_path.display())))?;
    FileExt::lock_exclusive(&f).map_err(|e| {
        CmdError::Usage(format!("acquire wallet lock {}: {e}", lock_path.display()))
    })?;
    Ok(WalletGuard(f))
}

fn lock_path_for(keystore_path: &Path) -> PathBuf {
    let parent = keystore_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(".taskfast-wallet.lock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn lock_is_exclusive_across_threads() {
        let tmp = TempDir::new().unwrap();
        let keystore = tmp.path().join("keystore.json");
        std::fs::write(&keystore, "{}").unwrap();

        let first = acquire(&keystore).expect("first acquire");

        let (tx, rx) = mpsc::channel();
        let ks = keystore.clone();
        let joiner = thread::spawn(move || {
            // Should block until main releases. We signal when it
            // finally returns so the assertion below can ordering-check.
            let _g = acquire(&ks).expect("second acquire");
            tx.send(()).unwrap();
        });

        // Prove the second thread is blocked: no signal for 100 ms.
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "second acquire must block while first holds the lock"
        );
        drop(first);
        rx.recv_timeout(Duration::from_secs(2))
            .expect("second acquire must unblock after first drops");
        joiner.join().unwrap();
    }

    #[test]
    fn lock_path_sits_beside_keystore() {
        let p = Path::new("/some/dir/agent.keystore.json");
        assert_eq!(
            lock_path_for(p),
            PathBuf::from("/some/dir/.taskfast-wallet.lock")
        );
    }
}
