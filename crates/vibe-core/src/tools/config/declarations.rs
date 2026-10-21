//! What each tool class declares: its configuration keys and their defaults,
//! under the names an operator writes in `tools.<name>`.

use serde_json::Value as JsonValue;
use toml::Value;

use super::ToolConfigResolver;
use super::lists::{shell_allowlist, shell_denylist, shell_denylist_standalone};
use crate::policy::PermissionMode;

/// What a tool declares for one configuration key.
///
/// The variants are the value kinds the reference config models use, plus the
/// three host-composed shell lists, which cannot be a constant because the
/// reference builds them from the running shell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToolConfigDefault {
    /// Reference `BaseToolConfig.permission`.
    Permission(PermissionMode),
    Flag(bool),
    /// A whole number: a byte budget, a count, or a timeout in seconds.
    Integer(i64),
    /// A fractional number of seconds, which the reference types as `float`.
    Seconds(f64),
    Text(&'static str),
    Texts(&'static [&'static str]),
    /// Reference `_get_default_allowlist`.
    ShellAllowlist,
    /// Reference `_get_default_denylist`.
    ShellDenylist,
    /// Reference `_get_default_denylist_standalone`.
    ShellDenylistStandalone,
}

impl ToolConfigDefault {
    /// The declared value, for a host whose shell is POSIX or is not.
    #[must_use]
    pub fn value(self, posix_shell: bool) -> Value {
        match self {
            Self::Permission(mode) => Value::String(permission_label(mode).to_owned()),
            Self::Flag(flag) => Value::Boolean(flag),
            Self::Integer(number) => Value::Integer(number),
            Self::Seconds(seconds) => Value::Float(seconds),
            Self::Text(text) => Value::String(text.to_owned()),
            Self::Texts(entries) => text_array(entries),
            Self::ShellAllowlist => text_array(&shell_allowlist(posix_shell)),
            Self::ShellDenylist => text_array(shell_denylist(posix_shell)),
            Self::ShellDenylistStandalone => text_array(shell_denylist_standalone(posix_shell)),
        }
    }

    /// Whether `value` is a value this key can carry.
    ///
    /// A number is accepted for the other numeric kind when it is exact, the way
    /// the reference Pydantic accepts an `int` for a `float` field and a float
    /// with no fractional part for an `int` one.
    pub(super) fn accepts(self, value: &Value) -> bool {
        match self {
            Self::Permission(_) => value
                .as_str()
                .is_some_and(|text| parse_permission(text).is_some()),
            Self::Flag(_) => value.is_bool(),
            Self::Integer(_) => value.is_integer() || integral_float(value).is_some(),
            Self::Seconds(_) => value.is_float() || value.is_integer(),
            Self::Text(_) => value.is_str(),
            Self::Texts(_)
            | Self::ShellAllowlist
            | Self::ShellDenylist
            | Self::ShellDenylistStandalone => value
                .as_array()
                .is_some_and(|entries| entries.iter().all(Value::is_str)),
        }
    }

    /// Whether this key carries a limit that must stay positive.
    pub(super) fn is_limit(self) -> bool {
        matches!(self, Self::Integer(_) | Self::Seconds(_))
    }

    /// The kind name a diagnostic uses when a configured value is refused.
    pub(super) fn kind(self) -> &'static str {
        match self {
            Self::Permission(_) => "one of `ask`, `always` or `never`",
            Self::Flag(_) => "a boolean",
            Self::Integer(_) => "an integer",
            Self::Seconds(_) => "a number",
            Self::Text(_) => "a string",
            Self::Texts(_)
            | Self::ShellAllowlist
            | Self::ShellDenylist
            | Self::ShellDenylistStandalone => "an array of strings",
        }
    }
}

/// One configurable key, under the name the reference config model gives it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToolConfigKey {
    pub name: &'static str,
    pub default: ToolConfigDefault,
}

/// Every key one tool declares, under the name it publishes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToolConfigDeclaration {
    pub tool: &'static str,
    pub keys: &'static [ToolConfigKey],
}

impl ToolConfigDeclaration {
    /// The declared default for `key`, or [`None`] when this tool has no such
    /// key.
    #[must_use]
    pub fn default_for(&self, key: &str) -> Option<ToolConfigDefault> {
        self.keys
            .iter()
            .find(|declared| declared.name == key)
            .map(|declared| declared.default)
    }
}

