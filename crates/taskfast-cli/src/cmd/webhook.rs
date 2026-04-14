//! `taskfast webhook` — configure, test, subscribe, inspect, delete.
//!
//! Replaces `scripts/webhook-setup.sh`. All HTTP work lives in
//! `taskfast_agent::webhooks`; this module is the thin clap + envelope
//! layer.
//!
//! # Secret-file contract
//!
//! The platform returns the signing secret exactly once (see
//! `spec/openapi.yaml:2264-2269`). `register` persists it to
//! `--secret-file` with mode `0600` on unix. If the server returns a
//! null secret (idempotent PUT on existing config), we leave any
//! existing secret file untouched — never silently truncate a
//! persisted secret the caller may still rely on.

use std::fs;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde_json::json;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_agent::webhooks;
use taskfast_client::api::types::WebhookConfigRequest;

/// Default subscription set for a fresh worker. Matches the list
/// `scripts/webhook-setup.sh` hard-coded. Kept here only as a
/// convenience default for `subscribe --default-events`; the
/// authoritative list lives on the server and is surfaced by
/// `webhook subscribe --list` (via `GET /webhooks/subscriptions`).
const DEFAULT_WORKER_EVENTS: &[&str] = &[
    "task_assigned",
    "bid_accepted",
    "bid_rejected",
    "pickup_deadline_warning",
    "payment_held",
    "payment_disbursed",
    "dispute_resolved",
    "review_received",
    "message_received",
];

#[derive(Debug, Subcommand)]
pub enum Command {
    /// PUT /agents/me/webhooks — create or update the endpoint. On first
    /// creation the server returns the HMAC signing secret; pass
    /// `--secret-file` to persist it (chmod 600).
    Register(RegisterArgs),
    /// POST /agents/me/webhooks/test — ask the platform to deliver a
    /// signed test event and report the receipt.
    Test,
    /// PUT /agents/me/webhooks/subscriptions — replace the subscribed
    /// event list.
    Subscribe(SubscribeArgs),
    /// GET /agents/me/webhooks — current config (secret always null).
    Get,
    /// DELETE /agents/me/webhooks — unsubscribe this agent.
    Delete,
}

#[derive(Debug, Parser)]
pub struct RegisterArgs {
    /// HTTPS URL the platform will POST events to.
    #[arg(long)]
    pub url: String,

    /// Path to persist the signing secret when the server returns one.
    /// On unix the file is chmod'd to 0600. Existing files are
    /// overwritten only when a fresh secret is present; a null secret
    /// (idempotent re-register) leaves the file untouched.
    #[arg(long)]
    pub secret_file: Option<PathBuf>,

    /// Subscribe to an event type. Repeat to pass multiple. Optional —
    /// `taskfast webhook subscribe` can be run separately. Bypasses
    /// the subscription step when omitted.
    #[arg(long = "event", value_name = "EVENT")]
    pub events: Vec<String>,
}

#[derive(Debug, Parser)]
pub struct SubscribeArgs {
    /// Event type to subscribe to. Repeat to pass multiple. Mutually
    /// exclusive with `--default-events` and `--list`.
    #[arg(long = "event", value_name = "EVENT", conflicts_with_all = ["default_events", "list"])]
    pub events: Vec<String>,

    /// Subscribe to the built-in worker default set (see
    /// `DEFAULT_WORKER_EVENTS`). Convenience shortcut so orchestrators
    /// don't have to hard-code the list.
    #[arg(long, conflicts_with_all = ["events", "list"])]
    pub default_events: bool,

    /// Just list currently-subscribed + available event types; no
    /// mutation.
    #[arg(long, conflicts_with_all = ["events", "default_events"])]
    pub list: bool,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Register(args) => register(ctx, args).await,
        Command::Test => test(ctx).await,
        Command::Subscribe(args) => subscribe(ctx, args).await,
        Command::Get => get(ctx).await,
        Command::Delete => delete(ctx).await,
    }
}

