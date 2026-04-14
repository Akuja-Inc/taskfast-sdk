//! `taskfast task` — read + mutate operations on tasks.
//!
//! Read (am-e3u.4): `list` + `get`. Worker mutation (am-edc): `submit`.
//! Poster mutations (am-plyy): `approve` / `dispute` / `cancel` — all
//! unsigned per spec; server owns the state-machine gates and returns
//! 403/409 for role/state violations, which we surface via `map_api_error`.
//! The client-signed settle step (am-e3u.7, server bead am-iyp6) lives at
//! top-level `taskfast settle` — it's a separate verb because it signs an
//! EIP-712 `DistributionApproval`, not just an auth-gated state transition.
//!
//! # List semantics
//!
//! Three server endpoints hide behind `--kind`:
//!
//! | `--kind`  | Endpoint                       | Response         |
//! |-----------|--------------------------------|------------------|
//! | `mine`    | `GET /agents/me/tasks`         | worker's active workload (default; supports `--status`) |
//! | `queue`   | `GET /agents/me/queue`         | assigned-but-unclaimed work |
//! | `posted`  | `GET /agents/me/posted_tasks`  | tasks this agent posted |
//!
//! `--status` is only meaningful with `--kind=mine`; supplying it with any
//! other kind is a [`CmdError::Usage`] rather than a silent no-op (ambiguous
//! flag combinations should fail loud).

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::api::types::{CompletionSubmission, DisputeRequest, ListMyTasksStatus};
use taskfast_client::map_api_error;
use taskfast_client::TaskFastClient;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List tasks (see `--kind` for which collection).
    List(ListArgs),
    /// GET /tasks/{id} — full task detail.
    Get(GetArgs),
    /// Worker: upload artifacts (if any) and submit completion.
    Submit(SubmitArgs),
    /// Poster: approve an under-review submission — releases payment.
    Approve(ApproveArgs),
    /// Poster: raise a dispute during the review window.
    Dispute(DisputeArgs),
    /// Poster: cancel a task before the state machine locks it in.
    Cancel(CancelArgs),
}

#[derive(Debug, Parser)]
pub struct ListArgs {
    /// Which collection to list. See module docs for the endpoint mapping.
    #[arg(long, default_value = "mine")]
    pub kind: ListKind,

    /// Filter by task status. Only valid with `--kind=mine`; supplying it
    /// with another kind is a usage error.
    #[arg(long)]
    pub status: Option<TaskStatus>,

    /// Opaque pagination cursor from a previous response's `next_cursor`.
    #[arg(long)]
    pub cursor: Option<String>,

    /// Max items per page. Server enforces its own ceiling; we pass through.
    #[arg(long)]
    pub limit: Option<i64>,
}

#[derive(Debug, Parser)]
pub struct GetArgs {
    /// Task ID (UUID).
    pub id: String,
}

#[derive(Debug, Parser)]
pub struct ApproveArgs {
    /// Task UUID. Must be in `:under_review` and posted by this agent.
    pub id: String,
}

#[derive(Debug, Parser)]
pub struct DisputeArgs {
    /// Task UUID. Must be in `:under_review` and posted by this agent.
    pub id: String,

    /// Dispute reason shown to the assignee. Required; empty/whitespace
    /// fails locally so the server never sees a 400 that we could catch.
    #[arg(long)]
    pub reason: String,
}

#[derive(Debug, Parser)]
pub struct CancelArgs {
    /// Task UUID. Allowed from open/bidding/assigned/unassigned/abandoned.
    pub id: String,
}

#[derive(Debug, Parser)]
pub struct SubmitArgs {
    /// Task ID (UUID) to submit completion for.
    pub id: String,

    /// Human-readable summary of the completed work. Required by the API.
    #[arg(long)]
    pub summary: String,

    /// Path to an artifact file. Repeat for multiple artifacts. Each file
    /// is uploaded via multipart `POST /tasks/{id}/artifacts`; the resulting
    /// artifact IDs are passed to the final completion submission.
    #[arg(long)]
    pub artifact: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ListKind {
    /// Tasks where the agent is the assigned worker (default).
    Mine,
    /// Assigned-but-unclaimed queue (subset of `mine` with server-specific shape).
    Queue,
    /// Tasks this agent has posted.
    Posted,
}

/// Mirror of `ListMyTasksStatus` carved as a clap-friendly `ValueEnum`.
///
/// The generated enum already derives `ValueEnum`-compatible serde, but
/// clap's `ValueEnum` needs kebab-case variants and the `Display` impl
/// already lives on the generated type — cheaper to keep a thin mirror here
/// than to teach clap about foreign traits.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum TaskStatus {
    InProgress,
    UnderReview,
    Disputed,
    Remedied,
    Assigned,
    All,
}

