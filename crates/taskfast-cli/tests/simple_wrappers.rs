//! Wiremock tests for the thin-wrapper verbs: payment / dispute / agent /
//! platform / wallet. All are read-heavy; the shared test shape is "mock
//! an endpoint, drive the subcommand, assert envelope shape + one error
//! case."

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::agent::{
    self as agent_cmd, Command as AgentCommand, GetArgs as AgentGetArgs, ListArgs as AgentListArgs,
    UpdateMeArgs,
};
use taskfast_cli::cmd::dispute::{self as dispute_cmd, Args as DisputeArgs};
use taskfast_cli::cmd::payment::{
    self as payment_cmd, Command as PaymentCommand, GetArgs as PaymentGetArgs,
    ListArgs as PaymentListArgs,
};
use taskfast_cli::cmd::platform::{self as platform_cmd, Command as PlatformCommand};
use taskfast_cli::cmd::wallet::{self as wallet_cmd, Command as WalletCommand};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const TASK: &str = "11111111-1111-1111-1111-111111111111";
const AGENT: &str = "44444444-4444-4444-4444-444444444444";

fn ctx_for(server: &MockServer) -> Ctx {
    Ctx {
        api_key: Some("k".into()),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path: std::path::PathBuf::from("/dev/null"),
        dry_run: false,
        quiet: true,
        ..Default::default()
    }
}

fn env_value(e: &Envelope) -> serde_json::Value {
    serde_json::to_value(e).unwrap()
}

// ───────────── payment ─────────────

#[tokio::test]
async fn payment_get_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK}/payment")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": TASK, "amount": "10.00", "status": "pending"
        })))
        .mount(&server)
        .await;
    let envelope = payment_cmd::run(
        &ctx_for(&server),
        PaymentCommand::Get(PaymentGetArgs {
            task_id: TASK.into(),
        }),
    )
    .await
    .expect("payment get ok");
    assert_eq!(env_value(&envelope)["ok"], true);
}

#[tokio::test]
async fn payment_list_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/payments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": {"next_cursor": null, "has_more": false, "total_count": 0},
            "summary": {
                "pending_disbursement": "0",
                "total_earned": "0",
                "total_fees_paid": "0"
            }
        })))
        .mount(&server)
        .await;
    let envelope = payment_cmd::run(
        &ctx_for(&server),
        PaymentCommand::List(PaymentListArgs {
            status: None,
            from: None,
            to: None,
            cursor: None,
            limit: 50,
        }),
    )
    .await
    .expect("payment list ok");
    assert_eq!(env_value(&envelope)["ok"], true);
}

// ───────────── dispute ─────────────

#[tokio::test]
async fn dispute_get_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK}/dispute")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": TASK,
            "status": "disputed"
        })))
        .mount(&server)
        .await;
    let envelope = dispute_cmd::run(
        &ctx_for(&server),
        DisputeArgs {
            task_id: TASK.into(),
        },
    )
    .await
    .expect("dispute ok");
    let v = env_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["dispute"]["status"], "disputed");
}

#[tokio::test]
async fn dispute_rejects_non_uuid() {
    let server = MockServer::start().await;
    let err = dispute_cmd::run(
        &ctx_for(&server),
        DisputeArgs {
            task_id: "not-a-uuid".into(),
        },
    )
    .await
    .expect_err("bad uuid");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

// ───────────── agent ─────────────

#[tokio::test]
async fn agent_list_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "agents": [{"id": AGENT, "name": "alice"}],
            "next_cursor": null
        })))
        .mount(&server)
        .await;
    let envelope = agent_cmd::run(
        &ctx_for(&server),
        AgentCommand::List(AgentListArgs {
            capability: Some("coding".into()),
            cursor: None,
            limit: None,
        }),
    )
    .await
    .expect("agent list ok");
    let v = env_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["agents"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn agent_get_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/agents/{AGENT}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": AGENT, "name": "alice"
        })))
        .mount(&server)
        .await;
    let envelope = agent_cmd::run(
        &ctx_for(&server),
        AgentCommand::Get(AgentGetArgs {
            agent_id: AGENT.into(),
        }),
    )
    .await
    .expect("agent get ok");
    assert_eq!(env_value(&envelope)["data"]["agent"]["name"], "alice");
}

#[tokio::test]
async fn agent_update_me_requires_at_least_one_field() {
    let server = MockServer::start().await;
    let err = agent_cmd::run(
        &ctx_for(&server),
        AgentCommand::UpdateMe(UpdateMeArgs {
            name: None,
            description: None,
            capabilities: vec![],
            rate: None,
            max_task_budget: None,
            daily_spend_limit: None,
        }),
    )
    .await
    .expect_err("empty update rejected");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn agent_update_me_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": AGENT, "name": "renamed"
        })))
        .mount(&server)
        .await;
    let envelope = agent_cmd::run(
        &ctx_for(&server),
        AgentCommand::UpdateMe(UpdateMeArgs {
            name: Some("renamed".into()),
            description: None,
            capabilities: vec![],
            rate: None,
            max_task_budget: None,
            daily_spend_limit: None,
        }),
    )
    .await
    .expect("update-me ok");
    assert_eq!(env_value(&envelope)["data"]["agent"]["name"], "renamed");
}

// ───────────── platform / wallet ─────────────

#[tokio::test]
async fn platform_config_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/platform/config"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "review_window_hours": 24
        })))
        .mount(&server)
        .await;
    let envelope = platform_cmd::run(&ctx_for(&server), PlatformCommand::Config)
        .await
        .expect("platform config ok");
    assert_eq!(env_value(&envelope)["ok"], true);
}

#[tokio::test]
async fn wallet_balance_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/wallet/balance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "native": "0.1", "tokens": []
        })))
        .mount(&server)
        .await;
    let envelope = wallet_cmd::run(&ctx_for(&server), WalletCommand::Balance)
        .await
        .expect("wallet balance ok");
    assert_eq!(env_value(&envelope)["ok"], true);
}

#[tokio::test]
async fn wallet_balance_401_maps_to_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/wallet/balance"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "nope"})))
        .mount(&server)
        .await;
    let err = wallet_cmd::run(&ctx_for(&server), WalletCommand::Balance)
        .await
        .expect_err("401");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}
