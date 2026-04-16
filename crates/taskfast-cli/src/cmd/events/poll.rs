//! `taskfast events poll` — single-page read over `GET /agents/me/events`.
//!
//! The SDK also offers `stream_events` (long-running cursor chase);
//! the polling subcommand stays one-shot because a JSON-lines follow
//! mode doesn't mix with the envelope-per-invocation contract. Live
//! tailing lives in [`super::stream`] over WebSocket instead.

use clap::Parser;
use serde_json::json;

use super::super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_agent::events::list_events_page;

#[derive(Debug, Parser)]
pub struct PollArgs {
    /// Opaque pagination cursor from a previous response's `next_cursor`.
    #[arg(long)]
    pub cursor: Option<String>,

    /// Max events per page. Server enforces its own ceiling; we pass through.
    #[arg(long)]
    pub limit: Option<i64>,
}

pub async fn run(ctx: &Ctx, args: PollArgs) -> CmdResult {
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
