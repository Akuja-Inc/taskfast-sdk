//! `taskfast escrow sign <bid_id>` — headless poster escrow signing.
//!
//! Replaces the web-UI-only wagmi + passkey path at `assets/js/escrow_sign.js`.
//! Picks up bids the server parked in `:accepted_pending_escrow` after
//! `taskfast bid accept` and drives them to `:accepted` without a browser:
//!
//!  1. `GET /bids/:id/escrow/params` → server-derived on-chain params.
//!  2. `GET /agents/me/readiness` → EIP-712 `DistributionDomain`.
//!  3. ERC-20 `allowance` preflight; `approve` if short.
//!  4. Random 32-byte salt → `compute_escrow_id` (matches Solidity).
//!  5. EIP-712 `DistributionApproval(escrowId, deadline)` — `sign_distribution`.
//!  6. `TaskEscrow.open` / `openWithMemo` — broadcast + wait for receipt.
//!  7. `POST /bids/:id/escrow/finalize` with voucher + sig + deadline.
//!
//! Memo is server-driven: `memo_hash` in the params payload selects
//! `openWithMemo` vs `open`. No `--memo` flag.
//!
//! Error mapping delegates to `map_api_error` (401|403→Auth, 409|422→
//! Validation). Keystore / signing failures surface as `Wallet` (exit 5).

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use clap::Parser;
use serde_json::json;
use uuid::Uuid;

use super::{network_policy_for_chain_id, validate_override_rpc_url, CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_agent::bootstrap;
use taskfast_agent::chain::{compute_escrow_id, TaskEscrow, IERC20};
use taskfast_agent::tempo_rpc::{sign_and_broadcast_tx, TempoRpcClient};
use taskfast_chains::tempo::{sign_distribution, DistributionDomain};
use taskfast_client::api::types::{
    BidEscrowFinalizeRequest, BidEscrowFinalizeRequestPosterApprovalDeadline,
};
use taskfast_client::map_api_error;

/// Tempo chain IDs — must match `DistributionDomain::mainnet`/`testnet`.
/// Used by the network-aware receipt-timeout default. RPC URLs are no
/// longer hardcoded here; they come from `GET /config/network`.
const TEMPO_MAINNET_CHAIN_ID: i64 = 4217;
const TEMPO_TESTNET_CHAIN_ID: i64 = 42_431;

/// Poster approval deadline horizon — built-in default when neither the
/// `--approval-horizon` flag nor `approval_horizon` in the config file
/// is set. 7 days tightens the blast radius on a leaked keystore vs the
/// prior 30-day hardcode; CI flows that need the old window set
/// `TASKFAST_APPROVAL_HORIZON=30d` or the config field.
const DEFAULT_APPROVAL_HORIZON: Duration = Duration::from_hours(24 * 7);

/// `TaskEscrow.open()` receipt polling defaults. Mainnet gets a 3min
/// ceiling to ride out block-time jitter under congestion; testnet keeps
/// the prior 1min budget. Unknown chain_ids fall back to 1min.
const DEFAULT_RECEIPT_TIMEOUT_MAINNET: Duration = Duration::from_mins(3);
const DEFAULT_RECEIPT_TIMEOUT_TESTNET: Duration = Duration::from_mins(1);
const RECEIPT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Network-aware default receipt-timeout used when the caller supplies
/// neither `--receipt-timeout` nor `receipt_timeout` in config.
fn default_receipt_timeout(chain_id: i64) -> Duration {
    match chain_id {
        TEMPO_MAINNET_CHAIN_ID => DEFAULT_RECEIPT_TIMEOUT_MAINNET,
        TEMPO_TESTNET_CHAIN_ID => DEFAULT_RECEIPT_TIMEOUT_TESTNET,
        _ => DEFAULT_RECEIPT_TIMEOUT_TESTNET,
    }
}

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Sign + broadcast the on-chain escrow for a deferred bid; POST finalize.
    Sign(SignArgs),
}

