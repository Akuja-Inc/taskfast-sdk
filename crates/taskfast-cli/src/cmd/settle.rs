//! `taskfast settle` — poster signs an EIP-712 `DistributionApproval` and
//! POSTs it to `/tasks/{id}/settle` to release escrowed funds.
//!
//! Pairs with server bead am-iyp6. The signing surface is identical to the
//! one pinned in `taskfast_chains::tempo::sign_distribution`; the domain
//! (`chain_id`, `verifying_contract`) is sourced at runtime from
//! `GET /agents/me/readiness` so the same binary signs correctly on testnet
//! and mainnet without client-side chain config.
//!
//! Flow: parse UUID → `get_task` (pull `escrow_id` + `settlement_deadline`) →
//! `get_readiness` (pull `settlement_domain`) → load keystore → sign →
//! `POST /tasks/{id}/settle`. Dry-run skips the final POST but still signs
//! so the envelope carries a real signature the caller can audit.
//!
//! Error mapping delegates to the shared `map_api_error` (401|403→Auth,
//! 409|422→Validation); signing/keystore failures surface as `Wallet`
//! (exit 5) via `SigningError`/`KeystoreError` → `CmdError`.

use std::path::PathBuf;
use std::str::FromStr;

use alloy_primitives::{Address, B256, U256};
use clap::Parser;
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_agent::bootstrap;
use taskfast_chains::tempo::{sign_distribution, DistributionDomain};
use taskfast_client::api::types::{SettleTaskBody, SettleTaskBodySignature};
use taskfast_client::map_api_error;

#[derive(Debug, Parser)]
pub struct Args {
    /// Task UUID to settle. Task must have `escrow_id` + `settlement_deadline`
    /// populated (i.e. escrow was created) and be in `:complete` or
    /// `:disbursement_pending` on the server.
    pub task_id: String,

    /// Override the deadline signed into the approval. Defaults to the task's
    /// `settlement_deadline` returned by `GET /tasks/{id}`. Unix seconds.
    #[arg(long)]
    pub deadline_unix: Option<u64>,

    /// Keystore reference. Accepts a bare path (`/.../wallet.json`) or the
    /// `file:/path` form written by `taskfast init` to `TEMPO_KEY_SOURCE`.
    /// Mirrors `taskfast post`'s flag so a single keystore serves both verbs.
    #[arg(long, env = "TEMPO_KEY_SOURCE")]
    pub keystore: Option<String>,

    /// Path to a file holding the keystore password (mode-0400 recommended).
    /// `TASKFAST_WALLET_PASSWORD` env wins over this when both are set so CI
    /// workflows can keep using the env-var form.
    #[arg(long, env = "TASKFAST_WALLET_PASSWORD_FILE")]
    pub wallet_password_file: Option<PathBuf>,

    /// Poster's wallet address (0x-prefixed). When present, we fail early if
    /// the keystore decrypts to a different address — otherwise the server
    /// would 422 after the round-trip. Purely a UX preflight.
    #[arg(long, env = "TEMPO_WALLET_ADDRESS")]
    pub wallet_address: Option<String>,

    /// Acknowledge oversized budgets. Required when the task's budget
    /// exceeds `confirm_above_budget` in the config. Mirrors the same
    /// gate on `taskfast post`; both fail closed before any keystore
    /// prompt or signing.
    #[arg(long)]
    pub yes: bool,
}

