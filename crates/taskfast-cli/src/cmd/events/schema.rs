//! `taskfast events schema` — fetch the AsyncAPI 2.6 spec for the event stream.
//!
//! The spec is generated at server boot from the canonical
//! `WebhookDelivery.build_payload/2` registry (see
//! `lib/task_fast_web/async_api.ex`) so there is exactly one source of
//! truth for the 13 event types. `--event <name>` narrows the output to
//! one message schema; default prints the full document.

use clap::Parser;
use serde_json::json;

use super::super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::Error as ClientError;

#[derive(Debug, Parser)]
pub struct SchemaArgs {
    /// Restrict output to a single event type's message schema.
    /// Name matches the AsyncAPI `components.messages.<Camel>` key
    /// (e.g. `TaskAssigned`, `BidAccepted`).
    #[arg(long)]
    pub event: Option<String>,
}

pub async fn run(ctx: &Ctx, args: SchemaArgs) -> CmdResult {
    // Schema is public (no X-API-Key required on the server), but we reuse
    // the inner reqwest client so timeouts + TLS config match every other call.
    // Falls back to a bare reqwest::Client when no api_key is configured.
    let base = format!("{}/api/asyncapi.json", ctx.base_url().trim_end_matches('/'));
    let http = match ctx.client() {
        Ok(c) => c.inner().client().clone(),
        Err(_) => reqwest::Client::new(),
    };

    let resp = http
        .get(&base)
        .send()
        .await
        .map_err(|e| CmdError::from(ClientError::from(e)))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| CmdError::Network(e.to_string()))?;

    if !status.is_success() {
        return Err(CmdError::Server(format!(
            "GET /api/asyncapi.json → HTTP {}: {}",
            status.as_u16(),
            body
        )));
    }

    let spec: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| CmdError::Decode(format!("asyncapi response not JSON: {e}")))?;

    let data = match args.event {
        None => spec,
        Some(ref key) => spec
            .get("components")
            .and_then(|c| c.get("messages"))
            .and_then(|m| m.get(key))
            .cloned()
            .ok_or_else(|| CmdError::Validation {
                code: "unknown_event".into(),
                message: format!(
                    "no message named {key:?} in components.messages — run without --event to list all"
                ),
            })
            .map(|msg| json!({ "event": key, "message": msg }))?,
    };

    Ok(Envelope::success(ctx.environment, ctx.dry_run, data))
}