#[derive(Debug, Parser)]
pub struct SignArgs {
    /// Bid UUID. Bid must be in `:accepted_pending_escrow`; caller must be
    /// the parent task's poster.
    pub bid_id: String,

    /// Keystore reference (same form as `taskfast post` / `settle`).
    #[arg(long, env = "TEMPO_KEY_SOURCE")]
    pub keystore: Option<String>,

    /// Path to keystore password file.
    #[arg(long, env = "TASKFAST_WALLET_PASSWORD_FILE")]
    pub wallet_password_file: Option<PathBuf>,

    /// Poster wallet address preflight (0x-prefixed). When set, we fail
    /// before touching the chain if the keystore decrypts to a mismatch.
    #[arg(long, env = "TEMPO_WALLET_ADDRESS")]
    pub wallet_address: Option<String>,

    /// Tempo RPC override. Defaults to the deployment's authenticated
    /// proxy URL for `params.chain_id` (reverse-looked-up in the
    /// `GET /config/network` map returned by the backend).
    #[arg(long, env = "TEMPO_RPC_URL")]
    pub rpc_url: Option<String>,

    /// Skip the on-chain `allowance` preflight + `approve` tx. Only safe when
    /// the caller already granted a sufficient allowance out-of-band.
    #[arg(long)]
    pub skip_allowance_check: bool,

    /// Poster approval-deadline horizon — accepts human durations like
    /// `7d`, `24h`, `3600s`. Falls back to `approval_horizon` in config,
    /// then the built-in 7-day default.
    #[arg(
        long,
        env = "TASKFAST_APPROVAL_HORIZON",
        value_parser = humantime::parse_duration,
    )]
    pub approval_horizon: Option<Duration>,

    /// Receipt-polling ceiling — human duration (`3min`, `90s`). Falls
    /// back to `receipt_timeout` in config, then a chain-aware default
    /// (3min mainnet, 1min testnet).
    #[arg(
        long,
        env = "TASKFAST_RECEIPT_TIMEOUT",
        value_parser = humantime::parse_duration,
    )]
    pub receipt_timeout: Option<Duration>,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Sign(a) => sign(ctx, a).await,
    }
}

