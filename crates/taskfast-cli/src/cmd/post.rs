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

use super::{validate_override_rpc_url, CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_agent::tempo_rpc::{sign_and_broadcast_erc20_transfer, TempoRpcClient};
use taskfast_chains::tempo::{is_allowed_fee_token, is_known_network};
use taskfast_client::api::types::{
    CompletionCriterionInput, TaskDraftPrepareRequest, TaskDraftPrepareRequestAssignmentType,
    TaskDraftPrepareRequestPosterWalletAddress, TaskDraftSubmitRequest,
    TaskDraftSubmitRequestSignature,
};
use taskfast_client::{map_api_error, TaskFastClient};

#[derive(Debug, Parser)]
pub struct Args {
    /// Task title (required).
    #[arg(long)]
    pub title: String,

    /// Task description (required, non-empty). Validated client-side so a
    /// typoed script fails fast instead of paying for a server 422 round-trip.
    #[arg(long)]
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

    /// Tempo RPC endpoint. Defaults to the canonical proxy URL the
    /// deployment advertises for the env's network.
    #[arg(long, env = "TEMPO_RPC_URL")]
    pub rpc_url: Option<String>,

    /// Acknowledge oversized budgets. Required when the budget exceeds
    /// `confirm_above_budget` in the config. No-op when the gate is unset
    /// or when the budget is below it. Fail-closed by design (no TTY
    /// prompt) so a CI script accident doesn't broadcast a huge approve.
    #[arg(long)]
    pub yes: bool,
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

pub async fn run(ctx: &Ctx, args: Args) -> CmdResult {
    if args.description.trim().is_empty() {
        return Err(CmdError::Usage(
            "--description must not be empty (server requires a non-blank description)".into(),
        ));
    }
    ctx.enforce_budget_gate(args.budget.as_deref(), args.yes, "post a task")?;
    let wallet_address = args
        .wallet_address
        .as_deref()
        .or(ctx.wallet_address.as_deref())
        .ok_or_else(|| {
            CmdError::Usage(
                "--wallet-address (or TEMPO_WALLET_ADDRESS, or wallet_address in config) required to post a task".into(),
            )
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

    // Dry-run must perform zero HTTP. Predict the proxy URL locally so the
    // envelope still reports what the real path would have used.
    if ctx.dry_run {
        let network = ctx.environment.network();
        let rpc_url = if let Some(ref override_url) = args.rpc_url {
            validate_override_rpc_url(override_url, network, ctx.allow_custom_endpoints)?;
            override_url.clone()
        } else {
            format!(
                "{}/rpc/{}",
                ctx.base_url().trim_end_matches('/'),
                network.as_str()
            )
        };
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

    // Resolve the RPC URL. Default path: pull it from the deployment's
    // `GET /config/network` (public). Override path: user supplied
    // `--rpc-url` / `TEMPO_RPC_URL` and must also pass
    // `--allow-custom-endpoints`.
    let (rpc_url, _via_proxy) = resolve_rpc_url(&client, &args, ctx).await?;

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
    let keystore_ref = args.keystore.as_deref().map(str::to_string).or_else(|| {
        ctx.keystore_path
            .as_deref()
            .and_then(|p| p.to_str().map(str::to_string))
    });
    let signer = super::wallet_args::load_signer(
        keystore_ref.as_deref(),
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

    // Pick the http client by URL, not by resolution branch: any URL
    // landing on `{api_base}/rpc/` is the authenticated proxy and
    // needs `X-API-Key` (set as a default header on `client.http_client()`).
    // A `--rpc-url`/`TEMPO_RPC_URL` override that points at the proxy
    // hit this same path; the prior `via_proxy` flag missed that case
    // and dropped the header, returning 401 "missing API key".
    let http = ctx.rpc_http_client(&client, &rpc_url);
    let rpc = TempoRpcClient::new(http, rpc_url.clone());

    // F1: consult the chain's own `eth_chainId` (not anything the server
    // claims) and refuse to sign a `transfer` against a token that isn't
    // on the PathUSD allowlist for that chain. A compromised TaskFast API
    // returning an attacker `token_address` is the fund-drain vector here.
    let chain_id = rpc
        .chain_id()
        .await
        .map_err(|e| CmdError::Server(format!("tempo rpc eth_chainId: {e}")))?;
    if !ctx.allow_custom_endpoints && !is_known_network(chain_id) {
        return Err(CmdError::Validation {
            code: "unknown_chain".into(),
            message: format!(
                "RPC reports chain_id={chain_id} which is not a known Tempo or \
                 local-dev network; refusing to sign. Pass --allow-custom-endpoints \
                 to override."
            ),
        });
    }
    if !is_allowed_fee_token(chain_id, &prep.token_address) {
        return Err(CmdError::Validation {
            code: "fee_token_not_allowed".into(),
            message: format!(
                "server returned token_address {} which is not on the PathUSD \
                 allowlist for chain_id={chain_id}. Refusing to sign — this would \
                 redirect your fee transfer to an arbitrary ERC-20 contract.",
                prep.token_address
            ),
        });
    }

    // Audit line — always emitted to stderr so a CI log captures the exact
    // recipient+token+chain we committed funds to. Not a prompt: CLI stays
    // non-interactive.
    eprintln!(
        "taskfast: signing submission-fee transfer — token={} chain_id={} rpc={}",
        prep.token_address, chain_id, rpc_url
    );

    // F9: serialize nonce-consuming sign+broadcast per wallet. `_guard`
    // holds the exclusive file lock from here until function return,
    // which scope-covers both eth_getTransactionCount (inside
    // sign_and_broadcast_erc20_transfer) and eth_sendRawTransaction.
    // Locked on the keystore path (stripping the optional `file:` prefix
    // that `taskfast init` writes) so two processes sharing a keystore
    // can't race, while distinct wallets proceed in parallel.
    let _guard = if let Some(ref raw) = keystore_ref {
        let path_str = raw.strip_prefix("file:").unwrap_or(raw);
        Some(crate::wallet_lock::acquire(std::path::Path::new(path_str))?)
    } else {
        None
    };

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

/// Resolve the RPC endpoint for a post invocation.
///
/// Returns `(url, via_proxy)`:
///   * `(override, false)` — user supplied `--rpc-url` / `TEMPO_RPC_URL`,
///     passed the custom-endpoint guard, and will hit a bare upstream RPC.
///   * `(proxy_url, true)` — default path: the deployment's
///     `/config/network` entry for the selected network points the CLI
///     at `{api_base}/rpc/{network}`, which the server proxies to its
///     own Tempo upstream (Alchemy by default). The caller should carry
///     the authenticated reqwest::Client through to `TempoRpcClient`.
async fn resolve_rpc_url(
    client: &TaskFastClient,
    args: &Args,
    ctx: &Ctx,
) -> Result<(String, bool), CmdError> {
    let network = ctx.environment.network();
    if let Some(ref override_url) = args.rpc_url {
        validate_override_rpc_url(override_url, network, ctx.allow_custom_endpoints)?;
        return Ok((override_url.clone(), false));
    }
    let cfg = client.fetch_network_config().await.map_err(|e| match e {
        taskfast_client::Error::Auth(_) | taskfast_client::Error::Validation { .. } => {
            CmdError::Server(format!("fetch network config from {}: {e}", ctx.base_url()))
        }
        other => CmdError::from(other),
    })?;
    // Runtime invariant — issue #62 (server-side): the env's deployment must
    // advertise exactly its one network, and nothing else.
    //
    // Today's deployments still advertise multiple networks per response;
    // until #62 lands, a `len != 1` mismatch logs a warn and continues.
    // Set TASKFAST_STRICT_ENV_NETWORK=1 to fail-closed.
    // `--allow-custom-endpoints` and `Environment::Local` bypass entirely
    // (matches `enforce_endpoint_guard`).
    let name = network.as_str();
    if !ctx.allow_custom_endpoints
        && ctx.environment != crate::Environment::Local
        && cfg.networks.len() != 1
    {
        let advertised: Vec<&str> = cfg.networks.keys().map(String::as_str).collect();
        let strict = std::env::var("TASKFAST_STRICT_ENV_NETWORK")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        if strict {
            return Err(CmdError::Server(format!(
                "deployment at {} advertises networks {advertised:?}; env {} requires \
                 exactly [{name}]. Server-side fix tracked in issue #62. Unset \
                 TASKFAST_STRICT_ENV_NETWORK or pass --allow-custom-endpoints to bypass.",
                ctx.base_url(),
                ctx.environment.as_str(),
            )));
        }
        tracing::warn!(
            api_base = %ctx.base_url(),
            env = ctx.environment.as_str(),
            expected = name,
            advertised = ?advertised,
            "deployment advertises additional networks; server-side fix tracked in #62. \
             Set TASKFAST_STRICT_ENV_NETWORK=1 to fail-closed."
        );
    }
    let entry = cfg.entry(name).map_err(|e| {
        CmdError::Server(format!(
            "deployment at {} does not advertise network `{name}`: {e}",
            ctx.base_url()
        ))
    })?;
    // F2-equivalent guard, minus the static allowlist: the old
    // WELL_KNOWN_TEMPO_RPCS check trusted exactly two hardcoded URLs. The
    // replacement says "the proxy must live under the same api_base the
    // endpoint-guard already approved". Catches (a) a misconfigured
    // deployment returning an upstream URL instead of its own proxy, and
    // (b) a compromised backend trying to steer RPC traffic off-host —
    // minus a full MITM on api_base itself (which has its own F5/F6
    // defenses).
    let expected_prefix = format!("{}/rpc/", ctx.base_url().trim_end_matches('/'));
    if !entry.rpc_url.starts_with(&expected_prefix) {
        return Err(CmdError::Server(format!(
            "deployment at {} returned rpc_url {:?} for network `{name}`, \
             which does not live under `{expected_prefix}…`. Refusing to \
             route RPC traffic off-host.",
            ctx.base_url(),
            entry.rpc_url,
        )));
    }
    Ok((entry.rpc_url.clone(), true))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Network;

    #[test]
    fn validate_override_rejects_without_opt_in() {
        let err = validate_override_rpc_url("https://mallory.example", Network::Mainnet, false)
            .expect_err("override without opt-in must be refused");
        match err {
            CmdError::Usage(msg) => {
                assert!(msg.contains("--allow-custom-endpoints"), "msg: {msg}");
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn validate_override_accepts_with_opt_in() {
        validate_override_rpc_url("https://my-node.example", Network::Testnet, true)
            .expect("opt-in bypasses the guard");
    }

    #[test]
    fn validate_override_rejects_plain_http_on_mainnet() {
        let err = validate_override_rpc_url("http://my-node.example", Network::Mainnet, true)
            .expect_err("plain-http on mainnet is always refused (except loopback)");
        assert!(matches!(err, CmdError::Usage(msg) if msg.contains("plain-HTTP")));
    }

    #[test]
    fn validate_override_allows_plain_http_loopback_on_mainnet() {
        validate_override_rpc_url("http://127.0.0.1:8545", Network::Mainnet, true)
            .expect("loopback bypasses mainnet-HTTPS check");
        validate_override_rpc_url("http://localhost:8545", Network::Mainnet, true)
            .expect("loopback bypasses mainnet-HTTPS check");
    }

    #[test]
    fn validate_override_allows_plain_http_on_testnet() {
        validate_override_rpc_url("http://my-testnet.example", Network::Testnet, true)
            .expect("testnet does not enforce HTTPS");
    }

    #[test]
    fn network_name_matches_config_map_keys() {
        // The lookup key into `GET /config/network`'s `networks` map.
        // Must stay in sync with what the deployment advertises.
        assert_eq!(Network::Mainnet.as_str(), "mainnet");
        assert_eq!(Network::Testnet.as_str(), "testnet");
    }
}
