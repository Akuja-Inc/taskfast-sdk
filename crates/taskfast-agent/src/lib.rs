//! TaskFast agent orchestration library.
//!
//! Phase 1 scaffold: module stubs only. Each module's body (bootstrap flow,
//! wallet provisioning, webhook registration, event polling, EIP-712 signing,
//! keystore I/O) is filled in by follow-up tasks off the am-e3u epic.

// TODO: tighten doc coverage on public items + remove these allows.
// Tracked under the rust-best-practices follow-up rolled out alongside the
// taskfast-client 0.3 release.
#![allow(missing_docs)]

pub mod bootstrap;
pub mod events;
pub mod faucet;
pub mod keystore;
pub mod poster;
pub mod retry;
pub mod signing;
pub mod tempo_rpc;
pub mod wallet;
pub mod webhooks;
