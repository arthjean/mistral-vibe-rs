//! What an effect carries when the tool it ran is one this session only
//! proxies: the registry that published it, and how its answer settles.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which registry published a tool this session only proxies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteToolKind {
    Mcp,
    Connector,
}

/// The remote a proxied call reaches, carried beside the published name.
///
/// The published name is not invertible: `public_tool_name` sanitizes an MCP
/// name and joins it to a sanitized alias, so `acme_create_issue` no longer
/// says where the alias stops. The presentation needs the name the server
/// itself publishes, so the registration that knows it hands it forward rather
/// than letting the projection guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteToolOrigin {
    pub kind: RemoteToolKind,
    /// The name the server publishes the tool under, which the alias prefix is
    /// absent from.
    pub remote_name: String,
    /// The alias of the server or connector that publishes it, as configured,
    /// which the Unified Harness names the tool's group after. Empty when the
    /// registration did not record one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
}

impl RemoteToolOrigin {
    /// The origin of a tool an MCP server publishes as `remote_name`.
    #[must_use]
    pub fn mcp(remote_name: impl Into<String>) -> Self {
        Self {
            kind: RemoteToolKind::Mcp,
            remote_name: remote_name.into(),
            provider: String::new(),
        }
    }

    /// The origin of a tool a connector publishes as `remote_name`.
    #[must_use]
    pub fn connector(remote_name: impl Into<String>) -> Self {
        Self {
            kind: RemoteToolKind::Connector,
            remote_name: remote_name.into(),
            provider: String::new(),
        }
    }

    /// The same origin, naming `provider` as the alias that publishes it.
    #[must_use]
    pub fn from_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    /// What the loading indicator says while a proxied call runs.
    ///
    /// The reference's two proxy factories close over the remote tool, so the
    /// sentence names the tool as its server published it and never the alias
    /// prefix the session added. A registration that recorded no remote name
    /// falls back to the published one rather than naming nothing.
    #[must_use]
    pub(super) fn status_text(&self, published_name: &str) -> String {
        let named = if self.remote_name.is_empty() {
            published_name
        } else {
            &self.remote_name
        };
        match self.kind {
            RemoteToolKind::Mcp => format!("Calling MCP tool {named}"),
            RemoteToolKind::Connector => format!("Calling connector tool {named}"),
        }
    }

    /// The subject the settled header names, which the connector prefixes.
    pub(super) fn settled_subject(&self, answered: &str) -> String {
        match self.kind {
            RemoteToolKind::Mcp => answered.to_owned(),
            RemoteToolKind::Connector => format!("connector {answered}"),
        }
    }
}

/// What a settled proxied call reports, in the three fields the reference's
/// adapter reads before it hands the result to the proxy.
///
/// They are named rather than positional because two of the three are strings
/// that would otherwise be transposable, and reading an error as a skip reason
/// would publish a failure as a skip.
#[derive(Debug, Clone, Copy)]
pub struct RemoteSettlement<'a> {
    /// What the call failed with, empty when it did not fail.
    pub error: &'a str,
    /// Whether the call never ran, which a reason alone does not say.
    pub skipped: bool,
    /// Why it never ran, empty when nothing named a reason.
    pub skip_reason: &'a str,
    /// The typed result the server answered with.
    pub output: &'a Value,
}

impl<'a> RemoteSettlement<'a> {
    /// The settlement of a call that answered with `output`.
    #[must_use]
    pub fn answered(output: &'a Value) -> Self {
        Self {
            error: "",
            skipped: false,
            skip_reason: "",
            output,
        }
    }

    /// The settlement of a call that failed with `error`.
    #[must_use]
    pub fn failed(error: &'a str, output: &'a Value) -> Self {
        Self {
            error,
            skipped: false,
            skip_reason: "",
            output,
        }
    }

    /// The settlement of a call that never ran.
    #[must_use]
    pub fn skipped(skip_reason: &'a str, output: &'a Value) -> Self {
        Self {
            error: "",
            skipped: true,
            skip_reason,
            output,
        }
    }
}

/// The result a remote server answered with, as far as the settled header
/// reads it.
///
/// The reference models it as `MCPToolResult`, which its own proxy builds, so
/// every field it reads is the proxy's. This port's peers publish the server's
/// `structuredContent` verbatim as the typed result, so the same keys can be
/// the server's instead: a tool answering `{"ok": false}` about its own subject
/// is not a proxy reporting a failed call. `server` is the field that tells the
/// two apart, because only the proxy's own result names the source, which is
/// why it is required here rather than defaulted. A payload that is not an
/// object at all is what the reference's `isinstance` check rejects, and it
/// settles as a failure.
#[derive(Debug, Deserialize)]
pub(super) struct RemoteToolResult {
    #[serde(default = "remote_result_ok")]
    pub(super) ok: bool,
    #[serde(default)]
    pub(super) tool: Option<String>,
    #[serde(default)]
    server: Option<String>,
}

impl RemoteToolResult {
    /// Whether this payload is the proxy's own result rather than the content
    /// the remote tool answered about its own subject.
    pub(super) const fn is_the_proxys_own(&self) -> bool {
        self.server.is_some()
    }
}

const fn remote_result_ok() -> bool {
    true
}
