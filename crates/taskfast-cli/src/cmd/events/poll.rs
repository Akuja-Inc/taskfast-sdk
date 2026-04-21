//! `taskfast events poll` — single-page read over `GET /agents/me/events`.
//!
//! The SDK also offers `stream_events` (long-running cursor chase);
//! the polling subcommand stays one-shot because a JSON-lines follow
//! mode doesn't mix with the envelope-per-invocation contract. Live
//! tailing lives in [`super::stream`] over WebSocket instead.
//!
//! # Cursor persistence
//!
//! With `--cursor` absent, the previous invocation's `next_cursor` is
//! read from `<config_dir>/events.cursor` so back-to-back polls in an
//! agent loop resume past the last batch without orchestrator
//! bookkeeping. After a successful poll the new `next_cursor` is
//! written back. `--cursor -` and `TASKFAST_NO_CURSOR_STATE=1` both
//! disable read+write for ad-hoc inspection runs that shouldn't
//! mutate the persisted offset.

use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::json;

use super::super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_agent::events::list_events_page_tolerant;

#[derive(Debug, Parser)]
pub struct PollArgs {
    /// Opaque pagination cursor from a previous response's `next_cursor`.
    /// Pass `-` to bypass the persisted cursor file (no read, no write)
    /// for one-shot inspection without mutating the agent loop's offset.
    #[arg(long)]
    pub cursor: Option<String>,

    /// Max events per page. Defaults to 25 — comfortable batch for an
    /// agent loop tick without dragging in a wall of backlog.
    #[arg(long, default_value_t = 25)]
    pub limit: i64,
}

pub async fn run(ctx: &Ctx, args: PollArgs) -> CmdResult {
    let cursor_state = CursorState::resolve(args.cursor.as_deref(), ctx);
    let effective_cursor = cursor_state.effective_cursor();

    let client = ctx.client()?;
    let page = list_events_page_tolerant(&client, effective_cursor, Some(args.limit))
        .await
        .map_err(CmdError::from)?;

    if cursor_state.persistence_enabled() {
        if let Some(path) = cursor_state.path() {
            persist_cursor(path, page.meta.next_cursor.as_deref(), ctx);
        }
    }

    for item in &page.unparseable {
        tracing::warn!(
            target: "taskfast::events",
            raw = %item.raw,
            error = %item.error,
            "unparseable event surfaced via tolerant decode"
        );
    }

    let unparseable: Vec<serde_json::Value> = page
        .unparseable
        .iter()
        .map(|u| json!({ "raw": u.raw, "error": u.error }))
        .collect();

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "events": page.events,
            "unparseable": unparseable,
            "meta": page.meta,
        }),
    ))
}

/// Filename relative to the config directory. Living next to `config.json`
/// keeps a project's TaskFast state self-contained (one dir to back up,
/// one dir to gitignore) rather than scattering across XDG paths.
const CURSOR_FILENAME: &str = "events.cursor";

/// Sentinel passed via `--cursor -` to bypass the file completely.
const DISABLE_SENTINEL: &str = "-";

/// Env var that disables cursor persistence globally (read + write). Useful
/// for CI or transient debug runs that share a config dir with a real loop.
const DISABLE_ENV: &str = "TASKFAST_NO_CURSOR_STATE";

/// Carries the cursor value the request will use plus, if persistence
/// is enabled, the file path to overwrite with `next_cursor` after the
/// poll succeeds. The two are decoupled because an explicit `--cursor`
/// must still hit the API even when the file path is unavailable
/// (e.g. tests that point `config_path` at `/dev/null`).
struct CursorState {
    /// Cursor sent to the API on this call. None = first page.
    request_cursor: Option<String>,
    /// File path to write `next_cursor` into on success. None = no
    /// persistence (sentinel passed, env opt-out, or no config dir).
    persist_to: Option<PathBuf>,
}

impl CursorState {
    fn resolve(cursor_arg: Option<&str>, ctx: &Ctx) -> Self {
        let env_disabled = std::env::var(DISABLE_ENV).is_ok_and(|v| v == "1");
        let sentinel = matches!(cursor_arg, Some(s) if s == DISABLE_SENTINEL);
        let persist_to = if env_disabled || sentinel {
            None
        } else {
            cursor_path(ctx)
        };

        let request_cursor = match cursor_arg {
            Some(s) if s == DISABLE_SENTINEL => None,
            Some(s) => Some(s.to_string()),
            None => persist_to.as_deref().and_then(read_cursor),
        };

        Self {
            request_cursor,
            persist_to,
        }
    }

