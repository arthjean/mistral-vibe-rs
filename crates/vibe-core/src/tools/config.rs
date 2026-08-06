//! The configuration one tool resolves, composed the way the reference
//! composes it.
//!
//! Reference `ToolManager.get_tool_config` (`vibe/core/tools/manager.py:620`)
//! merges three things into the `BaseToolConfig` subclass a tool declares: the
//! class defaults, the `tools.<name>` table an operator writes, and the session
//! permission override `PermissionStore.get_tool_permission` answers with. A
//! tool then reads its limits off that object at every call, which is why a
//! handler here holds a [`ToolConfigResolver`] rather than a value: an operator
//! who raises a budget between two turns is obeyed on the second one without
//! the surface being registered again.
//!
//! [`DECLARATIONS`] is the declaration half. It carries the 26 tool classes the
//! reference enumerates through `ToolManager.discover_tool_defaults`, the 22
//! distinct keys they draw from and the 146 `(tool, key)` pairs that enumeration
//! returns, all replayed against the committed corpus by
//! `crate::tools::config_parity_tests`.
//!
//! Three of those defaults are host-dependent upstream: the shell allowlist,
//! denylist and standalone denylist are composed from `uses_posix_shell()`, so
//! they are declared here as the two branches the reference composes and
//! selected by [`ToolConfigResolver::with_posix_shell`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock, TryLockError};

use serde_json::Value as JsonValue;
use toml::{Table, Value};

use crate::config::{ConfigSnapshot, LayeredConfig};
use crate::policy::PermissionMode;

/// The `tools` table an operator writes settings into, and the key every
/// resolved document is addressed under.
pub const TOOL_SETTINGS_KEY: &str = "tools";

// --------------------------------------------------------------------------
// Declarations
// --------------------------------------------------------------------------

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
            Self::ShellAllowlist => text_array(shell_allowlist(posix_shell)),
            Self::ShellDenylist => text_array(shell_denylist(posix_shell)),
            Self::ShellDenylistStandalone => text_array(shell_denylist_standalone(posix_shell)),
        }
    }

    /// Whether `value` is a value this key can carry.
    ///
    /// A number is accepted for the other numeric kind when it is exact, the way
    /// the reference Pydantic accepts an `int` for a `float` field and a float
    /// with no fractional part for an `int` one.
    fn accepts(self, value: &Value) -> bool {
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
    fn is_limit(self) -> bool {
        matches!(self, Self::Integer(_) | Self::Seconds(_))
    }

    /// The kind name a diagnostic uses when a configured value is refused.
    fn kind(self) -> &'static str {
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

/// Reference `read_file.py` and `edit.py` `sensitive_patterns`.
const DOTENV_PATTERNS: &[&str] = &["**/.env", "**/.env.*"];

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

/// Reference `_get_default_allowlist`: the seven commands both branches share,
/// then `_READ_ONLY_COMMANDS_POSIX`.
const SHELL_ALLOWLIST_POSIX: [&str; 44] = [
    "cd",
    "echo",
    "git diff",
    "git log",
    "git status",
    "tree",
    "whoami",
    "basename",
    "cat",
    "comm",
    "cut",
    "date",
    "diff",
    "dirname",
    "du",
    "file",
    "find",
    "fmt",
    "fold",
    "grep",
    "head",
    "join",
    "less",
    "ls",
    "md5sum",
    "more",
    "nl",
    "od",
    "paste",
    "pwd",
    "readlink",
    "sha1sum",
    "sha256sum",
    "shasum",
    "sort",
    "stat",
    "sum",
    "tac",
    "tail",
    "tr",
    "uname",
    "uniq",
    "wc",
    "which",
];

/// The same seven, then `_READ_ONLY_COMMANDS_WINDOWS`.
const SHELL_ALLOWLIST_WINDOWS: [&str; 13] = [
    "cd",
    "echo",
    "git diff",
    "git log",
    "git status",
    "tree",
    "whoami",
    "dir",
    "findstr",
    "more",
    "type",
    "ver",
    "where",
];

/// Reference `_get_default_denylist`.
const SHELL_DENYLIST_POSIX: [&str; 14] = [
    "gdb", "pdb", "passwd", "nano", "vim", "vi", "emacs", "bash -i", "sh -i", "zsh -i", "fish -i",
    "dash -i", "screen", "tmux",
];

const SHELL_DENYLIST_WINDOWS: [&str; 7] = [
    "gdb",
    "pdb",
    "passwd",
    "cmd /k",
    "powershell -NoExit",
    "pwsh -NoExit",
    "notepad",
];

/// Reference `_get_default_denylist_standalone`.
const SHELL_DENYLIST_STANDALONE_POSIX: [&str; 11] = [
    "python", "python3", "ipython", "bash", "sh", "nohup", "vi", "vim", "emacs", "nano", "su",
];

const SHELL_DENYLIST_STANDALONE_WINDOWS: [&str; 7] = [
    "python",
    "python3",
    "ipython",
    "cmd",
    "powershell",
    "pwsh",
    "notepad",
];

#[must_use]
pub fn shell_allowlist(posix_shell: bool) -> &'static [&'static str] {
    if posix_shell {
        &SHELL_ALLOWLIST_POSIX
    } else {
        &SHELL_ALLOWLIST_WINDOWS
    }
}