pub async fn run(ctx: &Ctx, args: Args) -> CmdResult {
    // 1. Validate UUID upfront — bad input must not cost a round-trip.
    let task_id = Uuid::parse_str(&args.task_id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;

    let client = ctx.client()?;

    // 2. Fetch task detail — we need escrow_id + settlement_deadline.
    let task = match client.inner().get_task(&task_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };

    // Fail-closed budget gate before keystore prompt or signing — a
    // fat-finger settle on a huge task should die immediately.
    ctx.enforce_budget_gate(task.budget_max.as_deref(), args.yes, "settle this task")?;

    let escrow_id_hex: String =
        task.escrow_id
            .as_ref()
            .map(|e| e.to_string())
            .ok_or_else(|| {
                CmdError::Usage(
                    "task has no escrow_id; settle requires an initialized escrow \
                 (task must be in :complete or :disbursement_pending)"
                        .into(),
                )
            })?;

    // 3. Resolve deadline: explicit override wins; otherwise require the
    //    server-stored value. Either can be absent but not both.
    let deadline_unix: u64 = match args.deadline_unix {
        Some(n) => n,
        None => {
            let dt = task.settlement_deadline.ok_or_else(|| {
                CmdError::Usage(
                    "task has no settlement_deadline; either wait for escrow \
                     creation or pass --deadline-unix explicitly"
                        .into(),
                )
            })?;
            // `timestamp()` is i64; clamp to u64 for the uint256. A negative
            // server-side deadline is nonsensical; treat as decode.
            u64::try_from(dt.timestamp()).map_err(|_| {
                CmdError::Decode(format!(
                    "task settlement_deadline is before epoch: {}",
                    dt.to_rfc3339()
                ))
            })?
        }
    };

    // 4. Fetch readiness for the EIP-712 domain (chain_id + verifying_contract).
    //    Both fields are required by the spec, so a malformed response surfaces
    //    as a Decode error from progenitor before reaching this site.
    let readiness = bootstrap::get_readiness(&client)
        .await
        .map_err(CmdError::from)?;
    let domain_spec = readiness.settlement_domain;
    let verifying_contract_str = domain_spec.verifying_contract.to_string();
    let verifying_contract: Address = verifying_contract_str.parse().map_err(|e| {
        CmdError::Decode(format!(
            "readiness returned invalid verifying_contract `{verifying_contract_str}`: {e}"
        ))
    })?;
    let chain_id: u64 = u64::try_from(domain_spec.chain_id).map_err(|_| {
        CmdError::Decode(format!(
            "readiness returned negative chain_id: {}",
            domain_spec.chain_id
        ))
    })?;
    let domain = DistributionDomain::new(chain_id, verifying_contract);

    // 5. Parse escrow_id. The server's schema already regex-validates the
    //    shape; we re-parse locally to produce a `B256` for signing. A parse
    //    failure here would be a server contract violation.
    let escrow_id: B256 = B256::from_str(&escrow_id_hex).map_err(|e| {
        CmdError::Decode(format!(
            "server returned invalid escrow_id `{escrow_id_hex}`: {e}"
        ))
    })?;
    let deadline = U256::from(deadline_unix);

    // 6. Load keystore. Bead spec calls for the signature to appear in the
    //    dry-run envelope, so we sign even in dry-run and never short-circuit
    //    before keystore resolution.
    let keystore_ref = args.keystore.as_deref().map(str::to_string).or_else(|| {
        ctx.keystore_path
            .as_deref()
            .and_then(|p| p.to_str().map(str::to_string))
    });
    let signer = super::wallet_args::load_signer(
        keystore_ref.as_deref(),
        args.wallet_password_file.as_deref(),
        "settlement approval",
    )?;

    // 7. Optional preflight: keystore must match --wallet-address if given.
    //    Without this, a mismatch surfaces as a server 422 after the full
    //    round-trip (including the get_task + readiness calls above).
    let wallet_address_for_check = args
        .wallet_address
        .as_deref()
        .or(ctx.wallet_address.as_deref());
    if let Some(expected) = wallet_address_for_check {
        let expected_addr: Address = expected.parse().map_err(|e| {
            CmdError::Usage(format!("--wallet-address is not a valid EVM address: {e}"))
        })?;
        if signer.address() != expected_addr {
            return Err(CmdError::Usage(format!(
                "keystore address {:#x} does not match --wallet-address {}",
                signer.address(),
                expected
            )));
        }
    }

    // 8. Sign the DistributionApproval. `sign_distribution` returns the
    //    132-char 0x-prefixed `r||s||v` shape the server's verifier expects.
    let signature_hex = sign_distribution(&signer, &domain, escrow_id, deadline)?;

    // 9. Dry-run fork — envelope carries the real signature so callers can
    //    audit it (e.g., `cast wallet verify` or the Elixir verifier fixture)
    //    before committing the live POST.
    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_settle",
                "task_id": task_id.to_string(),
                "escrow_id": escrow_id_hex,
                "deadline": deadline_unix,
                "domain": {
                    "chain_id": chain_id,
                    "verifying_contract": verifying_contract_str,
                },
                "signature": signature_hex,
            }),
        ));
    }

    // 10. Live POST. The regex-validated newtype rejects anything that
    //     doesn't match the spec's `^0x[0-9a-fA-F]{130}$` pattern — a
    //     failure here means `sign_distribution` regressed (crypto-layer
    //     bug), not a network/server issue.
    let signature: SettleTaskBodySignature = signature_hex
        .parse()
        .map_err(|e| CmdError::Signing(format!("signer produced malformed signature hex: {e}")))?;
    let body = SettleTaskBody { signature };
    let resp = match client.inner().settle_task(&task_id, &body).await {
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
