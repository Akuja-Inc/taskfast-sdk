//! `taskfast events stream` — long-running WebSocket tail on `/socket/agent`.
//!
//! # Contract
//!
//! Stdout is JSONL. Each line is either an event payload (matching the
//! AsyncAPI `components.messages` shape) or a sentinel frame:
//!
//! * `{"sentinel":"connected"}`    — initial join succeeded.
//! * `{"sentinel":"reconnecting"}` — transport dropped; backoff started.
//! * `{"sentinel":"resumed"}`      — reconnect succeeded; backlog drains.
//! * `{"sentinel":"closing"}`      — SIGINT/SIGTERM received; exiting 0.
//! * `{"sentinel":"auth_failed"}`  — server rejected the API key; exits 2.
//!
//! # Auth
//!
//! The API key is sent via the `Sec-WebSocket-Protocol` handshake header
//! (`taskfast.v1, bearer.<key>`). Keeping credentials off the URL line
//! keeps them out of proxy access logs and browser history-like caches.
//!
//! # Ack mechanic
//!
//! This subcommand **does not** ack. Acks travel via a separate process:
//! ```text
//!   taskfast events ack <event_id>
//! ```
//! which POSTs `/api/agents/me/events/:id/ack`. The server's channel
//! subscribes to a per-agent ack PubSub topic, so the HTTP ack pumps the
//! next event on the live stream without IPC between the two CLI
//! processes.
//!
//! # Protocol
//!
//! Phoenix Channels v2 JSON envelope: `[join_ref, msg_ref, topic, event, payload]`.
//! The server pushes marketplace events as `event == "event"`. Heartbeat
//! every 30s on topic `"phoenix"`. Reconnect uses exponential backoff
//! (1s → 2s → 4s → … → 30s cap); same backlog replays from cursor on
//! resume (server-side state).

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::time::{sleep, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::tungstenite::ClientRequestBuilder;

use super::super::{CmdError, Ctx};

#[derive(Debug, Parser)]
pub struct StreamArgs {
    /// Exit after the first backlog drain (join + any pushes delivered
    /// before an idle moment). Intended for integration tests; production
    /// callers leave this off and the stream runs until SIGTERM/SIGINT.
    #[arg(long)]
    pub once: bool,

    /// Exit on first transport drop instead of reconnecting. Reverts to the
    /// pre-auto-reconnect behaviour for callers that own the respawn loop
    /// themselves (e.g. systemd unit with `Restart=on-failure`).
    #[arg(long)]
    pub no_reconnect: bool,
}

/// Topic the CLI joins on the socket.
const TOPIC: &str = "agent:me";
/// Phoenix heartbeat cadence. Matches the server default.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Reconnect backoff ceiling.
const RECONNECT_CAP: Duration = Duration::from_secs(30);

/// Entry point. Returns a process exit code because the stream sidesteps
/// the envelope wrapper: error envelopes would contaminate the JSONL
/// stream and swallow stdout buffering semantics.
pub async fn run(ctx: &Ctx, args: StreamArgs) -> ExitCode {
    let api_key = match ctx.api_key.as_deref() {
        Some(k) => k.to_string(),
        None => {
            emit_error_envelope(ctx, &CmdError::MissingApiKey);
            return CmdError::MissingApiKey.exit_code().into();
        }
    };
    let ws_url = match ws_url_from_base(ctx.base_url()) {
        Ok(u) => u,
        Err(e) => {
            emit_error_envelope(ctx, &e);
            return e.exit_code().into();
        }
    };

    let mut backoff = Duration::from_secs(1);
    let mut first_connect = true;

    let shutdown = install_shutdown_signal();

    loop {
        if shutdown.is_cancelled() {
            emit_sentinel("closing");
            return ExitCode::SUCCESS;
        }

        match run_once(&ws_url, &api_key, first_connect, args.once, &shutdown).await {
            ConnectOutcome::AuthFailed => {
                emit_sentinel("auth_failed");
                return ExitCode::from(2);
            }
            ConnectOutcome::Shutdown => {
                emit_sentinel("closing");
                return ExitCode::SUCCESS;
            }
            ConnectOutcome::OnceDone => {
                return ExitCode::SUCCESS;
            }
            ConnectOutcome::Disconnected => {
                // Fall through to reconnect loop below.
            }
        }

        if args.no_reconnect {
            // Surface the drop as a non-zero exit so supervisors (systemd,
            // k8s) see it as a failure and respawn under their own policy.
            return ExitCode::from(1);
        }

        emit_sentinel("reconnecting");
        let wait = backoff;
        tokio::select! {
            () = sleep(wait) => {},
            () = shutdown.cancelled() => {
                emit_sentinel("closing");
                return ExitCode::SUCCESS;
            }
        }
        backoff = (backoff * 2).min(RECONNECT_CAP);
        first_connect = false;
    }
}

enum ConnectOutcome {
    AuthFailed,
    Shutdown,
    OnceDone,
    Disconnected,
}

async fn run_once(
    ws_url: &str,
    api_key: &str,
    first_connect: bool,
    once: bool,
    shutdown: &ShutdownToken,
) -> ConnectOutcome {
    let req = match build_request(ws_url, api_key) {
        Ok(r) => r,
        Err(_) => return ConnectOutcome::Disconnected,
    };

    let (ws, resp) = match tokio_tungstenite::connect_async(req).await {
        Ok(v) => v,
        Err(e) => {
            if is_auth_error(&e) {
                return ConnectOutcome::AuthFailed;
            }
            return ConnectOutcome::Disconnected;
        }
    };

    // Server must select `taskfast.v1`. If it didn't, either the socket
    // rejected us or a middlebox rewrote the handshake — treat as auth.
    if !has_taskfast_subprotocol(&resp) {
        return ConnectOutcome::AuthFailed;
    }

    let (mut sink, mut source) = ws.split();

    // Phoenix join
    let join_msg = phx_frame("1", "1", TOPIC, "phx_join", json!({}));
    if sink.send(Message::Text(join_msg.into())).await.is_err() {
        return ConnectOutcome::Disconnected;
    }

    // Await join reply before announcing connected.
    let mut join_acked = false;
    let mut msg_ref: u64 = 1;
    let mut heartbeat_next = Instant::now() + HEARTBEAT_INTERVAL;

    loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                let _ = sink.send(Message::Close(None)).await;
                return ConnectOutcome::Shutdown;
            }
            () = sleep_until(heartbeat_next) => {
                msg_ref += 1;
                let hb = phx_frame_null_join(&msg_ref.to_string(), "phoenix", "heartbeat", json!({}));
                if sink.send(Message::Text(hb.into())).await.is_err() {
                    return ConnectOutcome::Disconnected;
                }
                heartbeat_next = Instant::now() + HEARTBEAT_INTERVAL;
            }
            frame = source.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let parsed: Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(
                                    target: "taskfast::events",
                                    error = %e,
                                    "WS text frame was not valid JSON; surfacing as unparseable"
                                );
                                emit_unparseable(&text, &e.to_string());
                                continue;
                            }
                        };
                        match classify_frame(&parsed) {
                            FrameKind::JoinOk => {
                                if !join_acked {
                                    join_acked = true;
                                    let sentinel = if first_connect { "connected" } else { "resumed" };
                                    emit_sentinel(sentinel);
                                }
                            }
                            FrameKind::JoinError => {
                                return ConnectOutcome::AuthFailed;
                            }
                            FrameKind::Event(payload) => {
                                emit_event(&payload);
                                if once {
                                    let _ = sink.send(Message::Close(None)).await;
                                    return ConnectOutcome::OnceDone;
                                }
                            }
                            FrameKind::Ignored => {}
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = sink.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        return ConnectOutcome::Disconnected;
                    }
                    Some(Err(_)) => {
                        return ConnectOutcome::Disconnected;
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline <= now {
        return;
    }
    sleep(deadline - now).await;
}