#[must_use]
pub fn shell_denylist(posix_shell: bool) -> &'static [&'static str] {
    if posix_shell {
        &SHELL_DENYLIST_POSIX
    } else {
        &SHELL_DENYLIST_WINDOWS
    }
}

#[must_use]
pub fn shell_denylist_standalone(posix_shell: bool) -> &'static [&'static str] {
    if posix_shell {
        &SHELL_DENYLIST_STANDALONE_POSIX
    } else {
        &SHELL_DENYLIST_STANDALONE_WINDOWS
    }
}

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
fn integral_float(value: &Value) -> Option<i64> {
    let number = value.as_float()?;
    if number.fract() != 0.0 || !number.is_finite() {
        return None;
    }
    Some(number as i64)
}

// --------------------------------------------------------------------------
// A resolved configuration
// --------------------------------------------------------------------------

/// One tool's configuration, composed from its declared defaults, the
/// `tools.<name>` table and the session permission override.
///
/// A key the tool does not declare is carried rather than dropped, which is the
/// divergence [`crate::config::ConfigSnapshot::unregistered_keys`] already
/// records for the document as a whole: a setting written by a newer client
/// survives a round trip through this one.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedToolConfig {
    tool: String,
    values: BTreeMap<String, Value>,
    declaration: &'static ToolConfigDeclaration,
    posix_shell: bool,
}