async fn sign(ctx: &Ctx, args: SignArgs) -> CmdResult {
    // 1. Validate bid UUID locally.
    let bid_id = Uuid::parse_str(&args.bid_id)
        .map_err(|e| CmdError::Usage(format!("bid id must be a UUID: {e}")))?;

    let client = ctx.client()?;

    // 2. Fetch escrow params — server enforces poster-auth + bid status.
    let params = match client.inner().get_bid_escrow_params(&bid_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };

    // 3. Readiness → EIP-712 domain. settlement_domain must be populated
    //    and cross-consistent with params.chain_id.
    let readiness = bootstrap::get_readiness(&client)
        .await
        .map_err(CmdError::from)?;
    let domain_spec = readiness.settlement_domain;
    if domain_spec.chain_id != params.chain_id {
        return Err(CmdError::Decode(format!(
            "readiness chain_id={} disagrees with escrow params chain_id={}",
            domain_spec.chain_id, params.chain_id
        )));
    }
    let verifying_contract_str = domain_spec.verifying_contract.to_string();
    let verifying_contract: Address = verifying_contract_str.parse().map_err(|e| {
        CmdError::Decode(format!(
            "readiness returned invalid verifying_contract `{verifying_contract_str}`: {e}"
        ))
    })?;
    let chain_id_u64 = u64::try_from(params.chain_id).map_err(|_| {
        CmdError::Decode(format!(
            "escrow params chain_id={} is negative",
            params.chain_id
        ))
    })?;
    let domain = DistributionDomain::new(chain_id_u64, verifying_contract);

    // 4. Parse addresses + scale decimal amounts to U256 raw units.
    let token_address: Address = params.token_address.parse().map_err(|e| {
        CmdError::Decode(format!(
            "server token_address `{}` not a valid EVM address: {e}",
            params.token_address
        ))
    })?;
    let task_escrow: Address = params.task_escrow_contract.parse().map_err(|e| {
        CmdError::Decode(format!(
            "server task_escrow_contract `{}` not a valid EVM address: {e}",
            params.task_escrow_contract
        ))
    })?;
    let worker: Address = params.worker_address.parse().map_err(|e| {
        CmdError::Decode(format!(
            "server worker_address `{}` not a valid EVM address: {e}",
            params.worker_address
        ))
    })?;
    let platform_wallet: Address = params.platform_wallet.parse().map_err(|e| {
        CmdError::Decode(format!(
            "server platform_wallet `{}` not a valid EVM address: {e}",
            params.platform_wallet
        ))
    })?;

    // Cross-check: the readiness domain's verifying_contract must equal the
    // task_escrow contract returned by params. A mismatch means the server
    // is stitching two different chain configs — signing against that would
    // yield an unrecoverable signature.
    if verifying_contract != task_escrow {
        return Err(CmdError::Decode(format!(
            "readiness verifying_contract={verifying_contract:#x} \
             disagrees with escrow params task_escrow_contract={task_escrow:#x}"
        )));
    }

    let decimals = u8::try_from(params.decimals).map_err(|_| {
        CmdError::Decode(format!(
            "escrow params decimals={} out of u8 range",
            params.decimals
        ))
    })?;
    let deposit = decimal_to_u256(&params.amount, decimals)?;
    let platform_fee = decimal_to_u256(&params.platform_fee_amount, decimals)?;

    // 5. Optional memo — pass through exactly; server re-derives authoritative
    //    memo on finalize, so a client mismatch is a Validation error there.
    let memo_hash_opt: Option<B256> = params
        .memo_hash
        .as_deref()
        .map(|s| {
            B256::from_str(s).map_err(|e| {
                CmdError::Decode(format!(
                    "server memo_hash `{s}` not a 0x-prefixed 32-byte hex: {e}"
                ))
            })
        })
        .transpose()?;

    // 6. Load signer + preflight address equality.
    let keystore_ref = args.keystore.as_deref().map(str::to_string).or_else(|| {
        ctx.keystore_path
            .as_deref()
            .and_then(|p| p.to_str().map(str::to_string))
    });
    let signer = super::wallet_args::load_signer(
        keystore_ref.as_deref(),
        args.wallet_password_file.as_deref(),
        "escrow approval",
    )?;
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

    // 7. Derive random salt + predicted escrow_id. Must byte-match Solidity
    //    `TaskEscrow.computeEscrowId`. See
    //    `taskfast-agent::chain::compute_escrow_id` for the caveat on
    //    fee-on-transfer tokens.
    let salt = B256::from(rand::random::<[u8; 32]>());
    let escrow_id = compute_escrow_id(
        signer.address(),
        worker,
        token_address,
        deposit,
        platform_fee,
        platform_wallet,
        salt,
    );

    // 8. Sign DistributionApproval(escrowId, deadline). Deadline is absolute
    //    seconds. Horizon precedence: flag > config > 7d built-in default.
    let approval_horizon = super::resolve_duration(
        args.approval_horizon,
        ctx.approval_horizon,
        DEFAULT_APPROVAL_HORIZON,
    );
    let deadline_unix = u64::try_from(chrono::Utc::now().timestamp())
        .map_err(|_| CmdError::Decode("system clock before epoch".into()))?
        .saturating_add(approval_horizon.as_secs());
    let deadline = U256::from(deadline_unix);
    let signature_hex = sign_distribution(&signer, &domain, escrow_id, deadline)?;

    // 9. Build open / openWithMemo calldata up front — we reuse it for the
    //    dry-run envelope and the live broadcast.
    let open_calldata: Bytes = if let Some(memo_hash) = memo_hash_opt {
        TaskEscrow::openWithMemoCall {
            token: token_address,
            deposit,
            worker,
            platformFeeAmount: platform_fee,
            platform: platform_wallet,
            salt,
            memoHash: memo_hash,
        }
        .abi_encode()
        .into()
    } else {
        TaskEscrow::openCall {
            token: token_address,
            deposit,
            worker,
            platformFeeAmount: platform_fee,
            platform: platform_wallet,
            salt,
        }
        .abi_encode()
        .into()
    };

    // Resolve RPC URL. Override flows to a bare upstream gateway; default
    // path pulls the deployment's proxy URL from `GET /config/network`
    // (reverse-lookup by chain_id, so the selected entry matches the
    // server-returned escrow params). The proxy URL is sanity-checked to
    // live under the authenticated api_base — catches a misconfigured (or
    // malicious) deployment returning an off-host upstream.
    let (rpc_url, _via_proxy) = if let Some(ref url) = args.rpc_url {
        validate_override_rpc_url(
            url,
            network_policy_for_chain_id(chain_id_u64),
            ctx.allow_custom_endpoints,
        )?;
        (url.clone(), false)
    } else {
        let cfg = client.fetch_network_config().await.map_err(|e| {
            CmdError::Server(format!("fetch network config from {}: {e}", ctx.base_url()))
        })?;
        let (_name, entry) = cfg.entry_by_chain_id(params.chain_id).map_err(|e| {
            CmdError::Server(format!(
                "deployment at {} does not advertise chain_id={}: {e}",
                ctx.base_url(),
                params.chain_id
            ))
        })?;
        let expected_prefix = format!("{}/rpc/", ctx.base_url().trim_end_matches('/'));
        if !entry.rpc_url.starts_with(&expected_prefix) {
            return Err(CmdError::Server(format!(
                "deployment at {} returned rpc_url {:?} for chain_id={}, \
                 which does not live under `{expected_prefix}…`. Refusing \
                 to route RPC traffic off-host.",
                ctx.base_url(),
                entry.rpc_url,
                params.chain_id,
            )));
        }
        (entry.rpc_url.clone(), true)
    };

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_sign_escrow",
                "bid_id": bid_id.to_string(),
                "task_id": params.task_id.to_string(),
                "escrow_id": format!("{escrow_id:#x}"),
                "salt": format!("{salt:#x}"),
                "deadline": deadline_unix,
                "signature": signature_hex,
                "open_calldata": format!("0x{}", hex::encode(&open_calldata)),
                "memo_hash": params.memo_hash,
                "rpc_url": rpc_url,
                "domain": {
                    "chain_id": chain_id_u64,
                    "verifying_contract": format!("{verifying_contract:#x}"),
                },
            }),
        ));
    }

    // Receipt-timeout precedence: flag > config > network-aware default.
    // Resolved here (post-`params.chain_id`) so both approve + open receipts
    // share the same ceiling.
    let receipt_timeout = super::resolve_duration(
        args.receipt_timeout,
        ctx.receipt_timeout,
        default_receipt_timeout(params.chain_id),
    );

    // 10. Live RPC: allowance preflight + optional approve.
    //     TaskEscrow.open calls transferFrom for `deposit + platformFeeAmount` in a
    //     single call, so balance/allowance/approve must cover the sum — not
    //     just deposit.
    let total_required = deposit
        .checked_add(platform_fee)
        .ok_or_else(|| CmdError::Decode("deposit + platform_fee overflow U256".into()))?;
    // Pick the http client by URL prefix (any URL on `{api_base}/rpc/`
    // is our authenticated proxy and needs `X-API-Key`); see
    // `Ctx::rpc_http_client` for the rationale. A `--rpc-url` override that
    // happens to point at the proxy still gets the authenticated client.
    let http = ctx.rpc_http_client(&client, &rpc_url);
    let rpc = TempoRpcClient::new(http, rpc_url.clone());
    let mut approval_tx_hex: Option<String> = None;
    if !args.skip_allowance_check {
        let balance = erc20_balance_of(&rpc, token_address, signer.address()).await?;
        if balance < total_required {
            return Err(CmdError::Usage(format!(
                "poster balance {balance} < required {total_required} (deposit {deposit} + fee {platform_fee}) on token {token_address:#x}"
            )));
        }
        let current_allowance =
            erc20_allowance(&rpc, token_address, signer.address(), task_escrow).await?;
        if current_allowance < total_required {
            let approve_calldata: Bytes = IERC20::approveCall {
                spender: task_escrow,
                amount: total_required,
            }
            .abi_encode()
            .into();
            let approve_hash =
                sign_and_broadcast_tx(&rpc, &signer, token_address, approve_calldata)
                    .await
                    .map_err(|e| CmdError::Server(format!("approve broadcast failed: {e}")))?;
            let ok = rpc
                .wait_for_receipt(approve_hash, receipt_timeout, RECEIPT_POLL_INTERVAL)
                .await
                .map_err(|e| CmdError::Server(format!("approve receipt: {e}")))?;
            if !ok {
                return Err(CmdError::Server(format!(
                    "approve tx {approve_hash:#x} reverted on-chain"
                )));
            }
            let new_allowance =
                erc20_allowance(&rpc, token_address, signer.address(), task_escrow).await?;
            if new_allowance < total_required {
                return Err(CmdError::Server(format!(
                    "allowance still {new_allowance} after approve tx {approve_hash:#x} (needed {total_required})"
                )));
            }
            approval_tx_hex = Some(format!("{approve_hash:#x}"));
        }
    }

    // 11. Broadcast TaskEscrow.open / openWithMemo, wait for receipt.
    let voucher_tx = sign_and_broadcast_tx(&rpc, &signer, task_escrow, open_calldata)
        .await
        .map_err(|e| CmdError::Server(format!("open broadcast failed: {e}")))?;
    let voucher_ok = rpc
        .wait_for_receipt(voucher_tx, receipt_timeout, RECEIPT_POLL_INTERVAL)
        .await
        .map_err(|e| CmdError::Server(format!("open receipt: {e}")))?;
    if !voucher_ok {
        return Err(CmdError::Server(format!(
            "open tx {voucher_tx:#x} reverted on-chain — finalize aborted"
        )));
    }
    let voucher_hex = format!("{voucher_tx:#x}");

    // 12. POST finalize.
    let body = BidEscrowFinalizeRequest {
        voucher: voucher_hex.clone(),
        poster_approval_signature: signature_hex.clone(),
        poster_approval_deadline: BidEscrowFinalizeRequestPosterApprovalDeadline::Variant0(
            deadline_unix as i64,
        ),
        memo_hash: params.memo_hash.clone(),
    };
    let resp = match client.inner().finalize_bid_escrow(&bid_id, &body).await {
        Ok(r) => r.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "bid_id": resp.bid_id,
            "task_id": resp.task_id,
            "status": resp.status,
            "task_status": resp.task_status,
            "escrow_id": format!("{escrow_id:#x}"),
            "voucher_tx_hash": voucher_hex,
            "approval_tx_hash": approval_tx_hex,
            "deadline": deadline_unix,
        }),
    ))
}

