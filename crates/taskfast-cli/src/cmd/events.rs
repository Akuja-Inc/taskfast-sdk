//! `taskfast events` — lifecycle event polling.
//!
//! This slice (am-8z7) exposes a **single-page** read over
//! `GET /api/agents/me/events`. The underlying SDK already offers both
//! `list_events_page` (one-shot) and `stream_events` (long-running); we
//! only surface the one-shot form here because mixing a per-event
//! JSON-lines stream with the CLI's envelope-on-run contract needs a
//! dedicated design pass. Follow mode lands in a later bead.

use clap::{Parser, Subcommand};
use serde_json::json;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_agent::events::list_events_page;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// GET /agents/me/events — one page of lifecycle events.
    Poll(PollArgs),
}

#[derive(Debug, Parser)]
pub struct PollArgs {
    /// Opaque pagination cursor from a previous response's `next_cursor`.
    #[arg(long)]
    pub cursor: Option<String>,

    /// Max events per page. Server enforces its own ceiling; we pass through.
    #[arg(long)]
    pub limit: Option<i64>,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Poll(args) => poll(ctx, args).await,
    }
}

async fn poll(ctx: &Ctx, args: PollArgs) -> CmdResult {
    let client = ctx.client()?;
    let page = list_events_page(&client, args.cursor.as_deref(), args.limit)
        .await
        .map_err(CmdError::from)?;
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "events": page.data,
            "meta": page.meta,
        }),
    ))
}