async fn register(ctx: &Ctx, args: RegisterArgs) -> CmdResult {
    if args.url.trim().is_empty() {
        return Err(CmdError::Usage("--url must not be empty".into()));
    }

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_register",
                "url": args.url,
                "events": args.events,
                "secret_file": args.secret_file.as_ref().map(|p| p.display().to_string()),
            }),
        ));
    }

    let client = ctx.client()?;
    let body = WebhookConfigRequest {
        url: args.url.clone(),
        secret: None,
        // Per spec, `events` here is informational — the server tracks
        // subscriptions independently via the /subscriptions endpoint.
        // Left empty on PUT so re-registering the URL doesn't clobber
        // a carefully-curated subscription list.
        events: None,
    };
    let cfg = webhooks::configure_webhook(&client, &body)
        .await
        .map_err(CmdError::from)?;

    let secret_persisted = match (cfg.secret.as_deref(), args.secret_file.as_ref()) {
        (Some(secret), Some(path)) => {
            persist_secret(path, secret)?;
            true
        }
        _ => false,
    };

    // Optional one-shot subscription. Silent no-op when --event not
    // supplied so `register` stays single-purpose.
    let subscription = if args.events.is_empty() {
        None
    } else {
        let subs = webhooks::update_subscriptions(&client, args.events.clone())
            .await
            .map_err(CmdError::from)?;
        Some(json!({
            "subscribed": subs.subscribed_event_types,
            "available": subs.available_event_types,
        }))
    };

    let mut data = json!({
        "action": "registered",
        "url": cfg.url,
        "events": cfg.events,
        "secret_returned": cfg.secret.is_some(),
        "secret_persisted": secret_persisted,
    });
    if let Some(path) = args.secret_file.as_ref() {
        data["secret_file"] = json!(path.display().to_string());
    }
    if let Some(s) = subscription {
        data["subscription"] = s;
    }
    Ok(Envelope::success(ctx.environment, ctx.dry_run, data))
}

async fn test(ctx: &Ctx) -> CmdResult {
    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({ "action": "would_test" }),
        ));
    }
    let client = ctx.client()?;
    let receipt = webhooks::test_webhook(&client)
        .await
        .map_err(CmdError::from)?;
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "success": receipt.success,
            "status_code": receipt.status_code,
            "message": receipt.message,
        }),
    ))
}

async fn subscribe(ctx: &Ctx, args: SubscribeArgs) -> CmdResult {
    let client = ctx.client()?;

    if args.list {
        let subs = webhooks::get_subscriptions(&client)
            .await
            .map_err(CmdError::from)?;
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "subscribed": subs.subscribed_event_types,
                "available": subs.available_event_types,
            }),
        ));
    }

    let events: Vec<String> = if args.default_events {
        DEFAULT_WORKER_EVENTS
            .iter()
            .map(|e| (*e).to_string())
            .collect()
    } else if !args.events.is_empty() {
        args.events.clone()
    } else {
        return Err(CmdError::Usage(
            "pass --event <NAME> (repeatable), --default-events, or --list".into(),
        ));
    };

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_subscribe",
                "events": events,
            }),
        ));
    }

    let subs = webhooks::update_subscriptions(&client, events)
        .await
        .map_err(CmdError::from)?;
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "subscribed": subs.subscribed_event_types,
            "available": subs.available_event_types,
        }),
    ))
}

async fn get(ctx: &Ctx) -> CmdResult {
    let client = ctx.client()?;
    let cfg = webhooks::get_webhook(&client)
        .await
        .map_err(CmdError::from)?;
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "url": cfg.url,
            "events": cfg.events,
            "created_at": cfg.created_at,
            "updated_at": cfg.updated_at,
        }),
    ))
}

async fn delete(ctx: &Ctx) -> CmdResult {
    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({ "action": "would_delete" }),
        ));
    }
    let client = ctx.client()?;
    webhooks::delete_webhook(&client)
        .await
        .map_err(CmdError::from)?;
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "action": "deleted" }),
    ))
}

/// Write the signing secret with 0600 permissions on unix. The write +
/// chmod pair is *not* atomic — a concurrent reader on the same path
/// could observe the new contents before the mode tightens. The risk
/// is narrow (local agent workflow, single writer) and the shell
/// script had the same property; fixing it would mean a tempfile +
/// rename dance that isn't worth the complexity here.
pub(crate) fn persist_secret(path: &std::path::Path, secret: &str) -> Result<(), CmdError> {
    fs::write(path, secret)
        .map_err(|e| CmdError::Usage(format!("write {}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)
            .map_err(|e| CmdError::Usage(format!("stat {}: {e}", path.display())))?
            .permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms)
            .map_err(|e| CmdError::Usage(format!("chmod {}: {e}", path.display())))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn persist_secret_writes_file_with_tight_perms() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hook.secret");
        persist_secret(&path, "whsec_abc").unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "whsec_abc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "secret file must be 0600");
        }
    }

    #[test]
    fn default_worker_events_match_shell_script_list() {
        // Pin the list: this is the one documented in SKILL.md /
        // BOOT.md and callers may rely on the exact set.
        assert_eq!(
            DEFAULT_WORKER_EVENTS,
            &[
                "task_assigned",
                "bid_accepted",
                "bid_rejected",
                "pickup_deadline_warning",
                "payment_held",
                "payment_disbursed",
                "dispute_resolved",
                "review_received",
                "message_received",
            ]
        );
    }
}
