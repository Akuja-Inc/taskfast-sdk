//! `taskfast post` — two-phase task creation (prepare → sign+broadcast → submit).
//!
//! Supersedes the `scripts/post-task` shell flow whose defect chain is tracked
//! under am-q1m. Server contract (`lib/task_fast/payments/tempo_provider.ex`
//! `charge_submission_fee/4`) accepts **two voucher forms** on
//! `POST /api/task_drafts/{id}/submit`:
//!
//!   - `0x<64hex>` → already-broadcast tx hash; server polls for confirmation.
//!   - any other hex → raw RLP-encoded signed tx; server broadcasts via
//!     `eth_sendRawTransaction`.
//!
//! We take the **tx-hash voucher path**: the client signs + broadcasts the
//! ERC-20 transfer locally, then hands the server just the hash. Rationale:
//! failures on the broadcast side surface to the agent as a concrete RPC
//! error before we pollute the server's draft state with a would-be-bad
//! voucher.
//!
//! `payload_to_sign` (despite its historical name) is **not** an ECDSA signing
//! payload — it's the encoded ERC-20 `transfer` calldata. We wrap it in a
//! replay-safe legacy tx and sign the transaction hash via
//! [`taskfast_agent::tempo_rpc::sign_and_broadcast_erc20_transfer`].

use std::path::PathBuf;

use alloy_primitives::{Address, Bytes};
use clap::Parser;
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_agent::tempo_rpc::{sign_and_broadcast_erc20_transfer, TempoRpcClient};
use taskfast_client::api::types::{
    CompletionCriterionInput, TaskDraftPrepareRequest, TaskDraftPrepareRequestAssignmentType,
    TaskDraftPrepareRequestPosterWalletAddress, TaskDraftSubmitRequest,
    TaskDraftSubmitRequestSignature,
};
use taskfast_client::map_api_error;

/// Canonical Tempo RPC URLs. Mirrors `TempoConstants` on the platform. Kept
/// in sync with `lib/task_fast/payments/tempo_constants.ex` — any drift
/// here would silently point the CLI at the wrong chain.
const TEMPO_MAINNET_RPC: &str = "https://rpc.tempo.xyz";
const TEMPO_TESTNET_RPC: &str = "https://rpc.moderato.tempo.xyz";

#[derive(Debug, Parser)]
pub struct Args {
    /// Task title (required).
    #[arg(long)]
    pub title: String,

    /// Task description. Required by the server; defaults to an empty string
    /// if the caller omits it so the 422 is a server-side signal rather than
    /// a client-side shape error.
    #[arg(long, default_value = "")]
    pub description: String,

    /// Max budget the poster will pay, as a decimal string ("2.50"). Passed
    /// through as `budget_max` on the draft.
    #[arg(long)]
    pub budget: Option<String>,

    /// Capability tags required from the assignee. Comma-separated.
    #[arg(long, value_delimiter = ',')]
    pub capabilities: Vec<String>,

    /// Completion-criterion payout gate as a JSON object. Repeat `--criterion`
    /// for multiple gates. Shape matches `CompletionCriterionInput`:
    /// `{"description":"…","check_type":"json_schema|regex|count|http_status|file_exists",`
    /// `"check_expression":"…","expected_value":"…","target_artifact_pattern":null}`.
    /// Missing ⇒ no objective gate; server policy decides payout.
    #[arg(long = "criterion")]
    pub criteria: Vec<String>,

    /// Path to a JSON file containing an array of `CompletionCriterionInput`
    /// objects. Merged before any `--criterion` flags (file entries first).
    /// Use this when you have many gates or want to keep them in version
    /// control alongside the task.
    #[arg(long)]
    pub criteria_file: Option<PathBuf>,

    /// Pickup deadline (RFC3339 timestamp, e.g. `2026-05-01T00:00:00Z`).
    #[arg(long)]
    pub pickup_deadline: Option<String>,

    /// Execution deadline (RFC3339 timestamp).
    #[arg(long)]
    pub execution_deadline: Option<String>,

    /// Assignment model: `open` (auction to any qualified bidder) or
    /// `direct` (assign to a specific agent; requires `--direct-agent-id`).
    #[arg(long, default_value = "open")]
    pub assignment_type: AssignmentType,

    /// Direct assignment target. Required when `--assignment-type=direct`.
    #[arg(long)]
    pub direct_agent_id: Option<String>,