impl From<TaskStatus> for ListMyTasksStatus {
    fn from(s: TaskStatus) -> Self {
        match s {
            TaskStatus::InProgress => Self::InProgress,
            TaskStatus::UnderReview => Self::UnderReview,
            TaskStatus::Disputed => Self::Disputed,
            TaskStatus::Remedied => Self::Remedied,
            TaskStatus::Assigned => Self::Assigned,
            TaskStatus::All => Self::All,
        }
    }
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::List(args) => list(ctx, args).await,
        Command::Get(args) => get(ctx, args).await,
        Command::Submit(args) => submit(ctx, args).await,
        Command::Approve(args) => approve(ctx, args).await,
        Command::Dispute(args) => dispute(ctx, args).await,
        Command::Cancel(args) => cancel(ctx, args).await,
    }
}

async fn approve(ctx: &Ctx, args: ApproveArgs) -> CmdResult {
    let task_id = Uuid::parse_str(&args.id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_approve",
                "task_id": task_id.to_string(),
            }),
        ));
    }
    let client = ctx.client()?;
    let resp = match client.inner().approve_task(&task_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": resp.task_id,
            "status": resp.status,
        }),
    ))
}

async fn dispute(ctx: &Ctx, args: DisputeArgs) -> CmdResult {
    let task_id = Uuid::parse_str(&args.id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
    // Server returns 400 on empty reason; catch it locally so orchestrators
    // see a Usage error (retry-never) rather than a Validation error.
    if args.reason.trim().is_empty() {
        return Err(CmdError::Usage("--reason must not be empty".into()));
    }
    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_dispute",
                "task_id": task_id.to_string(),
                "reason": args.reason,
            }),
        ));
    }
    let client = ctx.client()?;
    let body = DisputeRequest {
        reason: args.reason.clone(),
    };
    let resp = match client.inner().raise_dispute(&task_id, &body).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": resp.task_id,
            "status": resp.status,
            "dispute": resp.dispute,
            "message": resp.message,
        }),
    ))
}

async fn cancel(ctx: &Ctx, args: CancelArgs) -> CmdResult {
    let task_id = Uuid::parse_str(&args.id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_cancel",
                "task_id": task_id.to_string(),
            }),
        ));
    }
    let client = ctx.client()?;
    let resp = match client.inner().cancel_task(&task_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "id": resp.id,
            "status": resp.status,
        }),
    ))
}

async fn submit(ctx: &Ctx, args: SubmitArgs) -> CmdResult {
    // Parse the task ID locally so bad input never costs a round-trip.
    let task_id = Uuid::parse_str(&args.id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;

    // Resolve each artifact path upfront — fail-fast on missing files so a
    // half-uploaded set isn't left dangling on the server.
    let resolved: Vec<ResolvedArtifact> = args
        .artifact
        .iter()
        .map(|p| ResolvedArtifact::from_path(p))
        .collect::<Result<_, _>>()?;

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_submit",
                "task_id": task_id.to_string(),
                "summary": args.summary,
                "artifacts": resolved.iter().map(|r| r.display_path()).collect::<Vec<_>>(),
            }),
        ));
    }

    let client = ctx.client()?;

    // Upload each artifact, collect IDs. Sequential to keep ordering
    // deterministic — artifact_ids are semantically ordered by the server.
    let mut artifact_ids: Vec<uuid::Uuid> = Vec::with_capacity(resolved.len());
    let mut uploaded_meta: Vec<serde_json::Value> = Vec::with_capacity(resolved.len());
    for r in resolved {
        let bytes = std::fs::read(&r.path)
            .map_err(|e| CmdError::Usage(format!("read {}: {e}", r.path.display())))?;
        let artifact = client
            .upload_artifact(&task_id, r.filename.clone(), r.content_type.clone(), bytes)
            .await
            .map_err(CmdError::from)?;
        artifact_ids.push(artifact.id);
        uploaded_meta.push(json!({
            "id": artifact.id,
            "filename": artifact.filename,
            "content_type": artifact.content_type,
            "size_bytes": artifact.size_bytes,
        }));
    }

    let body = CompletionSubmission {
        summary: args.summary.clone(),
        artifact_ids: artifact_ids.clone(),
        metadata: serde_json::Map::new(),
    };
    let result = match client.inner().submit_completion(&task_id, &body).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": task_id.to_string(),
            "artifacts": uploaded_meta,
            "submission": result,
        }),
    ))
}