impl ResolvedToolConfig {
    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// The value carried under `key`, declared or not.
    #[must_use]
    pub fn value(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// The keys carried under this tool that no declaration covers.
    #[must_use]
    pub fn undeclared_keys(&self) -> Vec<&str> {
        self.values
            .keys()
            .map(String::as_str)
            .filter(|key| self.declaration.default_for(key).is_none())
            .collect()
    }

    /// The document this configuration composes, in key order.
    #[must_use]
    pub fn into_table(self) -> Table {
        self.values.into_iter().collect()
    }

    #[must_use]
    pub fn permission(&self) -> PermissionMode {
        self.values
            .get("permission")
            .and_then(Value::as_str)
            .and_then(parse_permission)
            .unwrap_or(PermissionMode::Ask)
    }

    #[must_use]
    pub fn strings(&self, key: &str) -> Vec<String> {
        self.values
            .get(key)
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_else(|| self.declared_strings(key))
    }

    #[must_use]
    pub fn text(&self, key: &str) -> String {
        self.values
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_default()
    }

    #[must_use]
    pub fn flag(&self, key: &str) -> bool {
        self.values
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or_default()
    }

    /// A whole-number key, as the count or byte budget a caller compares
    /// against.
    #[must_use]
    pub fn count(&self, key: &str) -> usize {
        usize::try_from(self.integer(key)).unwrap_or(usize::MAX)
    }

    #[must_use]
    pub fn integer(&self, key: &str) -> i64 {
        self.values
            .get(key)
            .and_then(|value| value.as_integer().or_else(|| integral_float(value)))
            .unwrap_or_default()
    }

    /// A whole number of seconds, as the timeout a caller waits out.
    #[must_use]
    pub fn duration_seconds(&self, key: &str) -> u64 {
        u64::try_from(self.integer(key)).unwrap_or(0)
    }

    /// A fractional number of seconds, which the reference types as `float`.
    #[must_use]
    pub fn seconds(&self, key: &str) -> f64 {
        self.values
            .get(key)
            .and_then(|value| {
                value
                    .as_float()
                    .or_else(|| value.as_integer().map(|number| number as f64))
            })
            .unwrap_or_default()
    }

    fn declared_strings(&self, key: &str) -> Vec<String> {
        self.declaration
            .default_for(key)
            .map(|default| default.value(self.posix_shell))
            .as_ref()
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
}

// --------------------------------------------------------------------------
// Typed views
// --------------------------------------------------------------------------

/// A value a view carries, in the form the `tools.<name>` document holds it.
trait ConfigField {
    fn to_config_value(&self) -> Value;
}

impl ConfigField for bool {
    fn to_config_value(&self) -> Value {
        Value::Boolean(*self)
    }
}

impl ConfigField for usize {
    fn to_config_value(&self) -> Value {
        Value::Integer(i64::try_from(*self).unwrap_or(i64::MAX))
    }
}

impl ConfigField for u64 {
    fn to_config_value(&self) -> Value {
        Value::Integer(i64::try_from(*self).unwrap_or(i64::MAX))
    }
}

impl ConfigField for f64 {
    fn to_config_value(&self) -> Value {
        Value::Float(*self)
    }
}

impl ConfigField for String {
    fn to_config_value(&self) -> Value {
        Value::String(self.clone())
    }
}

impl ConfigField for Vec<String> {
    fn to_config_value(&self) -> Value {
        Value::Array(
            self.iter()
                .map(|entry| Value::String(entry.clone()))
                .collect(),
        )
    }
}

impl ConfigField for PermissionMode {
    fn to_config_value(&self) -> Value {
        Value::String(permission_label(*self).to_owned())
    }
}

/// The typed configuration one tool reads at every call.
///
/// A view carries exactly the keys its tool declares, which is what makes the
/// declaration table and the handlers stay in step: a key that loses its reader
/// loses its field, and the parity replay names it.
pub trait ToolConfigView: Sized {
    fn from_resolved(config: &ResolvedToolConfig) -> Self;
    /// The document this view carries, under the reference key names.
    fn document(&self) -> Table;
}

/// The four keys `BaseToolConfig` gives every tool.
#[derive(Debug, Clone, PartialEq)]
pub struct SharedToolConfig {
    pub permission: PermissionMode,
    pub allowlist: Vec<String>,
    pub denylist: Vec<String>,
    pub sensitive_patterns: Vec<String>,
}

impl ToolConfigView for SharedToolConfig {
    fn from_resolved(config: &ResolvedToolConfig) -> Self {
        Self {
            permission: config.permission(),
            allowlist: config.strings("allowlist"),
            denylist: config.strings("denylist"),
            sensitive_patterns: config.strings("sensitive_patterns"),
        }
    }

    fn document(&self) -> Table {
        Table::from_iter([
            ("permission".to_owned(), self.permission.to_config_value()),
            ("allowlist".to_owned(), self.allowlist.to_config_value()),
            ("denylist".to_owned(), self.denylist.to_config_value()),
            (
                "sensitive_patterns".to_owned(),
                self.sensitive_patterns.to_config_value(),
            ),
        ])
    }
}

impl Default for SharedToolConfig {
    fn default() -> Self {
        Self {
            permission: PermissionMode::Ask,
            allowlist: Vec::new(),
            denylist: Vec::new(),
            sensitive_patterns: Vec::new(),
        }
    }
}

/// Declares a view carrying the shared keys plus the tool's own.
macro_rules! tool_view {
    (
        $(#[$attribute:meta])*
        $name:ident { $($field:ident : $type:ty = $reader:ident),* $(,)? }
    ) => {
        $(#[$attribute])*
        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            pub shared: SharedToolConfig,
            $(pub $field: $type,)*
        }

        impl ToolConfigView for $name {
            fn from_resolved(config: &ResolvedToolConfig) -> Self {
                Self {
                    shared: SharedToolConfig::from_resolved(config),
                    $($field: config.$reader(stringify!($field)),)*
                }
            }

            fn document(&self) -> Table {
                let mut document = self.shared.document();
                $(document.insert(
                    stringify!($field).to_owned(),
                    self.$field.to_config_value(),
                );)*
                document
            }
        }
    };
}

tool_view!(
    /// Reference `ReadFileConfig`.
    ReadFileConfig {
        max_read_bytes: usize = count,
    }
);

tool_view!(
    /// Reference `WriteFileConfig`.
    WriteFileConfig {
        create_parent_dirs: bool = flag,
        max_write_bytes: usize = count,
    }
);

tool_view!(
    /// Reference `GrepConfig`.
    GrepConfig {
        codeignore_file: String = text,
        default_max_matches: usize = count,
        default_timeout: u64 = duration_seconds,
        exclude_patterns: Vec<String> = strings,
        max_output_bytes: usize = count,
    }
);

tool_view!(
    /// Reference `TodoConfig`.
    TodoConfig {
        max_todos: usize = count,
    }
);

tool_view!(
    /// Reference `WebFetchConfig`.
    WebFetchConfig {
        default_timeout: u64 = duration_seconds,
        max_content_bytes: usize = count,
        max_timeout: u64 = duration_seconds,
        user_agent: String = text,
    }
);

tool_view!(
    /// Reference `WebSearchConfig`.
    WebSearchConfig {
        model: String = text,
        timeout: u64 = duration_seconds,
    }
);

tool_view!(
    /// Reference `BashToolConfig`, published by all three shell families.
    ShellCommandConfig {
        default_timeout: u64 = duration_seconds,
        denylist_standalone: Vec<String> = strings,
        max_inline_bytes: usize = count,
        max_output_bytes: usize = count,
        max_timeout_seconds: f64 = seconds,
    }
);

tool_view!(
    /// Reference `BashOutputConfig`.
    ShellOutputConfig {
        max_inline_bytes: usize = count,
        max_poll_seconds: f64 = seconds,
    }
);

tool_view!(
    /// Reference `BashSessionsConfig` and `BashLogFileConfig`, which declare the
    /// same single limit.
    ShellInlineConfig {
        max_inline_bytes: usize = count,
    }
);

// --------------------------------------------------------------------------
// The resolver
// --------------------------------------------------------------------------

/// The session permission override a resolver consults, reference
/// `PermissionStore.get_tool_permission`.
pub type SessionPermissions = Arc<dyn Fn(&str) -> Option<PermissionMode> + Send + Sync>;

/// How long a resolution may wait on the settings lock before it falls back to
/// the declared defaults.
///
/// Nothing waits at all: the settings are read through `try_read`, so a turn
/// never blocks behind the refresh that a configuration write triggers. This
/// constant records the bound the non-functional requirement states, and the
/// test that measures it.
pub const MAX_RESOLUTION_BLOCK_MILLIS: u128 = 50;

/// Hands every tool the configuration it reads at each call.
///
/// One resolver serves the process: the settings it caches are shared by every
/// clone, so a configuration write observed once is observed by every session.
/// A session narrows it with [`Self::with_session_permissions`], which is the
/// only per-session part of the composition.
#[derive(Clone)]
pub struct ToolConfigResolver {
    settings: Arc<RwLock<Table>>,
    diagnostics: Arc<Mutex<Vec<String>>>,
    session_permissions: Option<SessionPermissions>,
    posix_shell: bool,
}

impl std::fmt::Debug for ToolConfigResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolConfigResolver")
            .field("posix_shell", &self.posix_shell)
            .field("session_permissions", &self.session_permissions.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for ToolConfigResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolConfigResolver {
    /// A resolver holding no operator settings: every tool resolves to its
    /// declared defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            settings: Arc::new(RwLock::new(Table::new())),
            diagnostics: Arc::new(Mutex::new(Vec::new())),
            session_permissions: None,
            posix_shell: !cfg!(windows),
        }
    }

    /// The same resolver reading the shell lists of the given branch.
    ///
    /// Reference `uses_posix_shell` answers for the interpreter a session
    /// drives rather than for the operating system, so a Windows host driving
    /// Git Bash reads the POSIX lists. The settings cache is shared with the
    /// resolver this was built from, so narrowing the branch does not drop what
    /// an operator configured.
    #[must_use]
    pub fn with_posix_shell(mut self, posix_shell: bool) -> Self {
        self.posix_shell = posix_shell;
        self
    }

    /// The same resolver answering with `permissions` for the session override.
    ///
    /// The settings cache is shared with the resolver this was built from, so a
    /// session narrowing the resolver still observes a configuration change.
    #[must_use]
    pub fn with_session_permissions(mut self, permissions: SessionPermissions) -> Self {
        self.session_permissions = Some(permissions);
        self
    }

    /// Replaces the cached `tools` table, refusing the values no key can carry.
    ///
    /// A refused value leaves a diagnostic and the declared default in place:
    /// the reference validates the merged document per tool, and a session that
    /// cannot start because one limit was mistyped is worse than one that runs
    /// on the shipped value and says so.
    pub fn update(&self, tools: Table) {
        let mut diagnostics = Vec::new();
        let mut accepted = Table::new();
        for (tool, entry) in tools {
            let Some(table) = entry.as_table() else {
                diagnostics.push(format!(
                    "tools.{tool} expects a table, found {}; ignoring it",
                    kind_of(&entry)
                ));
                continue;
            };
            let declaration = declaration(&tool);
            let mut kept = Table::new();
            for (key, value) in table {
                let Some(default) = declaration.and_then(|declared| declared.default_for(key))
                else {
                    // A key no declaration covers is carried rather than
                    // dropped, so a setting a newer client wrote survives.
                    kept.insert(key.clone(), value.clone());
                    continue;
                };
                if !default.accepts(value) {
                    diagnostics.push(format!(
                        "tools.{tool}.{key} expects {}, found {}; using {}",
                        default.kind(),
                        kind_of(value),
                        render(&default.value(self.posix_shell))
                    ));
                    continue;
                }
                if default.is_limit() && !is_positive(value) {
                    diagnostics.push(format!(
                        "tools.{tool}.{key} must be positive; using {}",
                        render(&default.value(self.posix_shell))
                    ));
                    continue;
                }
                kept.insert(key.clone(), value.clone());
            }
            accepted.insert(tool, Value::Table(kept));
        }
        *write_through_poison(&self.settings) = accepted;
        *lock_through_poison(&self.diagnostics) = diagnostics;
    }

    /// Reads the `tools` table out of `snapshot` and caches it.
    pub fn refresh(&self, snapshot: &ConfigSnapshot) {
        let tools = snapshot
            .effective
            .get(TOOL_SETTINGS_KEY)
            .and_then(Value::as_table)
            .cloned()
            .unwrap_or_default();
        self.update(tools);
    }

    /// Caches `config` now and keeps the cache current for as long as the store
    /// lives.
    ///
    /// Every reader of the configuration goes through a load, and a load is
    /// what refreshes this cache, so a `tools` change reaches the handlers
    /// without the tool surface being registered again: a budget raised between
    /// two turns is read by the second one, whether a client patched it or an
    /// operator edited the file by hand.
    pub fn follow(&self, config: &LayeredConfig) {
        let resolver = self.clone();
        config.observe(Arc::new(move |snapshot| resolver.refresh(snapshot)));
        if let Ok(snapshot) = config.load() {
            self.refresh(&snapshot);
        }
    }

    /// What the last [`Self::update`] refused, one line per refused value.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<String> {
        lock_through_poison(&self.diagnostics).clone()
    }

    /// The configuration `tool` reads: its declared defaults, the operator's
    /// `tools.<name>` table over them, and the session permission override over
    /// that.
    ///
    /// Reference `get_tool_config` returns the class defaults untouched when
    /// neither an override nor a session permission exists, which is the same
    /// document this composes for that case.
    #[must_use]
    pub fn resolve(&self, tool: &str) -> ResolvedToolConfig {
        let declaration = declaration(tool).unwrap_or(&BASE_DECLARATION);
        let mut values = declaration
            .keys
            .iter()
            .map(|key| (key.name.to_owned(), key.default.value(self.posix_shell)))
            .collect::<BTreeMap<_, _>>();
        // A turn never waits behind the refresh a configuration write triggers:
        // a contended cache resolves to the declared defaults rather than
        // blocking, and a poisoned one is read through the poison.
        match self.settings.try_read() {
            Ok(settings) => merge_entry(&mut values, &settings, tool),
            Err(TryLockError::Poisoned(poisoned)) => {
                merge_entry(&mut values, &poisoned.into_inner(), tool);
            }
            Err(TryLockError::WouldBlock) => {}
        }
        if let Some(permissions) = &self.session_permissions
            && let Some(mode) = permissions(tool)
        {
            values.insert(
                "permission".to_owned(),
                Value::String(permission_label(mode).to_owned()),
            );
        }
        ResolvedToolConfig {
            tool: tool.to_owned(),
            values,
            declaration,
            posix_shell: self.posix_shell,
        }
    }

    /// The typed configuration `tool` reads.
    #[must_use]
    pub fn view<V: ToolConfigView>(&self, tool: &str) -> V {
        V::from_resolved(&self.resolve(tool))
    }

    /// The document the tool's own view carries, which is the resolved
    /// configuration minus anything no handler reads.
    #[must_use]
    pub fn view_document(&self, tool: &str) -> Table {
        let resolved = self.resolve(tool);
        match tool {
            "read_file" => ReadFileConfig::from_resolved(&resolved).document(),
            "write_file" => WriteFileConfig::from_resolved(&resolved).document(),
            "grep" => GrepConfig::from_resolved(&resolved).document(),
            "todo" => TodoConfig::from_resolved(&resolved).document(),
            "web_fetch" => WebFetchConfig::from_resolved(&resolved).document(),
            "web_search" => WebSearchConfig::from_resolved(&resolved).document(),
            "bash" | "git_bash" | "powershell" => {
                ShellCommandConfig::from_resolved(&resolved).document()
            }
            "bash_output" | "git_bash_output" | "powershell_output" => {
                ShellOutputConfig::from_resolved(&resolved).document()
            }
            "bash_sessions"
            | "git_bash_sessions"
            | "powershell_sessions"
            | "bash_log_file"
            | "git_bash_log_file"
            | "powershell_log_file" => ShellInlineConfig::from_resolved(&resolved).document(),
            _ => SharedToolConfig::from_resolved(&resolved).document(),
        }
    }

    /// The document [`crate::config::ConfigLayerKind::Discovered`] composes:
    /// every declared tool's defaults, under the `tools` key that carries them.
    #[must_use]
    pub fn discovered_document(&self) -> Table {
        Table::from_iter([(
            TOOL_SETTINGS_KEY.to_owned(),
            Value::Table(self.published_defaults()),
        )])
    }

    /// The `tools` document every declared tool publishes, which is what the
    /// discovery layer contributes and what a settings screen renders.
    ///
    /// Reference `create_default_config` fills the same table from
    /// `discover_tool_defaults`, so a fresh configuration reads back the
    /// defaults rather than an empty table. The Windows-only families are
    /// declared here too: the reference enumerates declarations, not the
    /// surface a host publishes.
    #[must_use]
    pub fn published_defaults(&self) -> Table {
        let defaults = Self::new().with_posix_shell(self.posix_shell);
        DECLARATIONS
            .iter()
            .map(|declaration| {
                (
                    declaration.tool.to_owned(),
                    Value::Table(defaults.view_document(declaration.tool)),
                )
            })
            .collect()
    }
}

fn merge_entry(values: &mut BTreeMap<String, Value>, settings: &Table, tool: &str) {
    let Some(entry) = settings.get(tool).and_then(Value::as_table) else {
        return;
    };
    for (key, value) in entry {
        values.insert(key.clone(), value.clone());
    }
}

/// The write guard, whether or not a panicking writer poisoned the lock.
///
/// A poisoned settings lock still holds a table, and refusing to write through
/// it would freeze every later configuration change behind one panic.
fn write_through_poison<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_through_poison<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn is_positive(value: &Value) -> bool {
    value
        .as_integer()
        .map(|number| number > 0)
        .or_else(|| value.as_float().map(|number| number > 0.0))
        .unwrap_or(true)
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "a string",
        Value::Integer(_) => "an integer",
        Value::Float(_) => "a number",
        Value::Boolean(_) => "a boolean",
        Value::Datetime(_) => "a date",
        Value::Array(_) => "an array",
        Value::Table(_) => "a table",
    }
}

/// A refused value's replacement, rendered short enough for the single line the
/// accessibility requirement bounds a failure message to.
fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(entries) => format!("{} entries", entries.len()),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod config_tests;
