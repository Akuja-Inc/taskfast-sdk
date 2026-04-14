//! `taskfast-cli` library surface.
//!
//! The crate ships primarily as the `taskfast` binary (see `src/main.rs`),
//! but every subcommand's `run` function and the shared envelope/exit/error
//! types are re-exported here so integration tests (and, later, embedded
//! callers) can drive the pipeline without spawning a process.

// TODO: tighten doc coverage on public items + remove this allow.
// Tracked under the rust-best-practices follow-up.
#![allow(missing_docs)]

pub mod cmd;
pub mod dotenv;
pub mod envelope;
pub mod exit;

pub use envelope::{Envelope, ErrorPayload};
pub use exit::ExitCode;

/// Re-exported from `main.rs` so tests can construct a [`cmd::Ctx`] with a
/// named [`Environment`] without depending on the binary entry point.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Environment {
    Prod,
    Staging,
    Local,
}

impl Environment {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prod => "production",
            Self::Staging => "staging",
            Self::Local => "local",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Prod => "https://api.taskfast.app",
            Self::Staging => "https://staging.api.taskfast.app",
            Self::Local => "http://localhost:4000",
        }
    }
}
