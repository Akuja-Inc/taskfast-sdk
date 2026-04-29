//! Throwaway: probe why `TaskEscrow.openWithMemo` reverts on moderato testnet
//! for the pending-escrow bid, by narrowing down the inputs via `eth_call`.
//!
//! Run: `BID=<uuid> cargo run --example escrow_probe -p taskfast-cli`
//! Needs env vars exported: TASKFAST_API, TASKFAST_API_KEY,
//! TEMPO_KEY_SOURCE, TASKFAST_WALLET_PASSWORD_FILE.

use std::env;

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use reqwest::Client;
use serde_json::{json, Value};
use taskfast_agent::chain::{TaskEscrow, IERC20};
use taskfast_client::TaskFastClient;

async fn rpc(client: &Client, rpc_url: &str, method: &str, params: Value) -> Value {
    let resp = client
        .post(rpc_url)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .await
        .expect("rpc send");
    let body: Value = resp.json().await.expect("rpc parse");
    println!("---- {method}");
    println!("{}", serde_json::to_string_pretty(&body).unwrap());
    body
}

async fn eth_call_probe(
    client: &Client,
    rpc_url: &str,
    from: Address,
    to: Address,
    data: &Bytes,
    label: &str,
) {
    println!("\n==== {label} ====");
    rpc(
        client,
        rpc_url,
        "eth_call",
        json!([{
            "from": format!("{from:#x}"),
            "to": format!("{to:#x}"),
            "data": format!("0x{}", hex::encode(data)),
        }, "latest"]),
    )
    .await;
}