async fn erc20_balance_of(
    rpc: &TempoRpcClient,
    token: Address,
    owner: Address,
) -> Result<U256, CmdError> {
    let calldata: Bytes = IERC20::balanceOfCall { account: owner }.abi_encode().into();
    let raw = rpc
        .eth_call(token, &calldata)
        .await
        .map_err(|e| CmdError::Server(format!("balanceOf rpc: {e}")))?;
    decode_u256(&raw, "balanceOf")
}

async fn erc20_allowance(
    rpc: &TempoRpcClient,
    token: Address,
    owner: Address,
    spender: Address,
) -> Result<U256, CmdError> {
    let calldata: Bytes = IERC20::allowanceCall { owner, spender }.abi_encode().into();
    let raw = rpc
        .eth_call(token, &calldata)
        .await
        .map_err(|e| CmdError::Server(format!("allowance rpc: {e}")))?;
    decode_u256(&raw, "allowance")
}

fn decode_u256(bytes: &[u8], label: &str) -> Result<U256, CmdError> {
    if bytes.len() < 32 {
        return Err(CmdError::Decode(format!(
            "{label} returned {} bytes, expected >=32",
            bytes.len()
        )));
    }
    Ok(U256::from_be_slice(&bytes[..32]))
}

/// Scale a decimal string (`"75.00"`) to raw U256 token units given `decimals`.
/// Rejects negatives, exponential notation, and fractional digits that exceed
/// `decimals` (would silently truncate user intent).
fn decimal_to_u256(s: &str, decimals: u8) -> Result<U256, CmdError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(CmdError::Decode("empty decimal amount".into()));
    }
    if trimmed.starts_with('-') {
        return Err(CmdError::Decode(format!(
            "negative amount `{s}` disallowed"
        )));
    }
    let (whole, frac) = match trimmed.split_once('.') {
        Some((w, f)) => (w, f),
        None => (trimmed, ""),
    };
    let frac_len = frac.len();
    if frac_len > decimals as usize {
        return Err(CmdError::Decode(format!(
            "amount `{s}` has {frac_len} fractional digits but token has only {decimals}"
        )));
    }
    let mut combined = String::with_capacity(whole.len() + decimals as usize);
    combined.push_str(whole);
    combined.push_str(frac);
    for _ in 0..(decimals as usize - frac_len) {
        combined.push('0');
    }
    // Strip leading zeros but keep at least one digit so "0" parses.
    let stripped = combined.trim_start_matches('0');
    let digits = if stripped.is_empty() { "0" } else { stripped };
    U256::from_str_radix(digits, 10)
        .map_err(|e| CmdError::Decode(format!("amount `{s}` not parseable as integer: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_to_u256_scales_basic() {
        assert_eq!(
            decimal_to_u256("75.00", 6).unwrap(),
            U256::from(75_000_000u64)
        );
        assert_eq!(decimal_to_u256("75", 6).unwrap(), U256::from(75_000_000u64));
        assert_eq!(decimal_to_u256("0.5", 6).unwrap(), U256::from(500_000u64));
        assert_eq!(decimal_to_u256("0", 6).unwrap(), U256::ZERO);
    }

    #[test]
    fn decimal_to_u256_rejects_excess_fractional_digits() {
        let err = decimal_to_u256("1.1234567", 6).expect_err("7 > 6 must fail");
        matches!(err, CmdError::Decode(_));
    }

    #[test]
    fn decimal_to_u256_rejects_negative() {
        let err = decimal_to_u256("-1.00", 6).expect_err("negative must fail");
        matches!(err, CmdError::Decode(_));
    }

    #[test]
    fn chain_id_constants_match_tempo_protocol() {
        // Pinned against server truth (`lib/task_fast/payments/tempo_constants.ex`)
        // and the `DistributionDomain` domain-separator consumer. Drift here
        // would desync the EIP-712 signing domain from the on-chain verifier.
        assert_eq!(TEMPO_MAINNET_CHAIN_ID, 4_217);
        assert_eq!(TEMPO_TESTNET_CHAIN_ID, 42_431);
    }

    #[test]
    fn default_receipt_timeout_mainnet_is_three_minutes() {
        assert_eq!(
            default_receipt_timeout(TEMPO_MAINNET_CHAIN_ID),
            Duration::from_mins(3),
        );
    }

    #[test]
    fn default_receipt_timeout_testnet_is_one_minute() {
        assert_eq!(
            default_receipt_timeout(TEMPO_TESTNET_CHAIN_ID),
            Duration::from_mins(1),
        );
    }

    #[test]
    fn default_receipt_timeout_unknown_chain_falls_back_to_testnet_budget() {
        // Dev / anvil chains get the shorter budget — mirrors `default_rpc_for`.
        assert_eq!(default_receipt_timeout(31_337), Duration::from_mins(1));
    }

    #[test]
    fn default_approval_horizon_is_seven_days() {
        // Pinned: the whole point of #9 is trimming the 30-day hardcode to
        // 7 days. If someone widens it, the gate-tightening premise breaks.
        assert_eq!(DEFAULT_APPROVAL_HORIZON, Duration::from_hours(24 * 7));
    }
}
