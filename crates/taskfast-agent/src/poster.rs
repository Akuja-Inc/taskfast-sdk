//! Two-phase task creation for poster-role agents.
//!
//! Phase 1 — [`create_task_draft`] — asks the server to prepare an unsigned
//! `payload_to_sign` alongside the ERC-20 token address the submission fee
//! will be pulled from. The caller signs the payload offline with its own
//! private key; the platform never touches it.
//!
//! Phase 2 — [`submit_task_draft`] — hands the signature back. The server
//! verifies it, broadcasts the submission-fee transfer, and mints the task.
//!
//! Both wrappers are thin: they call the generated progenitor client and
//! funnel errors through `map_api_error`. No bootstrap-layer invariants —
//! the two-phase flow is self-describing (draft_id threads through) and the
//! server owns all validation.

use taskfast_client::api::types::{
    TaskDraftPrepareRequest, TaskDraftPrepareResponse, TaskDraftSubmitRequest,
    TaskDraftSubmitResponse,
};
use taskfast_client::{map_api_error, Result, TaskFastClient};
use uuid::Uuid;

/// `POST /task_drafts` — prepare an unsigned task payload for offline signing.
///
/// Returns `{draft_id, payload_to_sign, token_address}`. Caller must sign
/// `payload_to_sign` with the key tied to `poster_wallet_address` and then
/// call [`submit_task_draft`] with the resulting signature.
pub async fn create_task_draft(
    client: &TaskFastClient,
    body: &TaskDraftPrepareRequest,
) -> Result<TaskDraftPrepareResponse> {
    match client.inner().prepare_task_draft(body).await {
        Ok(v) => Ok(v.into_inner()),
        Err(e) => Err(map_api_error(e).await),
    }
}

/// `POST /task_drafts/{draft_id}/submit` — exchange the signed payload for a
/// live task.
///
/// Idempotent: resubmitting with the same `draft_id` returns the
/// already-created task rather than double-minting.
pub async fn submit_task_draft(
    client: &TaskFastClient,
    draft_id: &Uuid,
    body: &TaskDraftSubmitRequest,
) -> Result<TaskDraftSubmitResponse> {
    match client.inner().submit_task_draft(draft_id, body).await {
        Ok(v) => Ok(v.into_inner()),
        Err(e) => Err(map_api_error(e).await),
    }
}
