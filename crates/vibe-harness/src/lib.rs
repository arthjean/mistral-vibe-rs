#![cfg_attr(test, allow(clippy::expect_used, clippy::panic, clippy::unwrap_used))]
//! The Unified Harness, the backend a reference session runs on by default
//! since v2.26.0 (`vibe/_experimental_harness.py`), and row 36 of
//! `docs/parity.md`.
//!
//! The reference splits it into a native core, a runtime driving it and an
//! app-server adapter (`harness/core/`, `harness/runtimes/python/`,
//! `vibe/app_server/_unified_harness_backend_adapter.py`). What a client reads
//! of it is a public session state the runtime republishes after every
//! committed step, and notifications the adapter derives by diffing each state
//! against the one before. This crate keeps that contract: the engine that
//! produces the states, the projection that shapes them and the diff that
//! turns them into notifications.

pub mod catalog;
pub mod code_mode;
pub mod compaction;
mod connector_labels;
pub mod discovery;
pub mod engine;
pub mod gate;
pub mod invocation;
pub mod loops;
pub mod patch;
pub mod processes;
pub mod programmatic;
pub mod prompt;
pub mod reconcile;
pub mod rewind;
pub mod skills;
pub mod smart;
pub mod state;
pub mod store;
pub mod subagents;
pub mod title;
pub mod tool_projection;
pub mod tools;
mod utility_call;
pub mod wire;
