//! Plugins: packages of skills, MCP servers, hooks, knowledge, agents,
//! libraries and connectors installed per project, per user, or shipped with
//! the binary.
//!
//! Reference `vibe/core/plugins/`. Plugins resolve only under the unified
//! harness backend (`vibe/app_server/_plugins.py`); this module owns the
//! provider-neutral part: discovery, validation, digests, the resolved
//! snapshot, and its materialization on disk.

pub mod adapters;
pub mod builtin;
pub mod canonical;
pub mod catalog;
pub mod claude;
pub mod codex;
pub mod compatibility;
pub mod content;
pub mod diagnostics;
pub mod drift;
pub mod foreign;
pub mod kimi;
pub mod materialize;
pub mod naming;
pub mod native;
pub mod paths;
pub mod redaction;
pub mod snapshot;
pub mod strict;

#[cfg(test)]
mod plugins_parity_tests;

pub use diagnostics::{PluginConfigIssue, Severity};
pub use native::{PluginDescriptor, PluginResolver, ResolvedPluginSet};
