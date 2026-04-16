//! `taskfast events` — lifecycle event read/stream/ack + AsyncAPI schema.
//!
//! Subcommands:
//!
//! * [`poll`] — one-shot `GET /api/agents/me/events` (cursor-paginated
//!   read). Envelope-wrapped like every other subcommand.
//! * [`stream`] — long-running WebSocket to `/socket/agent` that emits
//!   one JSONL line per event on stdout. Bypasses the envelope — stdout
//!   is the contract. Ack via `taskfast events ack <id>` from a
//!   separate process.
//! * [`ack`] — `POST /api/agents/me/events/:id/ack`. Advances the
//!   server-side cursor and releases the in-flight slot so the next
//!   event can be pushed on the live stream.
//! * [`schema`] — `GET /api/asyncapi.json`. Prints the full AsyncAPI
//!   2.6 document or filters to a single event type via `--event`.

use clap::Subcommand;

use super::{CmdError, CmdResult, Ctx};

pub mod ack;
pub mod poll;
pub mod schema;
pub mod stream;

pub use ack::AckArgs;
pub use poll::PollArgs;
pub use schema::SchemaArgs;
pub use stream::StreamArgs;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// GET /agents/me/events — one page of lifecycle events.
    Poll(PollArgs),
    /// Stream lifecycle events over WebSocket as JSONL on stdout.
    ///
    /// Bypasses the JSON envelope: every line is either an event
    /// payload or a sentinel (`{"sentinel":"connected"}`,
    /// `{"sentinel":"reconnecting"}`, `{"sentinel":"resumed"}`,
    /// `{"sentinel":"closing"}`). Ack each event from a separate
    /// process via `taskfast events ack <id>`.
    Stream(StreamArgs),
    /// POST /agents/me/events/:id/ack — advance cursor + release
    /// in-flight slot on the live stream.
    Ack(AckArgs),
    /// GET /asyncapi.json — AsyncAPI 2.6 schema for the event stream.
    Schema(SchemaArgs),
}

/// Dispatch non-stream subcommands. `Stream` is handled in `main.rs`
/// because it writes JSONL directly to stdout and must bypass the
/// envelope wrapper.
pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Poll(a) => poll::run(ctx, a).await,
        Command::Ack(a) => ack::run(ctx, a).await,
        Command::Schema(a) => schema::run(ctx, a).await,
        Command::Stream(_) => Err(CmdError::Usage(
            "`events stream` must be dispatched by main.rs (bypass envelope)".into(),
        )),
    }
}
