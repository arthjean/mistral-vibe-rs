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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, TryLockError};

use toml::{Table, Value};

use crate::config::{ConfigSnapshot, LayeredConfig};
use crate::policy::PermissionMode;
use crate::sync;

mod declarations;
mod lists;

#[cfg(test)]
pub(crate) use declarations::DOTENV_PATTERNS;
use declarations::integral_float;
pub use declarations::{
    BASE_DECLARATION, DECLARATIONS, ToolConfigDeclaration, ToolConfigDefault, ToolConfigKey,
    declaration, declared_document, parse_permission, permission_label,
};
pub use lists::{
    shell_allowlist, shell_denylist, shell_denylist_standalone, shell_read_only_commands,
};

/// The `tools` table an operator writes settings into, and the key every
/// resolved document is addressed under.
pub const TOOL_SETTINGS_KEY: &str = "tools";

/// The configuration field selecting the managed shell family, reference
/// `managed_shell_tools_enabled`.
const MANAGED_SHELL_KEY: &str = "managed_shell_tools_enabled";

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
    values: BTreeMap<String, Value>,
    declaration: &'static ToolConfigDeclaration,
    posix_shell: bool,
}

impl ResolvedToolConfig {
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

/// Reference `ExperimentalBashToolConfig`, published by all three shell
/// families.
///
/// `shell` is the one optional key: reference `discover_tool_defaults` dumps
/// the model with `exclude_none`, so the key publishes no default and the
/// document carries it only once an operator set it.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellCommandConfig {
    pub shared: SharedToolConfig,
    pub default_timeout: u64,
    pub denylist_standalone: Vec<String>,
    pub max_inline_bytes: usize,
    pub max_output_bytes: usize,
    pub max_timeout_seconds: f64,
    /// Reference `ExperimentalBashToolConfig.shell`, the executable a managed
    /// session starts when the call names none.
    pub shell: Option<String>,
}

impl ToolConfigView for ShellCommandConfig {
    fn from_resolved(config: &ResolvedToolConfig) -> Self {
        Self {
            shared: SharedToolConfig::from_resolved(config),
            default_timeout: config.duration_seconds("default_timeout"),
            denylist_standalone: config.strings("denylist_standalone"),
            max_inline_bytes: config.count("max_inline_bytes"),
            max_output_bytes: config.count("max_output_bytes"),
            max_timeout_seconds: config.seconds("max_timeout_seconds"),
            shell: config
                .value("shell")
                .and_then(Value::as_str)
                .filter(|shell| !shell.is_empty())
                .map(str::to_owned),
        }
    }

    fn document(&self) -> Table {
        let mut document = self.shared.document();
        document.insert(
            "default_timeout".to_owned(),
            self.default_timeout.to_config_value(),
        );
        document.insert(
            "denylist_standalone".to_owned(),
            self.denylist_standalone.to_config_value(),
        );
        document.insert(
            "max_inline_bytes".to_owned(),
            self.max_inline_bytes.to_config_value(),
        );
        document.insert(
            "max_output_bytes".to_owned(),
            self.max_output_bytes.to_config_value(),
        );
        document.insert(
            "max_timeout_seconds".to_owned(),
            self.max_timeout_seconds.to_config_value(),
        );
        if let Some(shell) = &self.shell {
            document.insert("shell".to_owned(), shell.to_config_value());
        }
        document
    }
}

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
    /// The shell rollout the merged configuration resolved, shared with every
    /// clone so a narrowed resolver still answers what the configuration says.
    managed_shell: Arc<AtomicBool>,
    /// The server the Mistral provider's `api_base` names, shared the same way.
    mistral_server_url: Arc<RwLock<Option<String>>>,
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
            managed_shell: Arc::new(AtomicBool::new(false)),
            mistral_server_url: Arc::new(RwLock::new(None)),
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
        *sync::write(&self.settings) = accepted;
        *sync::lock(&self.diagnostics) = diagnostics;
    }

    /// Reads the `tools` table and the shell rollout out of `snapshot` and
    /// caches both.
    pub fn refresh(&self, snapshot: &ConfigSnapshot) {
        let tools = snapshot
            .effective
            .get(TOOL_SETTINGS_KEY)
            .and_then(Value::as_table)
            .cloned()
            .unwrap_or_default();
        self.set_managed_shell_tools(
            snapshot
                .effective
                .get(MANAGED_SHELL_KEY)
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
        *sync::write(&self.mistral_server_url) =
            crate::telemetry::mistral_provider(&snapshot.effective).and_then(|provider| {
                provider
                    .get("api_base")
                    .and_then(Value::as_str)
                    .and_then(crate::llm::mistral::server_url)
            });
        self.update(tools);
    }

    /// The server a tool reaching the Mistral API directly addresses: the one
    /// the configured Mistral provider's `api_base` names, or `None` for the
    /// SDK's default. Reference `WebSearch._resolve_server_url`.
    #[must_use]
    pub fn mistral_server_url(&self) -> Option<String> {
        sync::read(&self.mistral_server_url).clone()
    }

    /// Whether the managed shell family is published in place of the one-shot
    /// command tool.
    ///
    /// Reference `_is_enabled_for_shell_rollout` reads
    /// `managed_shell_tools_enabled` off the session configuration at every
    /// availability check; here the value is cached by [`Self::refresh`] and
    /// read at every registration, which is the same freshness through the same
    /// bus every `tools.<name>` change already travels on.
    #[must_use]
    pub fn managed_shell_tools_enabled(&self) -> bool {
        self.managed_shell.load(Ordering::Relaxed)
    }

    /// Sets the rollout directly, for a caller that composes no configuration.
    ///
    /// The flag is shared by every clone, as the settings cache is: a family
    /// published against a narrowed resolver still reads the rollout the
    /// configuration resolved.
    pub fn set_managed_shell_tools(&self, enabled: bool) {
        self.managed_shell.store(enabled, Ordering::Relaxed);
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
        sync::lock(&self.diagnostics).clone()
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
    // Reference `AliasChoices("max_inline_bytes", "max_inline_chars")` reads
    // the alias only when the canonical name is absent, so an entry that sets
    // both keeps the canonical value.
    for (alias, canonical) in CONFIG_ALIASES {
        if !entry.contains_key(*canonical)
            && values.contains_key(*canonical)
            && let Some(value) = entry.get(*alias)
        {
            values.insert((*canonical).to_owned(), value.clone());
        }
    }
}

/// The `validation_alias` choices the shell configuration models accept, as
/// `(alias, canonical)`: `max_inline_chars` is read as `max_inline_bytes` by
/// every model declaring the latter.
const CONFIG_ALIASES: &[(&str, &str)] = &[("max_inline_chars", "max_inline_bytes")];

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
mod config_parity_tests;
#[cfg(test)]
mod config_tests;