/// The four keys `BaseToolConfig` gives every tool, and the tool-specific ones
/// after them.
macro_rules! declare_tool {
    (
        $tool:literal,
        permission: $permission:expr,
        allowlist: $allowlist:expr,
        denylist: $denylist:expr,
        sensitive_patterns: $sensitive:expr
        $(, $key:literal: $default:expr)* $(,)?
    ) => {
        ToolConfigDeclaration {
            tool: $tool,
            keys: &[
                ToolConfigKey { name: "permission", default: ToolConfigDefault::Permission($permission) },
                ToolConfigKey { name: "allowlist", default: $allowlist },
                ToolConfigKey { name: "denylist", default: $denylist },
                ToolConfigKey { name: "sensitive_patterns", default: $sensitive },
                $(ToolConfigKey { name: $key, default: $default },)*
            ],
        }
    };
}

/// The keys `BaseToolConfig` declares, for a tool no declaration covers.
///
/// Reference `get_tool_config` falls back to `BaseToolConfig` for an unknown
/// name, which is what an MCP or connector tool with a `tools.<name>` entry
/// resolves through.
pub const BASE_DECLARATION: ToolConfigDeclaration = declare_tool!(
    "",
    permission: PermissionMode::Ask,
    allowlist: ToolConfigDefault::Texts(&[]),
    denylist: ToolConfigDefault::Texts(&[]),
    sensitive_patterns: ToolConfigDefault::Texts(&[]),
);

/// Reference `DEFAULT_SENSITIVE_PATTERNS` (`vibe/core/tools/utils.py`), the
/// `sensitive_patterns` default of `read_file`, `write_file`, `edit` and `grep`.
pub(crate) const DOTENV_PATTERNS: &[&str] = &[
    "**/.env",
    "**/.env.*",
    "**/.env~",
    "**/.envrc",
    "**/.envrc.*",
    "**/.envrc~",
];

/// Reference `grep.py` `exclude_patterns`.
const GREP_EXCLUDE_PATTERNS: &[&str] = &[
    ".venv/",
    "venv/",
    ".env/",
    "env/",
    "node_modules/",
    ".git/",
    "__pycache__/",
    ".pytest_cache/",
    ".mypy_cache/",
    ".tox/",
    ".nox/",
    ".coverage/",
    "htmlcov/",
    "dist/",
    "build/",
    ".idea/",
    ".vscode/",
    "*.egg-info",
    "*.pyc",
    "*.pyo",
    "*.pyd",
    ".DS_Store",
    "Thumbs.db",
];

/// Reference `web_fetch.py` `user_agent`: a browser string, because a plain bot
/// agent is refused by a large share of the pages an operator asks for.
const FETCH_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/120.0.0.0 Safari/537.36";

/// One shell family's declaration: the reference publishes the same config
/// model for `bash`, `git_bash` and `powershell`.
macro_rules! declare_shell_family {
    ($command:literal, $output:literal, $stdin:literal, $sessions:literal, $log_file:literal) => {
        [
            declare_tool!(
                $command,
                permission: PermissionMode::Ask,
                allowlist: ToolConfigDefault::ShellAllowlist,
                denylist: ToolConfigDefault::ShellDenylist,
                sensitive_patterns: ToolConfigDefault::Texts(&["sudo"]),
                "default_timeout": ToolConfigDefault::Integer(300),
                "denylist_standalone": ToolConfigDefault::ShellDenylistStandalone,
                "max_inline_bytes": ToolConfigDefault::Integer(30_000),
                "max_output_bytes": ToolConfigDefault::Integer(16_000),
                "max_timeout_seconds": ToolConfigDefault::Seconds(600.0),
            ),
            declare_tool!(
                $output,
                permission: PermissionMode::Always,
                allowlist: ToolConfigDefault::Texts(&[]),
                denylist: ToolConfigDefault::Texts(&[]),
                sensitive_patterns: ToolConfigDefault::Texts(&[]),
                "max_inline_bytes": ToolConfigDefault::Integer(30_000),
                "max_poll_seconds": ToolConfigDefault::Seconds(300.0),
            ),
            declare_tool!(
                $stdin,
                permission: PermissionMode::Always,
                allowlist: ToolConfigDefault::Texts(&[]),
                denylist: ToolConfigDefault::Texts(&[]),
                sensitive_patterns: ToolConfigDefault::Texts(&[]),
            ),
            declare_tool!(
                $sessions,
                permission: PermissionMode::Always,
                allowlist: ToolConfigDefault::Texts(&[]),
                denylist: ToolConfigDefault::Texts(&[]),
                sensitive_patterns: ToolConfigDefault::Texts(&[]),
                "max_inline_bytes": ToolConfigDefault::Integer(30_000),
            ),
            declare_tool!(
                $log_file,
                permission: PermissionMode::Ask,
                allowlist: ToolConfigDefault::Texts(&[]),
                denylist: ToolConfigDefault::Texts(&[]),
                sensitive_patterns: ToolConfigDefault::Texts(&[]),
                "max_inline_bytes": ToolConfigDefault::Integer(30_000),
            ),
        ]
    };
}

