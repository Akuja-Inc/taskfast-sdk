//! `taskfast bid` — read + mutate operations on bids.
//!
//! Read (am-4yr): `list` over `GET /agents/me/bids`.
//! Worker mutations (am-e3u.8): `create` + `cancel`. No EIP-712 — API key
//! alone authorizes both (`BidRequest { price, pitch? }`; withdraw has no
//! body). Poster-side `accept` / `reject` are still stubbed; they land in
//! am-e3u.11 once escrow delegation (am-4w2) is settled.

use clap::{Parser, Subcommand};
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::TaskFastClient;
use taskfast_client::api::types::BidRequest;
use taskfast_client::map_api_error;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// GET /agents/me/bids — bids placed by this agent.
    List(ListArgs),
    /// Worker: place a bid on an open task.
    Create(CreateArgs),
    /// Worker: withdraw a pending bid.
    Cancel(CancelArgs),
    /// Poster: accept a bid. (Deferred — escrow delegation; see am-4w2.)
    Accept { id: String },
    /// Poster: reject a bid. (Deferred.)
    Reject { id: String },
}

#[derive(Debug, Parser)]
pub struct ListArgs {
    /// Opaque pagination cursor from a previous response's `next_cursor`.
    #[arg(long)]
    pub cursor: Option<String>,

    /// Max items per page. Server enforces its own ceiling; we pass through.
    #[arg(long)]
    pub limit: Option<i64>,
}

#[derive(Debug, Parser)]
pub struct CreateArgs {
    /// Target task UUID.
    pub task_id: String,

    /// Offered price as a decimal string, e.g. `75.00`. Pass-through; server
    /// owns the canonical decimal parsing + min/max policy.
    #[arg(long)]
    pub price: String,

    /// Optional pitch — free-form "why this agent fits" blurb shown to the poster.
    #[arg(long)]
    pub pitch: Option<String>,
}

#[derive(Debug, Parser)]
pub struct CancelArgs {
    /// Bid UUID to withdraw. Must belong to this agent and be `:pending`.
    pub id: String,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::List(args) => list(ctx, args).await,
        Command::Create(args) => create(ctx, args).await,
        Command::Cancel(args) => cancel(ctx, args).await,
        Command::Accept { .. } => Err(CmdError::Unimplemented("taskfast bid accept")),
        Command::Reject { .. } => Err(CmdError::Unimplemented("taskfast bid reject")),
    }
}

async fn list(ctx: &Ctx, args: ListArgs) -> CmdResult {
    let client = ctx.client()?;
    let data = list_bids(&client, &args).await?;
    Ok(Envelope::success(ctx.environment, ctx.dry_run, data))
}

async fn list_bids(
    client: &TaskFastClient,
    args: &ListArgs,
) -> Result<serde_json::Value, CmdError> {
    let resp = match client
        .inner()
        .get_agent_bids(args.cursor.as_deref(), args.limit)
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(json!({
        "bids": resp.data,
        "meta": resp.meta,
    }))
}

async fn create(ctx: &Ctx, args: CreateArgs) -> CmdResult {
    // Fail on bad UUID before any HTTP so typos never cost a round-trip.
    let task_id = Uuid::parse_str(&args.task_id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
    // Reject empty/whitespace-only price upfront. Server owns the decimal
    // validation — we just make sure we're not sending something obviously
    // malformed that would waste a 422.
    if args.price.trim().is_empty() {
        return Err(CmdError::Usage("--price must not be empty".into()));
    }

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_create_bid",
                "task_id": task_id.to_string(),
                "price": args.price,
                "pitch": args.pitch,
            }),
        ));
    }

    let client = ctx.client()?;
    let body = BidRequest {
        price: args.price.clone(),
        pitch: args.pitch.clone(),
    };
    let bid = match client.inner().create_bid(&task_id, &body).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "bid": bid }),
    ))
}

async fn cancel(ctx: &Ctx, args: CancelArgs) -> CmdResult {
    let bid_id = Uuid::parse_str(&args.id)
        .map_err(|e| CmdError::Usage(format!("bid id must be a UUID: {e}")))?;

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_cancel_bid",
                "bid_id": bid_id.to_string(),
            }),
        ));
    }

    let client = ctx.client()?;
    let bid = match client.inner().withdraw_bid(&bid_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "bid": bid }),
    ))
}
