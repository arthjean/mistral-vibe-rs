//! What resolving a plugin tree reports when something in it is wrong.
//!
//! Reference `vibe/core/plugins/_diagnostics.py` declares the closed code
//! vocabulary and `PluginConfigIssue` in `_native.py` carries one report: the
//! file it concerns, the message, a severity, an optional code, whether it
//! dropped the plugin, the format the plugin was read as, and the component
//! kind. The codes are reproduced verbatim because clients and the reload
//! notices key on them; the messages are this port's own prose.

use std::path::PathBuf;

use super::compatibility::DetectedPluginFormat;

pub const MANIFEST_INVALID: &str = "plugin.manifest.invalid";
pub const SCHEMA_VERSION_UNSUPPORTED: &str = "plugin.schema.version_unsupported";
pub const FORMAT_AMBIGUOUS: &str = "plugin.compatibility.format_ambiguous";
pub const FORMAT_UNRECOGNIZED: &str = "plugin.compatibility.format_unrecognized";
pub const EXECUTABLE_FORMAT_UNSUPPORTED: &str =
    "plugin.compatibility.executable_format_unsupported";
pub const FORMAT_UNSUPPORTED_BUILTIN: &str = "plugin.compatibility.format_unsupported_builtin";
pub const ADAPTER_UNAVAILABLE: &str = "plugin.compatibility.adapter_unavailable";
pub const NAMESPACE_RESERVED: &str = "plugin.namespace.reserved";
pub const PATH_OUTSIDE_ROOT: &str = "plugin.path.outside_root";
pub const FILESYSTEM_ERROR: &str = "plugin.filesystem.error";
pub const SKILL_INVALID: &str = "plugin.skill.invalid";
pub const SKILL_COLLISION: &str = "plugin.skill.collision";
pub const SKILL_MATERIALIZATION_FAILED: &str = "plugin.skill.materialization_failed";
pub const SKILL_OPENAI_METADATA_INVALID: &str = "plugin.skill.openai_metadata_invalid";
pub const HOOKS_INVALID: &str = "plugin.hooks.invalid";
pub const HOOKS_LIMIT_EXCEEDED: &str = "plugin.hooks.limit_exceeded";
pub const HOOKS_DUPLICATE_NAME: &str = "plugin.hooks.duplicate_name";
pub const KNOWLEDGE_INVALID: &str = "plugin.knowledge.invalid";
pub const KNOWLEDGE_LIMIT_EXCEEDED: &str = "plugin.knowledge.limit_exceeded";
pub const KNOWLEDGE_MATERIALIZATION_FAILED: &str = "plugin.knowledge.materialization_failed";
pub const AGENT_INVALID: &str = "plugin.agent.invalid";
pub const AGENT_LIMIT_EXCEEDED: &str = "plugin.agent.limit_exceeded";
pub const LIBRARIES_INVALID: &str = "plugin.libraries.invalid";
pub const LIBRARY_INVALID: &str = "plugin.library.invalid";
pub const LIBRARY_ALIAS_COLLISION: &str = "plugin.library.alias_collision";
pub const LIBRARY_RUNTIME_UNAVAILABLE: &str = "plugin.library.runtime_unavailable";
pub const LIBRARY_MATERIALIZATION_FAILED: &str = "plugin.library.materialization_failed";
pub const CONNECTORS_INVALID: &str = "plugin.connectors.invalid";
pub const CONNECTOR_RUNTIME_UNAVAILABLE: &str = "plugin.connector.runtime_unavailable";
pub const CONNECTOR_UNAVAILABLE: &str = "plugin.connector.unavailable";
pub const CONNECTOR_TOOL_UNAVAILABLE: &str = "plugin.connector.tool_unavailable";
pub const CONNECTOR_TOOL_SCHEMA_INVALID: &str = "plugin.connector.tool_schema_invalid";
pub const MCP_AUTHORIZATION_REQUIRED: &str = "plugin.mcp.authorization_required";
pub const MCP_CONNECTION_FAILED: &str = "plugin.mcp.connection_failed";
pub const MCP_SERVER_SHADOWED: &str = "plugin.mcp.server_shadowed";
pub const MCP_TOOL_SCHEMA_INVALID: &str = "plugin.mcp.tool_schema_invalid";
pub const TOOL_NAME_COLLISION: &str = "plugin.tool.name_collision";
pub const TOOL_OVERRIDE_UNUSED: &str = "plugin.tool_override.unused";

/// How serious a report is, spelled as the reference's literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    /// The literal a foreign adapter declares, read back.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "info" => Some(Self::Info),
            "warning" => Some(Self::Warning),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// One report a resolve produced. Reference `PluginConfigIssue`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginConfigIssue {
    pub file: PathBuf,
    pub message: String,
    pub severity: Severity,
    pub code: Option<String>,
    pub fatal: bool,
    pub source_format: Option<DetectedPluginFormat>,
    pub component: Option<String>,
}

impl PluginConfigIssue {
    /// A report with only a file and a message: severity `error`, no code,
    /// not fatal, which is the reference model's default.
    #[must_use]
    pub fn plain(file: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            message: message.into(),
            severity: Severity::Error,
            code: None,
            fatal: false,
            source_format: None,
            component: None,
        }
    }

    /// A coded report about one component of a plugin read in some format.
    #[must_use]
    pub fn coded(
        file: impl Into<PathBuf>,
        message: impl Into<String>,
        code: &str,
        source_format: Option<DetectedPluginFormat>,
        component: &str,
    ) -> Self {
        Self {
            code: Some(code.to_owned()),
            source_format,
            component: Some(component.to_owned()),
            ..Self::plain(file, message)
        }
    }

    #[must_use]
    pub fn fatal(mut self) -> Self {
        self.fatal = true;
        self
    }

    #[must_use]
    pub fn severity(mut self, severity: Severity) -> Self {
        self.severity = severity;
        self
    }
}