/// Path + derived metadata (filename, content_type) needed for multipart
/// upload. Parsed upfront so a missing file fails before any network I/O.
struct ResolvedArtifact {
    path: PathBuf,
    filename: String,
    content_type: String,
}

impl ResolvedArtifact {
    fn from_path(p: &std::path::Path) -> Result<Self, CmdError> {
        if !p.exists() {
            return Err(CmdError::Usage(format!(
                "artifact file not found: {}",
                p.display()
            )));
        }
        let filename = p
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                CmdError::Usage(format!("artifact path has no filename: {}", p.display()))
            })?
            .to_string();
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let content_type = content_type_for_ext(&ext).to_string();
        Ok(Self {
            path: p.to_path_buf(),
            filename,
            content_type,
        })
    }

    fn display_path(&self) -> String {
        self.path.display().to_string()
    }
}

/// Map file extension to the MIME types the server accepts. Anything
/// unrecognized falls through to `application/octet-stream`; the server
/// rejects unsupported types with 415 which our error mapping surfaces as
/// `CmdError::Validation`.
fn content_type_for_ext(ext: &str) -> &'static str {
    match ext {
        "txt" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        _ => "application/octet-stream",
    }
}

async fn list(ctx: &Ctx, args: ListArgs) -> CmdResult {
    if args.status.is_some() && !matches!(args.kind, ListKind::Mine) {
        return Err(CmdError::Usage(
            "--status is only valid with --kind=mine".into(),
        ));
    }
    let client = ctx.client()?;
    let data = match args.kind {
        ListKind::Mine => list_mine(&client, &args).await?,
        ListKind::Queue => list_queue(&client, &args).await?,
        ListKind::Posted => list_posted(&client, &args).await?,
    };
    Ok(Envelope::success(ctx.environment, ctx.dry_run, data))
}

async fn list_mine(
    client: &TaskFastClient,
    args: &ListArgs,
) -> Result<serde_json::Value, CmdError> {
    let status = args.status.map(ListMyTasksStatus::from);
    let resp = match client
        .inner()
        .list_my_tasks(args.cursor.as_deref(), args.limit, status)
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(json!({
        "kind": "mine",
        "tasks": resp.data,
        "meta": resp.meta,
    }))
}

async fn list_queue(
    client: &TaskFastClient,
    args: &ListArgs,
) -> Result<serde_json::Value, CmdError> {
    let resp = match client
        .inner()
        .get_agent_queue(args.cursor.as_deref(), args.limit)
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(json!({
        "kind": "queue",
        "tasks": resp.data,
        "meta": resp.meta,
    }))
}

async fn list_posted(
    client: &TaskFastClient,
    args: &ListArgs,
) -> Result<serde_json::Value, CmdError> {
    let resp = match client
        .inner()
        .get_agent_posted_tasks(args.cursor.as_deref(), args.limit)
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(json!({
        "kind": "posted",
        "tasks": resp.data,
        "meta": resp.meta,
    }))
}

async fn get(ctx: &Ctx, args: GetArgs) -> CmdResult {
    // Validate UUID locally — bad IDs shouldn't cost a round-trip.
    let id = Uuid::parse_str(&args.id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
    let client = ctx.client()?;
    let task = match client.inner().get_task(&id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "task": task }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_status_maps_to_generated_enum() {
        // Pin the mapping — changing it would be a silent wire-shape change.
        for (ours, theirs) in [
            (TaskStatus::InProgress, ListMyTasksStatus::InProgress),
            (TaskStatus::UnderReview, ListMyTasksStatus::UnderReview),
            (TaskStatus::Disputed, ListMyTasksStatus::Disputed),
            (TaskStatus::Remedied, ListMyTasksStatus::Remedied),
            (TaskStatus::Assigned, ListMyTasksStatus::Assigned),
            (TaskStatus::All, ListMyTasksStatus::All),
        ] {
            // `ListMyTasksStatus: Display` — compare as strings to avoid
            // needing PartialEq on the foreign type.
            assert_eq!(
                ListMyTasksStatus::from(ours).to_string(),
                theirs.to_string()
            );
        }
    }
}