const BASH_FAMILY: [ToolConfigDeclaration; 5] = declare_shell_family!(
    "bash",
    "bash_output",
    "bash_stdin",
    "bash_sessions",
    "bash_log_file"
);

const GIT_BASH_FAMILY: [ToolConfigDeclaration; 5] = declare_shell_family!(
    "git_bash",
    "git_bash_output",
    "git_bash_stdin",
    "git_bash_sessions",
    "git_bash_log_file"
);

const POWERSHELL_FAMILY: [ToolConfigDeclaration; 5] = declare_shell_family!(
    "powershell",
    "powershell_output",
    "powershell_stdin",
    "powershell_sessions",
    "powershell_log_file"
);

/// The universal tools, the file tools and the three shell families: the 26
/// classes reference `discover_tool_defaults` enumerates.
pub const DECLARATIONS: [ToolConfigDeclaration; 26] = [
    declare_tool!(
        "ask_user_question",
        permission: PermissionMode::Always,
        allowlist: ToolConfigDefault::Texts(&[]),
        denylist: ToolConfigDefault::Texts(&[]),
        sensitive_patterns: ToolConfigDefault::Texts(&[]),
    ),
    BASH_FAMILY[0],
    BASH_FAMILY[1],
    BASH_FAMILY[2],
    BASH_FAMILY[3],
    BASH_FAMILY[4],
    declare_tool!(
        "edit",
        permission: PermissionMode::Ask,
        allowlist: ToolConfigDefault::Texts(&[]),
        denylist: ToolConfigDefault::Texts(&[]),
        sensitive_patterns: ToolConfigDefault::Texts(DOTENV_PATTERNS),
    ),
    declare_tool!(
        "exit_plan_mode",
        permission: PermissionMode::Always,
        allowlist: ToolConfigDefault::Texts(&[]),
        denylist: ToolConfigDefault::Texts(&[]),
        sensitive_patterns: ToolConfigDefault::Texts(&[]),
    ),
    GIT_BASH_FAMILY[0],
    GIT_BASH_FAMILY[1],
    GIT_BASH_FAMILY[2],
    GIT_BASH_FAMILY[3],
    GIT_BASH_FAMILY[4],
    declare_tool!(
        "grep",
        permission: PermissionMode::Always,
        allowlist: ToolConfigDefault::Texts(&[]),
        denylist: ToolConfigDefault::Texts(&[]),
        sensitive_patterns: ToolConfigDefault::Texts(DOTENV_PATTERNS),
        "codeignore_file": ToolConfigDefault::Text(".vibeignore"),
        "default_max_matches": ToolConfigDefault::Integer(100),
        "default_timeout": ToolConfigDefault::Integer(60),
        "exclude_patterns": ToolConfigDefault::Texts(GREP_EXCLUDE_PATTERNS),
        "max_output_bytes": ToolConfigDefault::Integer(64_000),
    ),
    POWERSHELL_FAMILY[0],
    POWERSHELL_FAMILY[1],
    POWERSHELL_FAMILY[2],
    POWERSHELL_FAMILY[3],
    POWERSHELL_FAMILY[4],
    declare_tool!(
        "read_file",
        permission: PermissionMode::Always,
        allowlist: ToolConfigDefault::Texts(&[]),
        denylist: ToolConfigDefault::Texts(&[]),
        sensitive_patterns: ToolConfigDefault::Texts(DOTENV_PATTERNS),
        "max_read_bytes": ToolConfigDefault::Integer(51_200),
    ),
    declare_tool!(
        "skill",
        permission: PermissionMode::Always,
        allowlist: ToolConfigDefault::Texts(&[]),
        denylist: ToolConfigDefault::Texts(&[]),
        sensitive_patterns: ToolConfigDefault::Texts(&[]),
    ),
    declare_tool!(
        "task",
        permission: PermissionMode::Ask,
        allowlist: ToolConfigDefault::Texts(&["explore"]),
        denylist: ToolConfigDefault::Texts(&[]),
        sensitive_patterns: ToolConfigDefault::Texts(&[]),
    ),
    declare_tool!(
        "todo",
        permission: PermissionMode::Always,
        allowlist: ToolConfigDefault::Texts(&[]),
        denylist: ToolConfigDefault::Texts(&[]),
        sensitive_patterns: ToolConfigDefault::Texts(&[]),
        "max_todos": ToolConfigDefault::Integer(100),
    ),
    declare_tool!(
        "web_fetch",
        permission: PermissionMode::Ask,
        allowlist: ToolConfigDefault::Texts(&[]),
        denylist: ToolConfigDefault::Texts(&[]),
        sensitive_patterns: ToolConfigDefault::Texts(&[]),
        "default_timeout": ToolConfigDefault::Integer(30),
        "max_content_bytes": ToolConfigDefault::Integer(120_000),
        "max_timeout": ToolConfigDefault::Integer(120),
        "user_agent": ToolConfigDefault::Text(FETCH_USER_AGENT),
    ),
    declare_tool!(
        "web_search",
        permission: PermissionMode::Ask,
        allowlist: ToolConfigDefault::Texts(&[]),
        denylist: ToolConfigDefault::Texts(&[]),
        sensitive_patterns: ToolConfigDefault::Texts(&[]),
        "model": ToolConfigDefault::Text("mistral-vibe-cli-with-tools"),
        "timeout": ToolConfigDefault::Integer(120),
    ),
    declare_tool!(
        "write_file",
        permission: PermissionMode::Ask,
        allowlist: ToolConfigDefault::Texts(&[]),
        denylist: ToolConfigDefault::Texts(&[]),
        sensitive_patterns: ToolConfigDefault::Texts(DOTENV_PATTERNS),
        "create_parent_dirs": ToolConfigDefault::Flag(true),
        "max_write_bytes": ToolConfigDefault::Integer(64_000),
    ),
];

