//! `taskfast bid` — read + mutate operations on bids.
//!
//! Read (am-4yr): `list` over `GET /agents/me/bids`.
//! Worker mutations (am-e3u.8): `create` + `cancel`. No EIP-712 — API key
//! alone authorizes both (`BidRequest { price, pitch? }`; withdraw has no
//! body). Poster mutations (am-e3u.11): `accept` + `reject`. No signing —
//! am-4w2 shipped the two-phase deferred-escrow flow, so the API call
//! just locks the bid; the poster signs the on-chain escrow later via the
//! web UI URL surfaced in the response.

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::api::types::{
    BidDetailStatus, BidRejectRequest, BidRejectRequestReason, BidRequest,
};
use taskfast_client::map_api_error;
use taskfast_client::TaskFastClient;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// GET /agents/me/bids — bids placed by this agent.
    List(ListArgs),
    /// Worker: place a bid on an open task.
    Create(CreateArgs),
    /// Worker: withdraw a pending bid.
    Cancel(CancelArgs),
    /// Poster: accept a bid (two-phase; poster signs escrow later via web UI).
    Accept(AcceptArgs),
    /// Poster: reject a pending bid with an optional reason.
    Reject(RejectArgs),
}

#[derive(Debug, Parser)]
pub struct ListArgs {
    /// Opaque pagination cursor from a previous response's `next_cursor`.
    #[arg(long)]
    pub cursor: Option<String>,

    /// Max items per page. Server enforces its own ceiling; we pass through.
    #[arg(long)]
    pub limit: Option<i64>,

    /// Filter results by bid status. Applied client-side — the
    /// `getAgentBids` endpoint has no status query param, so filtering here
    /// avoids a jq/grep fallback at call sites without forcing a spec change.
    #[arg(long)]
    pub status: Option<BidStatusFilter>,
}

/// clap-friendly mirror of the generated `BidDetailStatus` enum. Lives here
/// rather than in the codegen because `ValueEnum` needs `Clone + PartialEq`
/// (which the generated enum already has) plus kebab-case — it's cheaper to
/// maintain a 4-variant mirror than to teach clap about foreign serde attrs.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum BidStatusFilter {
    Pending,
    Accepted,
    Rejected,
    Withdrawn,
}

impl BidStatusFilter {
    fn matches(self, status: BidDetailStatus) -> bool {
        matches!(
            (self, status),
            (Self::Pending, BidDetailStatus::Pending)
                | (Self::Accepted, BidDetailStatus::Accepted)
                | (Self::Rejected, BidDetailStatus::Rejected)
                | (Self::Withdrawn, BidDetailStatus::Withdrawn)
        )
    }
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

#[derive(Debug, Args)]
pub struct AcceptArgs {
    /// Bid UUID to accept. Caller must be the poster of the parent task.
    pub id: String,
}

#[derive(Debug, Args)]
pub struct RejectArgs {
    /// Bid UUID to reject. Caller must be the poster; bid must be `:pending`.
    pub id: String,
    /// Optional reason (<=500 chars), stored on the bid.
    #[arg(long)]
    pub reason: Option<String>,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::List(args) => list(ctx, args).await,
        Command::Create(args) => create(ctx, args).await,
        Command::Cancel(args) => cancel(ctx, args).await,
        Command::Accept(args) => accept(ctx, args).await,
        Command::Reject(args) => reject(ctx, args).await,
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
    // Client-side filter: server has no status query param on /agents/me/bids.
    // Bids lacking a status field (shouldn't happen on real responses) are
    // dropped when a filter is supplied so we don't silently pass them through.
    // `meta` describes the *server* page pre-filter; `filtered_count` is added
    // when a filter applies so consumers can distinguish page semantics from
    // the filtered result count.
    let filter_applied = args.status.is_some();
    let bids = match args.status {
        Some(f) => resp
            .data
            .into_iter()
            .filter(|b| b.status.is_some_and(|s| f.matches(s)))
            .collect::<Vec<_>>(),
        None => resp.data,
    };
    if filter_applied {
        Ok(json!({
            "bids": bids,
            "meta": resp.meta,
            "filtered_count": bids.len(),
        }))
    } else {
        Ok(json!({
            "bids": bids,
            "meta": resp.meta,
        }))
    }
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

async fn accept(ctx: &Ctx, args: AcceptArgs) -> CmdResult {
    let bid_id = Uuid::parse_str(&args.id)
        .map_err(|e| CmdError::Usage(format!("bid id must be a UUID: {e}")))?;

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_accept_bid",
                "bid_id": bid_id.to_string(),
            }),
        ));
    }

    let client = ctx.client()?;
    let resp = match client.inner().accept_bid(&bid_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "bid": resp }),
    ))
}

async fn reject(ctx: &Ctx, args: RejectArgs) -> CmdResult {
    let bid_id = Uuid::parse_str(&args.id)
        .map_err(|e| CmdError::Usage(format!("bid id must be a UUID: {e}")))?;

    // Refuse an explicit but empty --reason pre-HTTP so orchestrators see a
    // never-retry Usage error, not a server-side 4xx. Mirrors task dispute.
    let reason_field = match args.reason.as_deref() {
        Some(s) if s.trim().is_empty() => {
            return Err(CmdError::Usage(
                "--reason must not be empty when passed".into(),
            ));
        }
        Some(s) => Some(
            BidRejectRequestReason::try_from(s)
                .map_err(|e| CmdError::Usage(format!("--reason invalid: {e}")))?,
        ),
        None => None,
    };

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_reject_bid",
                "bid_id": bid_id.to_string(),
                "reason": args.reason,
            }),
        ));
    }

    let client = ctx.client()?;
    let body = BidRejectRequest {
        reason: reason_field,
    };
    let resp = match client.inner().reject_bid(&bid_id, &body).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "bid": resp }),
    ))
}
