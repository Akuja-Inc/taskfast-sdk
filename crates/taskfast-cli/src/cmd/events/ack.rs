//! `taskfast events ack <id>` — advance server-side event cursor.
//!
//! Written as a raw reqwest POST rather than a progenitor-generated call
//! because the ack endpoint lives outside the OpenAPI surface (the live
//! event stream is specified via AsyncAPI — see [`super::schema`]). The
//! request is still routed through the shared [`taskfast_client::TaskFastClient`]'s
//! inner reqwest instance so the `X-API-Key` header, timeouts, and
//! non-2xx classification stay identical to every other call.

use clap::Parser;

use super::super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::Error as ClientError;

#[derive(Debug, Parser)]
pub struct AckArgs {
    /// Event id from the live stream (UUID).
    pub event_id: String,
}

pub async fn run(ctx: &Ctx, args: AckArgs) -> CmdResult {
    let client = ctx.client()?;
    let url = format!(
        "{}/agents/me/events/{}/ack",
        client.inner().baseurl(),
        args.event_id
    );
    let resp = client
        .inner()
        .client()
        .post(&url)
        .send()
        .await
        .map_err(|e| CmdError::from(ClientError::from(e)))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| CmdError::Network(e.to_string()))?;

    if !status.is_success() {
        return Err(classify_ack_error(status.as_u16(), &body));
    }

    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| CmdError::Decode(format!("ack response not JSON: {e}")))?;

    Ok(Envelope::success(ctx.environment, ctx.dry_run, value))
}

fn classify_ack_error(code: u16, body: &str) -> CmdError {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let err_code = parsed
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let msg = parsed
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or(body)
        .to_string();

    match code {
        401 | 403 => CmdError::Auth(format!("HTTP {code}: {msg}")),
        422 => CmdError::Validation {
            code: or_default(&err_code, "validation_error"),
            message: msg,
        },
        404 => CmdError::Validation {
            code: or_default(&err_code, "not_found"),
            message: msg,
        },
        400..=499 => CmdError::Validation {
            code: or_default(&err_code, &format!("http_{code}")),
            message: msg,
        },
        500..=599 => CmdError::Server(format!("HTTP {code}: {msg}")),
        _ => CmdError::Server(format!("unexpected status {code}: {msg}")),
    }
}

fn or_default(s: &str, fallback: &str) -> String {
    if s.is_empty() {
        fallback.to_string()
    } else {
        s.to_string()
    }
}