/// The configuration document a [`crate::tools::ToolSpec`] publishes for
/// `tool`: its declared keys and their defaults, under the names an operator
/// writes in `tools.<name>`.
///
/// A spec is a declaration and holds no resolver, so this answers from the
/// table rather than from a composition. The shell lists follow the host the
/// binary runs on, which is the branch the reference composes there too.
#[must_use]
pub fn declared_document(tool: &str) -> JsonValue {
    // A table of declared defaults always converts: its keys are strings, and
    // no default is a datetime or a float JSON cannot hold.
    serde_json::to_value(ToolConfigResolver::new().view_document(tool)).unwrap_or(JsonValue::Null)
}

/// The declaration `tool` publishes, or [`None`] when no class declares it.
#[must_use]
pub fn declaration(tool: &str) -> Option<&'static ToolConfigDeclaration> {
    DECLARATIONS
        .iter()
        .find(|declaration| declaration.tool == tool)
}

/// The value the reference writes for a permission, which is the lowercase
/// member name rather than the wire form [`PermissionMode`] serializes.
#[must_use]
pub fn permission_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Never => "never",
        PermissionMode::Ask => "ask",
        PermissionMode::Always => "always",
    }
}

/// The permission `text` names, case-insensitively, or [`None`] when it names
/// none.
#[must_use]
pub fn parse_permission(text: &str) -> Option<PermissionMode> {
    match text.trim().to_ascii_lowercase().as_str() {
        "never" => Some(PermissionMode::Never),
        "ask" => Some(PermissionMode::Ask),
        "always" => Some(PermissionMode::Always),
        _ => None,
    }
}

fn text_array(entries: &[&str]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|entry| Value::String((*entry).to_owned()))
            .collect(),
    )
}

/// The whole number a float carries, when it carries one exactly.
pub(super) fn integral_float(value: &Value) -> Option<i64> {
    let number = value.as_float()?;
    if number.fract() != 0.0 || !number.is_finite() {
        return None;
    }
    Some(number as i64)
}