enum FrameKind {
    JoinOk,
    JoinError,
    Event(Value),
    Ignored,
}

fn classify_frame(frame: &Value) -> FrameKind {
    let arr = match frame.as_array() {
        Some(a) if a.len() == 5 => a,
        _ => return FrameKind::Ignored,
    };
    let topic = arr[2].as_str().unwrap_or("");
    let event = arr[3].as_str().unwrap_or("");
    let payload = &arr[4];

    if event == "phx_reply" && topic == TOPIC {
        let status = payload.get("status").and_then(|v| v.as_str()).unwrap_or("");
        return match status {
            "ok" => FrameKind::JoinOk,
            "error" => FrameKind::JoinError,
            _ => FrameKind::Ignored,
        };
    }
    if event == "event" && topic == TOPIC {
        return FrameKind::Event(payload.clone());
    }
    FrameKind::Ignored
}

fn phx_frame(join_ref: &str, msg_ref: &str, topic: &str, event: &str, payload: Value) -> String {
    serde_json::to_string(&json!([join_ref, msg_ref, topic, event, payload]))
        .expect("phoenix frame must serialize")
}

fn phx_frame_null_join(msg_ref: &str, topic: &str, event: &str, payload: Value) -> String {
    serde_json::to_string(&json!([Value::Null, msg_ref, topic, event, payload]))
        .expect("phoenix frame must serialize")
}

fn build_request(
    ws_url: &str,
    api_key: &str,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, CmdError> {
    let uri: Uri =
        ws_url
            .parse()
            .map_err(|e: tokio_tungstenite::tungstenite::http::uri::InvalidUri| {
                CmdError::Usage(format!("invalid ws url {ws_url:?}: {e}"))
            })?;
    let builder = ClientRequestBuilder::new(uri)
        .with_sub_protocol("taskfast.v1")
        .with_sub_protocol(format!("bearer.{api_key}"));
    builder
        .into_client_request()
        .map_err(|e| CmdError::Usage(format!("ws request build failed: {e}")))
}

fn has_taskfast_subprotocol(
    resp: &tokio_tungstenite::tungstenite::handshake::client::Response,
) -> bool {
    resp.headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.split(',').any(|tok| tok.trim() == "taskfast.v1"))
}

