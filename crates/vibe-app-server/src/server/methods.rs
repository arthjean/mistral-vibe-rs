//! The method and notification names this build answers and emits, which
//! the routing and the outbound gate read.

#[cfg(test)]
use super::{BTreeSet, PROJECTS_METHODS, RESOURCE_METHODS, WORKSPACE_METHODS, connection};

/// Every notification name this build emits, sorted and unique.
///
/// [`notification_method`] gates each outbound name against this list, so it is
/// what the server actually sends rather than a hand-kept description of it. The
/// app-server surface replay reads it to report notification conformance, and
/// `docs/parity.md` records the names here that the reference does not declare.
pub const EMITTED_NOTIFICATIONS: &[&str] = &[
    "connector_catalog/authFailed",
    "connector_catalog/authRequired",
    "connector_catalog/authUrl",
    "error",
    "history/entryAdded",
    "history/entryUpdated",
    "mcp/authUrl",
    "mcp_catalog/authRequired",
    "mcp_catalog/authUrl",
    "runtime/updated",
    "session/childSessionUpdated",
    "session/compacted",
    "session/contextCleared",
    "session/snapshot",
    "session/statsUpdated",
    "session/updated",
    "turn/completed",
    "turn/queueUpdated",
    "turn/retrying",
    "turn/started",
    "vibeCode/teleport/event",
    "warning",
];

/// Every method this build routes, sorted and unique, whether or not the
/// reference declares it.
#[cfg(test)]
pub(crate) fn routed_methods() -> BTreeSet<&'static str> {
    IMPLEMENTED_METHODS
        .iter()
        .chain(WORKSPACE_METHODS)
        .chain(PROJECTS_METHODS)
        .chain(RESOURCE_METHODS)
        .copied()
        .collect()
}

/// Every reference method a stdio client reaches an answer for: the ones a
/// handler of this port serves, and the ones the reference routing answers by
/// itself, as the reference does (`connection::route`).
#[cfg(test)]
pub(crate) fn stdio_routed_methods() -> BTreeSet<&'static str> {
    let served = routed_methods();
    vibe_protocol::SERVER_METHODS
        .iter()
        .copied()
        .filter(|method| {
            served.contains(method)
                || !matches!(
                    connection::route(method, false),
                    connection::Route::Host
                        | connection::Route::SessionOptional
                        | connection::Route::Root
                        | connection::Route::Catalog
                )
        })
        .collect()
}

/// Gates a notification name on [`EMITTED_NOTIFICATIONS`] as it leaves.
///
/// Both notification funnels pass through here: the frames a dispatch batch
/// encodes and the frames the live projection builds. A name emitted without
/// being declared is a surface the conformance replay would never see.
pub(crate) fn notification_method(method: &str) -> &str {
    debug_assert!(
        EMITTED_NOTIFICATIONS.contains(&method),
        "{method} is emitted but absent from EMITTED_NOTIFICATIONS"
    );
    method
}

/// The methods this port's own handlers answer, beside the service lists.
#[cfg(test)]
const IMPLEMENTED_METHODS: &[&str] = &[
    "account/read",
    "callback/respond",
    "callback/result",
    "connector_catalog/auth/request",
    "connector_catalog/read",
    "connector_catalog/refresh",
    "connector_catalog/toggle",
    "connectors/auth/read",
    "connectors/read",
    "connectors/refresh",
    "diagnostics/list",
    "diagnostics/logs/read",
    "feedback/record",
    "feedback/shouldShow",
    "mcp/add",
    "mcp/login",
    "mcp/logout",
    "mcp/read",
    "mcp/refresh",
    "mcp/toggle",
    "mcp_catalog/add",
    "mcp_catalog/login",
    "mcp_catalog/logout",
    "mcp_catalog/read",
    "mcp_catalog/refresh",
    "mcp_catalog/remove",
    "mcp_catalog/toggle",
    "narration/summarize",
    "providerAuth/read",
    "review/approve",
    "review/baseline",
    "review/hunks",
    "review/revert",
    "review/state",
    "review/turnDiff",
    "runtime/read",
    "session/close",
    "session/compact",
    "session/compact/start",
    "session/context/inject",
    "session/continue",
    "session/delete",
    "session/fork",
    "session/history/clear",
    "session/history/get",
    "session/history/list",
    "session/list",
    "session/log/read",
    "session/overrides/write",
    "session/pin",
    "session/read",
    "session/ready/read",
    "session/ready/wait",
    "session/relocate",
    "session/rename",
    "session/resume",
    "session/rewind",
    "session/rewind/read",
    "session/settings/update",
    "session/shellCommand",
    "session/start",
    "session/stop",
    "session/title/update",
    "session/turn/enqueue",
    "session/turn/queue/read",
    "session/turn/queue/remove",
    "session/turn/queue/replace",
    "session/turn/queue/resume",
    "session/turns/list",
    "skills/catalog",
    "skills/convertLocal",
    "skills/detail",
    "skills/import",
    "skills/installed",
    "skills/remove",
    "skills/setAlias",
    "skills/setEnabled",
    "skills/setLatest",
    "skills/setVersion",
    "skills/updates",
    "skills/versions",
    "stats/read",
    "telemetry/record",
    "tools/list",
    "turn/interrupt",
    "turn/start",
    "turn/steer",
    "workspace/trust/decision",
    "workspace/trust/status",
    "workspace/trust/untrustedConfig",
];