#[tokio::main]
async fn main() {
    let api = env::var("TASKFAST_API").unwrap_or_else(|_| "http://localhost:4000".into());
    let api_key = env::var("TASKFAST_API_KEY").expect("TASKFAST_API_KEY");
    let bid_id = env::var("BID").expect("BID=<uuid>");

    // Pull the testnet RPC URL from the deployment's /config/network.
    // Reuse the client's pre-authenticated reqwest::Client for RPC calls —
    // the proxy at {api}/rpc/testnet requires X-API-Key.
    let tf = TaskFastClient::from_api_key(&api, &api_key).expect("construct client");
    let cfg = tf
        .fetch_network_config()
        .await
        .expect("fetch /config/network");
    let rpc_url = cfg
        .entry("testnet")
        .expect("deployment advertises testnet")
        .rpc_url
        .clone();
    let http = tf.http_client();

    // 1. Fetch escrow params from the API (CLI-equivalent, so we see exact numbers)
    let params: Value = http
        .get(format!("{api}/bids/{bid_id}/escrow/params"))
        .header("X-API-Key", &api_key)
        .send()
        .await
        .expect("api send")
        .json()
        .await
        .expect("api parse");
    println!("==== escrow_params");
    println!("{}", serde_json::to_string_pretty(&params).unwrap());

    let p = if params.get("data").is_some() {
        &params["data"]
    } else {
        &params
    };
    let token: Address = p["token_address"].as_str().unwrap().parse().unwrap();
    let task_escrow: Address = p["task_escrow_contract"].as_str().unwrap().parse().unwrap();
    let worker: Address = p["worker_address"].as_str().unwrap().parse().unwrap();
    let platform: Address = p["platform_wallet"].as_str().unwrap().parse().unwrap();
    let decimals = p["decimals"].as_i64().unwrap() as u8;
    let deposit = decimal_to_u256(p["amount"].as_str().unwrap(), decimals);
    let fee = decimal_to_u256(p["platform_fee_amount"].as_str().unwrap(), decimals);
    let memo_hash: Option<B256> = p["memo_hash"].as_str().map(|s| s.parse().unwrap());

    let poster: Address = env::var("TEMPO_WALLET_ADDRESS")
        .expect("TEMPO_WALLET_ADDRESS")
        .parse()
        .unwrap();

    println!("\ncomputed: token={token:#x} task_escrow={task_escrow:#x} worker={worker:#x} platform={platform:#x}");
    println!("          poster={poster:#x} deposit={deposit} fee={fee} memo_hash={memo_hash:?}");

    // 2. eth_getCode — confirm task_escrow contract deployed
    rpc(
        &http,
        &rpc_url,
        "eth_getCode",
        json!([format!("{task_escrow:#x}"), "latest"]),
    )
    .await;

    // 3. eth_getCode on token
    rpc(
        &http,
        &rpc_url,
        "eth_getCode",
        json!([format!("{token:#x}"), "latest"]),
    )
    .await;

    // 4. balance + allowance (sanity confirm)
    let balance_data: Bytes = IERC20::balanceOfCall { account: poster }
        .abi_encode()
        .into();
    eth_call_probe(
        &http,
        &rpc_url,
        poster,
        token,
        &balance_data,
        "token.balanceOf(poster)",
    )
    .await;

    let allow_data: Bytes = IERC20::allowanceCall {
        owner: poster,
        spender: task_escrow,
    }
    .abi_encode()
    .into();
    eth_call_probe(
        &http,
        &rpc_url,
        poster,
        token,
        &allow_data,
        "token.allowance(poster, task_escrow)",
    )
    .await;

    // 5. Attempt: openWithMemo with live salt
    let salt = B256::from(rand::random::<[u8; 32]>());
    let memo = memo_hash.unwrap_or_default();
    let open_memo_data: Bytes = TaskEscrow::openWithMemoCall {
        token,
        deposit,
        worker,
        platformFeeAmount: fee,
        platform,
        salt,
        memoHash: memo,
    }
    .abi_encode()
    .into();
    eth_call_probe(
        &http,
        &rpc_url,
        poster,
        task_escrow,
        &open_memo_data,
        "openWithMemo (live params)",
    )
    .await;

    // 6. Attempt: openCall (no memo)
    let open_data: Bytes = TaskEscrow::openCall {
        token,
        deposit,
        worker,
        platformFeeAmount: fee,
        platform,
        salt,
    }
    .abi_encode()
    .into();
    eth_call_probe(
        &http,
        &rpc_url,
        poster,
        task_escrow,
        &open_data,
        "open (no memo)",
    )
    .await;

    // 7. Attempt: openWithMemo with zero fee
    let open_zero_fee: Bytes = TaskEscrow::openWithMemoCall {
        token,
        deposit,
        worker,
        platformFeeAmount: U256::ZERO,
        platform,
        salt,
        memoHash: memo,
    }
    .abi_encode()
    .into();
    eth_call_probe(
        &http,
        &rpc_url,
        poster,
        task_escrow,
        &open_zero_fee,
        "openWithMemo (fee=0)",
    )
    .await;

    // 8. Attempt: openWithMemo with min deposit (1 wei-unit) — tests whether the
    //    revert depends on amount.
    let open_min: Bytes = TaskEscrow::openWithMemoCall {
        token,
        deposit: U256::from(1u64),
        worker,
        platformFeeAmount: U256::ZERO,
        platform,
        salt,
        memoHash: memo,
    }
    .abi_encode()
    .into();
    eth_call_probe(
        &http,
        &rpc_url,
        poster,
        task_escrow,
        &open_min,
        "openWithMemo (deposit=1, fee=0)",
    )
    .await;

    // 9. debug_traceCall (might not be exposed by public RPC)
    println!("\n==== debug_traceCall openWithMemo (may be unsupported) ====");
    rpc(
        &http,
        &rpc_url,
        "debug_traceCall",
        json!([{
            "from": format!("{poster:#x}"),
            "to": format!("{task_escrow:#x}"),
            "data": format!("0x{}", hex::encode(&open_memo_data)),
        }, "latest", {"tracer": "callTracer"}]),
    )
    .await;
}

fn decimal_to_u256(s: &str, decimals: u8) -> U256 {
    let (w, f) = s.split_once('.').unwrap_or((s, ""));
    let max = decimals as usize;
    assert!(
        f.len() <= max,
        "decimal_to_u256: {s} has {} fractional digits, exceeds token decimals {max}",
        f.len()
    );
    let mut combined = String::new();
    combined.push_str(w);
    combined.push_str(f);
    for _ in 0..max.saturating_sub(f.len()) {
        combined.push('0');
    }
    let stripped = combined.trim_start_matches('0');
    let digits = if stripped.is_empty() { "0" } else { stripped };
    U256::from_str_radix(digits, 10).unwrap()
}