    /// Poster's on-chain wallet address (0x-prefixed). Matches `TEMPO_WALLET_ADDRESS`
    /// written by `taskfast init`.
    #[arg(long, env = "TEMPO_WALLET_ADDRESS")]
    pub wallet_address: Option<String>,

    /// Keystore reference. Accepts a bare path (`/.../abc.json`) or the
    /// `file:/path` form `taskfast init` writes to `TEMPO_KEY_SOURCE`.
    /// Keychain backends will slot in later under the same flag.
    #[arg(long, env = "TEMPO_KEY_SOURCE")]
    pub keystore: Option<String>,

    /// Path to a file holding the keystore password (mode-0400 recommended).
    /// Preferred over `TASKFAST_WALLET_PASSWORD` — files don't leak via
    /// process args or `/proc/self/environ`.
    #[arg(long, env = "TASKFAST_WALLET_PASSWORD_FILE")]
    pub wallet_password_file: Option<PathBuf>,

    /// Tempo RPC endpoint. Defaults to the canonical URL for `--network`.
    #[arg(long, env = "TEMPO_RPC_URL")]
    pub rpc_url: Option<String>,

    /// Network selector for the default RPC URL.
    #[arg(long, default_value = "mainnet", env = "TEMPO_NETWORK")]
    pub network: Network,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum AssignmentType {
    Open,
    Direct,
}

impl AssignmentType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Direct => "direct",
        }
    }
}

impl From<AssignmentType> for TaskDraftPrepareRequestAssignmentType {
    fn from(a: AssignmentType) -> Self {
        match a {
            AssignmentType::Open => Self::Open,
            AssignmentType::Direct => Self::Direct,
        }
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Network {
    Mainnet,
    Testnet,
}

impl Network {
    fn default_rpc_url(self) -> &'static str {
        match self {
            Self::Mainnet => TEMPO_MAINNET_RPC,
            Self::Testnet => TEMPO_TESTNET_RPC,
        }
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> CmdResult {
    let wallet_address = args.wallet_address.as_deref().ok_or_else(|| {
        CmdError::Usage("--wallet-address (or TEMPO_WALLET_ADDRESS) required to post a task".into())
    })?;
    // Validate shape upfront so a typo never makes it to the server.
    let _: Address = wallet_address.parse().map_err(|e| {
        CmdError::Usage(format!("--wallet-address is not a valid EVM address: {e}"))
    })?;

    let direct_agent_id = match (args.assignment_type, args.direct_agent_id.as_deref()) {
        (AssignmentType::Direct, Some(s)) => Some(
            Uuid::parse_str(s)
                .map_err(|e| CmdError::Usage(format!("--direct-agent-id not a UUID: {e}")))?,
        ),
        (AssignmentType::Direct, None) => {
            return Err(CmdError::Usage(
                "--assignment-type=direct requires --direct-agent-id".into(),
            ));
        }
        (AssignmentType::Open, _) => None,
    };

    let pickup_deadline = parse_iso_opt(args.pickup_deadline.as_deref(), "--pickup-deadline")?;
    let execution_deadline =
        parse_iso_opt(args.execution_deadline.as_deref(), "--execution-deadline")?;

    let poster_wallet: TaskDraftPrepareRequestPosterWalletAddress = wallet_address
        .parse()
        .map_err(|e| CmdError::Usage(format!("--wallet-address rejected by schema: {e}")))?;

    let completion_criteria = resolve_criteria(args.criteria_file.as_deref(), &args.criteria)?;

    let prep_body = TaskDraftPrepareRequest {
        assignment_type: args.assignment_type.into(),
        budget_max: args.budget.clone(),
        completion_criteria,
        description: args.description.clone(),
        direct_agent_id,
        execution_deadline,
        pickup_deadline,
        poster_wallet_address: poster_wallet,
        required_capabilities: args.capabilities.clone(),
        title: args.title.clone(),
    };

    let rpc_url = args
        .rpc_url
        .clone()
        .unwrap_or_else(|| args.network.default_rpc_url().to_string());

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_post",
                "draft_id": serde_json::Value::Null,
                "title": args.title,
                "assignment_type": args.assignment_type.as_str(),
                "budget_max": args.budget,
                "required_capabilities": args.capabilities,
                "completion_criteria_count": prep_body.completion_criteria.len(),
                "rpc_url": rpc_url,
                "wallet_address": wallet_address,
            }),
        ));
    }

    let client = ctx.client()?;

    // Phase 1 — prepare. Server returns ERC-20 transfer calldata + draft_id.
    let prep = match client.inner().prepare_task_draft(&prep_body).await {
        Ok(r) => r.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };

    let token_address: Address = prep.token_address.parse().map_err(|e| {
        CmdError::Server(format!(
            "server returned invalid token_address `{}`: {e}",
            prep.token_address
        ))
    })?;
    let calldata = decode_0x_bytes(&prep.payload_to_sign)
        .map_err(|e| CmdError::Server(format!("server returned invalid payload_to_sign: {e}")))?;

    // Load signer only after prepare succeeds — avoids prompting the user for
    // a keystore password on a request that never leaves local validation.
    let signer = super::wallet_args::load_signer(
        args.keystore.as_deref(),
        args.wallet_password_file.as_deref(),
        "submission fee",
    )?;
    // Sanity: the wallet address in the draft must match what we're signing
    // with. A mismatch means the server recorded a charge on a wallet we
    // don't control, which would poll forever.
    let parsed_wallet_address = wallet_address
        .parse::<Address>()
        .map_err(|_| CmdError::Usage(format!("invalid wallet address: {}", wallet_address)))?;
    if signer.address() != parsed_wallet_address {
        return Err(CmdError::Usage(format!(
            "keystore address {:#x} does not match --wallet-address {}",
            signer.address(),
            wallet_address
        )));
    }

    let rpc = TempoRpcClient::new(reqwest::Client::new(), rpc_url.clone());
    let tx_hash =
        sign_and_broadcast_erc20_transfer(&rpc, &signer, token_address, Bytes::from(calldata))
            .await
            .map_err(|e| CmdError::Server(format!("tempo rpc: {e}")))?;
    let tx_hash_hex = format!("{tx_hash:#x}");

    // Phase 2 — submit voucher. The field is named `signature` for historical
    // reasons; semantically it's a voucher (tx hash, in our path).
    let signature: TaskDraftSubmitRequestSignature = tx_hash_hex
        .parse()
        .map_err(|e| CmdError::Server(format!("tx hash rejected by schema pattern: {e}")))?;
    let submit_body = TaskDraftSubmitRequest { signature };
    let submitted = match client
        .inner()
        .submit_task_draft(&prep.draft_id, &submit_body)
        .await
    {
        Ok(r) => r.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": submitted.id,
            "draft_id": prep.draft_id,
            "submission_fee_tx_hash": tx_hash_hex,
            "status": submitted.status,
            "submission_fee_status": submitted.submission_fee_status,
        }),
    ))
}

