//! TaskFast typed HTTP client.
//!
//! The `api` module is generated from `spec/openapi.yaml` at build time by
//! `build.rs`. The rewritten spec (error-alias folding) is produced in-memory
//! via `xtask::normalize_spec` so the on-disk spec stays authoritative.
//!
//! Use [`api::Client`] to issue requests; cross-cutting concerns
//! ([`errors::Error`], [`retry::with_backoff`]) live in sibling modules and
//! will be composed over the generated client in a follow-up.

pub mod errors;
pub mod retry;

pub use errors::{Error, Result};
pub use retry::{RetryPolicy, with_backoff};

#[allow(
    clippy::all,
    dead_code,
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
