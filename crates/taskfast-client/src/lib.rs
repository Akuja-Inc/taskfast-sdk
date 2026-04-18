//! TaskFast typed HTTP client.
//!
//! The `api` module is generated from `spec/openapi.yaml` at build time by
//! `build.rs`. The rewritten spec (error-alias folding) is produced in-memory
//! via `xtask::normalize_spec` so the on-disk spec stays authoritative.
//!
//! Use [`api::Client`] to issue requests; cross-cutting concerns
//! ([`errors::Error`], [`retry::with_backoff`]) live in sibling modules and
//! will be composed over the generated client in a follow-up.

pub mod client;
pub mod errors;
pub mod retry;

pub use client::{map_api_error, TaskFastClient, UserProfile};
pub use errors::{Error, Result};
pub use retry::{with_backoff, RetryPolicy};

/// Generated typed client + DTOs for the TaskFast OpenAPI spec.
///
/// Produced by `progenitor` from `spec/openapi.yaml` at build time; see
/// `build.rs` and `xtask::normalize_spec`. Do not edit by hand — regenerate
/// by changing the spec.
#[allow(
    clippy::all,
    clippy::pedantic,
    dead_code,
    irrefutable_let_patterns,
    missing_docs,
    non_camel_case_types,
    non_snake_case,
    renamed_and_removed_lints,
    unknown_lints,
    rustdoc::broken_intra_doc_links,
    rustdoc::invalid_html_tags
)]
pub mod api {
    include!(concat!(env!("OUT_DIR"), "/codegen.rs"));
}
