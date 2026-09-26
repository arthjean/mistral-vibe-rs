//! Hooks: the commands an operator runs around a tool call or a turn.
//!
//! Reference `vibe/core/hooks/`. A session loads its hooks from the
//! `hooks.toml` files its harness lists (every open project's
//! `.vibe/hooks.toml`, then the user's `$VIBE_HOME/hooks.toml`), and the
//! conversation engine runs them at three points: `pre_tool` before a call is
//! gated, where a hook may deny the call or rewrite its arguments; `post_tool`
//! after it ran, where a hook may replace or extend what the model reads; and
//! `post_agent` when the model is done, where a hook may send it back with a
//! message, at most three times per hook and user turn.
//!
//! A hook reads its invocation as JSON on stdin and answers on stdout with an
//! exit status of zero and either nothing or a response object. Anything else
//! is a failure, which warns and passes unless the hook is strict.

mod config;
mod executor;
mod json;
mod manager;
mod models;

#[cfg(test)]
mod hooks_tests;

pub use config::{load_hooks_file, load_hooks_from_fs};
pub use json::python_json_dumps;
pub use manager::{HooksManager, MAX_RETRIES};
pub use models::{
    DEFAULT_HOOK_TIMEOUT, HookConfig, HookConfigIssue, HookConfigResult, HookEvent,
    HookExecutionResult, HookInvocation, HookSessionContext, HookSeverity, HookType, HookYield,
    ToolStatus,
};

/// The file a directory's hooks are declared in.
pub const HOOKS_FILE: &str = "hooks.toml";