    fn effective_cursor(&self) -> Option<&str> {
        self.request_cursor.as_deref()
    }

    fn persistence_enabled(&self) -> bool {
        self.persist_to.is_some()
    }

    fn path(&self) -> Option<&Path> {
        self.persist_to.as_deref()
    }
}

/// Resolve the cursor file location from the config path. `/dev/null` (used
/// by tests) returns None so unit tests don't hit the filesystem.
fn cursor_path(ctx: &Ctx) -> Option<PathBuf> {
    let parent = ctx.config_path.parent()?;
    if parent.as_os_str().is_empty() {
        return Some(PathBuf::from(CURSOR_FILENAME));
    }
    if parent == Path::new("/dev") {
        return None;
    }
    Some(parent.join(CURSOR_FILENAME))
}

fn read_cursor(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Best-effort write. A failure (read-only FS, permission denied) is logged
/// to stderr but does not fail the command — the user already got their
/// page; losing the cursor only costs one redundant poll on next run.
///
/// When the server returns `next_cursor: null` (tip reached) the on-disk
/// cursor is left untouched. Overwriting with an empty string would make
/// the next `events poll` (without `--cursor`) restart from the beginning
/// and replay backlog.
fn persist_cursor(path: &Path, next_cursor: Option<&str>, ctx: &Ctx) {
    let Some(payload) = next_cursor else {
        return;
    };
    if let Some(dir) = path.parent() {
        if let Err(e) = fs::create_dir_all(dir) {
            warn_persist_failed(ctx, path, &e.to_string());
            return;
        }
    }
    if let Err(e) = fs::write(path, payload) {
        warn_persist_failed(ctx, path, &e.to_string());
    }
}

fn warn_persist_failed(ctx: &Ctx, path: &Path, reason: &str) {
    if ctx.quiet {
        return;
    }
    eprintln!(
        "warning: failed to persist events cursor to {}: {reason}",
        path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx_with_config(path: PathBuf) -> Ctx {
        Ctx {
            config_path: path,
            ..Default::default()
        }
    }

    #[test]
    fn cursor_path_uses_config_parent() {
        let ctx = ctx_with_config(PathBuf::from("/tmp/foo/.taskfast/config.json"));
        assert_eq!(
            cursor_path(&ctx),
            Some(PathBuf::from("/tmp/foo/.taskfast/events.cursor"))
        );
    }

    #[test]
    fn cursor_path_skips_dev_null() {
        let ctx = ctx_with_config(PathBuf::from("/dev/null"));
        assert_eq!(cursor_path(&ctx), None);
    }

    #[test]
    fn dash_sentinel_disables_both_request_and_persistence() {
        let ctx = ctx_with_config(PathBuf::from("/tmp/x/.taskfast/config.json"));
        let s = CursorState::resolve(Some("-"), &ctx);
        assert!(!s.persistence_enabled());
        assert_eq!(s.effective_cursor(), None);
    }

    #[test]
    fn explicit_cursor_uses_passed_value() {
        let ctx = ctx_with_config(PathBuf::from("/tmp/x/.taskfast/config.json"));
        let s = CursorState::resolve(Some("abc"), &ctx);
        assert_eq!(s.effective_cursor(), Some("abc"));
        assert!(s.persistence_enabled());
    }

    #[test]
    fn explicit_cursor_still_sent_when_persistence_unavailable() {
        let ctx = ctx_with_config(PathBuf::from("/dev/null"));
        let s = CursorState::resolve(Some("abc"), &ctx);
        assert_eq!(s.effective_cursor(), Some("abc"));
        assert!(!s.persistence_enabled());
    }

    #[test]
    fn dev_null_config_disables_persistence() {
        let ctx = ctx_with_config(PathBuf::from("/dev/null"));
        let s = CursorState::resolve(None, &ctx);
        assert_eq!(s.effective_cursor(), None);
        assert!(!s.persistence_enabled());
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.json");
        let ctx = ctx_with_config(cfg);
        let path = cursor_path(&ctx).unwrap();
        persist_cursor(&path, Some("xyz"), &ctx);
        assert_eq!(read_cursor(&path), Some("xyz".into()));
    }

    #[test]
    fn persist_none_preserves_last_known_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.json");
        let ctx = ctx_with_config(cfg);
        let path = cursor_path(&ctx).unwrap();
        persist_cursor(&path, Some("abc"), &ctx);
        persist_cursor(&path, None, &ctx);
        assert_eq!(read_cursor(&path), Some("abc".into()));
    }
}