/// Merge file-sourced and inline `--criterion` payloads into one validated
/// list of `CompletionCriterionInput`. File entries are prepended so a
/// shared base file can be augmented with one-off overrides on the command
/// line. Any shape/parse failure is a `Usage` error — the CLI catches it
/// before the request ever reaches the server, so the exit-code contract
/// (Auth=re-credential, Validation=server-rejected payload) stays clean.
fn resolve_criteria(
    file: Option<&std::path::Path>,
    inline: &[String],
) -> Result<Vec<CompletionCriterionInput>, CmdError> {
    let mut out = Vec::with_capacity(inline.len());
    if let Some(path) = file {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            CmdError::Usage(format!("read --criteria-file {}: {e}", path.display()))
        })?;
        let parsed: Vec<CompletionCriterionInput> = serde_json::from_str(&raw).map_err(|e| {
            CmdError::Usage(format!(
                "--criteria-file {} is not a JSON array of CompletionCriterionInput: {e}",
                path.display()
            ))
        })?;
        out.extend(parsed);
    }
    for (i, raw) in inline.iter().enumerate() {
        let one: CompletionCriterionInput = serde_json::from_str(raw)
            .map_err(|e| CmdError::Usage(format!("--criterion[{i}] not valid JSON: {e}")))?;
        out.push(one);
    }
    Ok(out)
}

fn parse_iso_opt(
    s: Option<&str>,
    flag: &str,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, CmdError> {
    match s {
        None => Ok(None),
        Some(raw) => chrono::DateTime::parse_from_rfc3339(raw)
            .map(|d| Some(d.with_timezone(&chrono::Utc)))
            .map_err(|e| CmdError::Usage(format!("{flag} not a valid RFC3339 timestamp: {e}"))),
    }
}

fn decode_0x_bytes(s: &str) -> Result<Vec<u8>, String> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    hex::decode(stripped).map_err(|e| e.to_string())
}