fn is_auth_error(e: &tokio_tungstenite::tungstenite::Error) -> bool {
    use tokio_tungstenite::tungstenite::Error as TE;
    match e {
        TE::Http(resp) => matches!(resp.status().as_u16(), 401 | 403),
        _ => false,
    }
}

fn ws_url_from_base(base: &str) -> Result<String, CmdError> {
    let trimmed = base.trim_end_matches('/');
    let swapped = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("wss://") || trimmed.starts_with("ws://") {
        trimmed.to_string()
    } else {
        return Err(CmdError::Usage(format!(
            "base url must start with http(s):// or ws(s)://, got {base:?}"
        )));
    };
    Ok(format!("{swapped}/socket/agent/websocket?vsn=2.0.0"))
}

fn emit_sentinel(kind: &str) {
    let line = format!("{}\n", json!({ "sentinel": kind }));
    let stdout = std::io::stdout();
    let mut guard = stdout.lock();
    let _ = guard.write_all(line.as_bytes());
    let _ = guard.flush();
}

fn emit_event(payload: &Value) {
    let mut buf = match serde_json::to_vec(payload) {
        Ok(b) => b,
        Err(_) => return,
    };
    buf.push(b'\n');
    let stdout = std::io::stdout();
    let mut guard = stdout.lock();
    let _ = guard.write_all(&buf);
    let _ = guard.flush();
}

/// Surface a malformed WS text frame on the `--json` path instead of
/// swallowing it. The `type: "unparseable"` tag keeps consumers' stream
/// parsers from mistaking it for a regular event.
fn emit_unparseable(raw: &str, error: &str) {
    let payload = json!({ "type": "unparseable", "raw": raw, "error": error });
    emit_event(&payload);
}

fn emit_error_envelope(ctx: &Ctx, err: &CmdError) {
    if ctx.quiet {
        return;
    }
    crate::envelope::Envelope::error(ctx.environment, ctx.dry_run, err).emit();
}

#[derive(Clone)]
struct ShutdownToken {
    inner: Arc<tokio::sync::Notify>,
    flag: Arc<std::sync::atomic::AtomicBool>,
}

impl ShutdownToken {
    fn new() -> Self {
        Self {
            inner: Arc::new(tokio::sync::Notify::new()),
            flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn trigger(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.inner.notified().await;
    }
}

fn install_shutdown_signal() -> ShutdownToken {
    let token = ShutdownToken::new();
    let tclone = token.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => {
                    let _ = tokio::signal::ctrl_c().await;
                    tclone.trigger();
                    return;
                }
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        tclone.trigger();
    });
    token
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_swaps_http_to_ws() {
        assert_eq!(
            ws_url_from_base("http://localhost:4000").unwrap(),
            "ws://localhost:4000/socket/agent/websocket?vsn=2.0.0"
        );
    }

    #[test]
    fn ws_url_swaps_https_to_wss() {
        assert_eq!(
            ws_url_from_base("https://api.taskfast.app/").unwrap(),
            "wss://api.taskfast.app/socket/agent/websocket?vsn=2.0.0"
        );
    }

    #[test]
    fn ws_url_passthrough_ws_scheme() {
        assert_eq!(
            ws_url_from_base("ws://localhost:4000").unwrap(),
            "ws://localhost:4000/socket/agent/websocket?vsn=2.0.0"
        );
    }

    #[test]
    fn ws_url_rejects_unknown_scheme() {
        assert!(ws_url_from_base("ftp://x").is_err());
    }

    #[test]
    fn classify_recognizes_join_ok() {
        let f = json!(["1", "1", "agent:me", "phx_reply", { "status": "ok", "response": {} }]);
        assert!(matches!(classify_frame(&f), FrameKind::JoinOk));
    }

    #[test]
    fn classify_recognizes_join_error() {
        let f = json!(["1", "1", "agent:me", "phx_reply", { "status": "error" }]);
        assert!(matches!(classify_frame(&f), FrameKind::JoinError));
    }

    #[test]
    fn classify_recognizes_event_push() {
        let f =
            json!(["1", null, "agent:me", "event", { "event_id": "x", "event": "task_assigned" }]);
        match classify_frame(&f) {
            FrameKind::Event(p) => assert_eq!(p["event"], "task_assigned"),
            _ => panic!("expected Event"),
        }
    }

    #[test]
    fn classify_ignores_other_topics() {
        let f = json!([null, "2", "phoenix", "phx_reply", { "status": "ok" }]);
        assert!(matches!(classify_frame(&f), FrameKind::Ignored));
    }
}
