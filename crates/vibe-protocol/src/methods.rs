//! The method names the contract declares, and the predicates that answer
//! whether a name belongs to one of them.

/// Every method the reference declares, sorted and unique.
///
/// This is the contract, not the routing table: a name belongs here because the
/// reference declares it, whether or not this build answers it yet. What a build
/// actually routes is what it advertises in
/// [`ServerCapabilities::methods`](crate::ServerCapabilities::methods).
///
/// Lifecycle methods (`initialize`, `initialized`, `shutdown`, `exit`) are
/// deliberately absent: they are handled before method dispatch and are not
/// part of the negotiated surface.
pub const SERVER_METHODS: [&str; 91] = [
    "account/read",
    "agents/install",
    "agents/list",
    "agents/uninstall",
    "callback/respond",
    "config/fields/read",
    "config/patch",
    "config/proxy/read",
    "config/proxy/write",
    "config/read",
    "config/reload",
    "config/schema",
    "config/thinking/write",
    "connectors/auth/read",
    "connectors/read",
    "connectors/refresh",
    "diagnostics/list",
    "diagnostics/logs/read",
    "feedback/record",
    "feedback/shouldShow",
    "history/list",
    "identity/read",
    "loops/clear",
    "loops/create",
    "loops/delete",
    "loops/list",
    "mcp/add",
    "mcp/login",
    "mcp/logout",
    "mcp/read",
    "mcp/refresh",
    "mcp/toggle",
    "narration/summarize",
    "projectLinks/create",
    "projectLinks/inspectRoot",
    "projectLinks/link",
    "projectLinks/list",
    "projectLinks/picker/load",
    "projectLinks/picker/loadMore",
    "projectLinks/resolveRoot",
    "projectLinks/save",
    "projectLinks/unlink",
    "review/approve",
    "review/baseline",
    "review/hunks",
    "review/revert",
    "review/state",
    "review/turnDiff",
    "runtime/read",
    "session/agent/update",
    "session/close",
    "session/compact/start",
    "session/context/inject",
    "session/continue",
    "session/delete",
    "session/fork",
    "session/history/clear",
    "session/list",
    "session/log/read",
    "session/read",
    "session/ready/read",
    "session/ready/wait",
    "session/resume",
    "session/rewind",
    "session/rewind/read",
    "session/settings/update",
    "session/start",
    "session/title/update",
    "shell/interrupt",
    "shell/run",
    "skills/list",
    "stats/read",
    "telemetry/record",
    "tools/list",
    "turn/interrupt",
    "turn/start",
    "turn/steer",
    "vibeCode/projects/cancel",
    "vibeCode/projects/create",
    "vibeCode/projects/loadMore",
    "vibeCode/projects/open",
    "vibeCode/projects/recover",
    "vibeCode/projects/select",
    "vibeCode/projects/unlink",
    "vibeCode/teleport/cancel",
    "vibeCode/teleport/push/respond",
    "vibeCode/teleport/start",
    "workspace/prompt/prepare",
    "workspace/trust/decision",
    "workspace/trust/status",
    "workspace/worktrees/list",
];

/// Methods this port routes that the reference does not declare, sorted and
/// unique.
///
/// They stay routable for the clients already calling them, and stay out of
/// [`SERVER_METHODS`] and out of the advertised capabilities, so a client
/// written against the reference protocol never learns a name only this
/// implementation answers. Each one has a row in the Accepted divergences table
/// of `docs/parity.md`.
pub const LOCAL_EXTENSION_METHODS: [&str; 4] = [
    "config/batchWrite",
    "connectors/toggle",
    "mcp/auth/complete",
    "session/overrides/write",
];

/// Reports whether `method` is part of the negotiated surface.
#[must_use]
pub fn is_server_method(method: &str) -> bool {
    SERVER_METHODS.binary_search(&method).is_ok()
}

/// Reports whether `method` is one of this port's local extensions.
pub(crate) fn is_local_extension_method(method: &str) -> bool {
    LOCAL_EXTENSION_METHODS.binary_search(&method).is_ok()
}

/// Reports whether `method` may be dispatched at all: the reference surface plus
/// the local extensions.
#[must_use]
pub fn is_dispatchable_method(method: &str) -> bool {
    is_server_method(method) || is_local_extension_method(method)
}

#[cfg(test)]
mod methods_tests;
