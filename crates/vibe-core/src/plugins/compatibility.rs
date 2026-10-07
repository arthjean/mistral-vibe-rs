//! Which format a plugin directory is written in, and the shapes a foreign
//! format is translated into.
//!
//! Reference `vibe/core/plugins/_compatibility.py`. A native package carries a
//! `plugin.json` naming the Agent Plugins 1.0 schema; otherwise the directory
//! is probed for the markers of the four foreign formats a compatibility
//! adapter reads, and more than one format is ambiguous.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use blake2::Blake2sVar;
use blake2::digest::{Update, VariableOutput};
use serde_json::Value;

use super::paths::{is_relative_to, resolve_strict};
use crate::hooks::HookConfig;
use crate::skills::SkillScope;

pub const NATIVE_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
const AGENT_PLUGIN_SCHEMA_PREFIX: &str = "https://agent-plugins.org/schemas/";
const MAX_SKILL_NAME_LENGTH: usize = 64;
const OPENCODE_CODE_SUFFIXES: [&str; 8] = ["cjs", "cts", "js", "jsx", "mjs", "mts", "ts", "tsx"];

/// The format a plugin directory was detected as. Reference
/// `DetectedPluginFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DetectedPluginFormat {
    AgentPlugins10,
    Codex,
    ClaudeCode,
    KimiCode,
    OpenCode,
    Unknown,
    Ambiguous,
}

impl DetectedPluginFormat {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentPlugins10 => "agent_plugins_1_0",
            Self::Codex => "codex",
            Self::ClaudeCode => "claude_code",
            Self::KimiCode => "kimi_code",
            Self::OpenCode => "opencode",
            Self::Unknown => "unknown",
            Self::Ambiguous => "ambiguous",
        }
    }

    /// The spelling read back. Reference `DetectedPluginFormat(value)`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::AgentPlugins10,
            Self::Codex,
            Self::ClaudeCode,
            Self::KimiCode,
            Self::OpenCode,
            Self::Unknown,
            Self::Ambiguous,
        ]
        .into_iter()
        .find(|format| format.as_str() == value)
    }
}

impl fmt::Display for DetectedPluginFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The names inside a package that are runtime state rather than content:
/// `.git` everywhere, and Claude Code's `.in_use` marker. Reference
/// `plugin_runtime_state_names`.
#[must_use]
pub fn plugin_runtime_state_names(source_format: DetectedPluginFormat) -> BTreeSet<String> {
    let mut names = BTreeSet::from([".git".to_owned()]);
    if source_format == DetectedPluginFormat::ClaudeCode {
        names.insert(".in_use".to_owned());
    }
    names
}

/// What detection found. Reference `PluginFormatDetection`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFormatDetection {
    pub source_format: DetectedPluginFormat,
    pub marker_paths: Vec<PathBuf>,
    pub unsupported_schema: Option<String>,
}

/// A rename or exposure a manifest declares for one source tool. Reference
/// `PluginToolOverride`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginToolOverride {
    pub name: Option<String>,
    pub exposure: Option<String>,
}

/// One MCP server a plugin declares, in the shape a session's MCP layer runs.
/// Reference `MCPStdio` and `MCPStreamableHttp` as a plugin builds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginMcpServer {
    Stdio {
        name: String,
        command: Vec<String>,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<String>,
    },
    /// `transport` is `streamable-http` or `http`.
    Http {
        name: String,
        transport: String,
        url: String,
        headers: BTreeMap<String, String>,
    },
    /// An `http` server that authenticates beyond its static headers, which
    /// only the Codex adapter declares. Reference `MCPHttp` with
    /// `MCPStaticAuth.api_key_env` or `MCPOAuth`.
    AuthenticatedHttp {
        name: String,
        url: String,
        headers: BTreeMap<String, String>,
        auth: PluginMcpHttpAuth,
    },
}

/// How an [`PluginMcpServer::AuthenticatedHttp`] server authenticates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginMcpHttpAuth {
    /// A token read from this environment variable when the server is
    /// reached, sent as `Authorization: Bearer <token>` unless the headers
    /// already name `Authorization`. Reference `MCPStaticAuth.http_headers`.
    BearerTokenEnv(String),
    /// OAuth with the authorization server's default scopes. Reference
    /// `MCPOAuth(type="oauth", scopes=[])`.
    OAuth,
}

impl PluginMcpServer {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Stdio { name, .. }
            | Self::Http { name, .. }
            | Self::AuthenticatedHttp { name, .. } => name,
        }
    }

    #[must_use]
    pub fn with_name(&self, name: &str) -> Self {
        let mut renamed = self.clone();
        match &mut renamed {
            Self::Stdio { name: current, .. }
            | Self::Http { name: current, .. }
            | Self::AuthenticatedHttp { name: current, .. } => {
                name.clone_into(current);
            }
        }
        renamed
    }

    #[must_use]
    pub fn transport(&self) -> &str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { transport, .. } => transport,
            Self::AuthenticatedHttp { .. } => "http",
        }
    }

    /// The command line, executable first. Reference `MCPStdio.argv`.
    #[must_use]
    pub fn argv(&self) -> Vec<String> {
        match self {
            Self::Stdio { command, args, .. } => command.iter().chain(args).cloned().collect(),
            Self::Http { .. } | Self::AuthenticatedHttp { .. } => Vec::new(),
        }
    }

    /// The headers an HTTP server is sent, a bearer token resolved from its
    /// environment variable when one is declared and set. Reference
    /// `MCPHttp.http_headers`.
    #[must_use]
    pub fn http_headers(&self) -> BTreeMap<String, String> {
        match self {
            Self::Stdio { .. } => BTreeMap::new(),
            Self::Http { headers, .. } => headers.clone(),
            Self::AuthenticatedHttp { headers, auth, .. } => match auth {
                PluginMcpHttpAuth::OAuth => BTreeMap::new(),
                PluginMcpHttpAuth::BearerTokenEnv(variable) => {
                    let mut resolved = headers.clone();
                    let explicit = resolved
                        .keys()
                        .any(|name| name.eq_ignore_ascii_case("authorization"));
                    if !explicit
                        && !variable.is_empty()
                        && let Some(token) = std::env::var(variable)
                            .ok()
                            .filter(|token| !token.is_empty())
                    {
                        resolved.insert("Authorization".to_owned(), format!("Bearer {token}"));
                    }
                    resolved
                }
            },
        }
    }

    /// Every value that may be a credential. Reference `mcp_server_secrets`.
    #[must_use]
    pub fn secrets(&self) -> Vec<String> {
        match self {
            Self::Stdio { env, .. } => env
                .values()
                .cloned()
                .chain(super::redaction::argv_values(&self.argv()))
                .collect(),
            Self::Http { url, .. } | Self::AuthenticatedHttp { url, .. } => {
                let query = crate::pyurl::PyUrl::split(url).query;
                self.http_headers()
                    .into_values()
                    .chain(query.split('&').map(ToOwned::to_owned))
                    .chain(std::iter::once(query.clone()))
                    .collect()
            }
        }
    }
}

/// An MCP server a foreign adapter translated. Reference `AdaptedMCPServer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptedMcpServer {
    pub source_id: String,
    pub server: PluginMcpServer,
    pub config_file: PathBuf,
}

/// A skill a foreign adapter translated or synthesized. Reference
/// `AdaptedSkill`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptedSkill {
    pub source_name: String,
    pub description: String,
    pub prompt: String,
    pub source_path: PathBuf,
    pub allowed_tools: Vec<String>,
    pub user_invocable: bool,
    pub model_invocable: bool,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    /// `Some("synthetic_skill")` for a skill rendered from another component.
    pub translation: Option<String>,
}

impl AdaptedSkill {
    #[must_use]
    pub fn new(
        source_name: String,
        description: String,
        prompt: String,
        source_path: PathBuf,
    ) -> Self {
        Self {
            source_name,
            description,
            prompt,
            source_path,
            allowed_tools: Vec::new(),
            user_invocable: true,
            model_invocable: true,
            license: None,
            compatibility: None,
            metadata: BTreeMap::new(),
            translation: None,
        }
    }
}

/// A hook a foreign adapter translated. Reference `AdaptedHook`.
#[derive(Debug, Clone, PartialEq)]
pub struct AdaptedHook {
    pub config: HookConfig,
    pub source_path: PathBuf,
    pub protocol: String,
}

/// A diagnostic a foreign adapter raised. Reference `PluginAdapterDiagnostic`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginAdapterDiagnostic {
    pub severity: super::diagnostics::Severity,
    pub code: String,
    pub path: PathBuf,
    pub message: String,
    pub fatal: bool,
    pub component: String,
}

/// A component a foreign adapter recognized and could not carry. Reference
/// `AdaptedUnsupportedComponent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptedUnsupportedComponent {
    pub kind: String,
    pub path: PathBuf,
    pub reason: String,
}

/// A foreign package in the native vocabulary. Reference
/// `AdaptedPluginPackage`.
#[derive(Debug, Clone, PartialEq)]
pub struct AdaptedPluginPackage {
    pub source_format: DetectedPluginFormat,
    pub manifest_path: PathBuf,
    pub name: String,
    pub version: Option<String>,
    pub description: String,
    pub namespace: String,
    pub data_root: PathBuf,
    pub scope: SkillScope,
    pub skill_roots: Vec<PathBuf>,
    pub mcp_servers: Vec<AdaptedMcpServer>,
    pub tool_overrides: BTreeMap<String, PluginToolOverride>,
    pub private_metadata: BTreeMap<String, Value>,
    pub author: Option<String>,
    pub adapted_skills: Vec<AdaptedSkill>,
    pub adapted_hooks: Vec<AdaptedHook>,
}

/// What a foreign adapter produced. Reference `PluginAdapterResult`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginAdapterResult {
    pub package: Option<AdaptedPluginPackage>,
    pub diagnostics: Vec<PluginAdapterDiagnostic>,
    pub unsupported_components: Vec<AdaptedUnsupportedComponent>,
}

/// Detects the format of the plugin directory at `root`. Reference
/// `detect_plugin_source_format`.
#[must_use]
pub fn detect_plugin_source_format(root: &Path) -> PluginFormatDetection {
    let native_manifest = root.join("plugin.json");
    let native_schema = read_schema_id(&native_manifest);
    if let Some(schema) = &native_schema {
        if schema == NATIVE_SCHEMA {
            return PluginFormatDetection {
                source_format: DetectedPluginFormat::AgentPlugins10,
                marker_paths: vec![native_manifest],
                unsupported_schema: None,
            };
        }
        if schema.starts_with(AGENT_PLUGIN_SCHEMA_PREFIX) {
            return PluginFormatDetection {
                source_format: DetectedPluginFormat::AgentPlugins10,
                marker_paths: vec![native_manifest],
                unsupported_schema: Some(schema.clone()),
            };
        }
    }
    let mut markers: Vec<(DetectedPluginFormat, PathBuf)> = [
        (
            DetectedPluginFormat::Codex,
            root.join(".codex-plugin").join("plugin.json"),
        ),
        (
            DetectedPluginFormat::ClaudeCode,
            root.join(".claude-plugin").join("plugin.json"),
        ),
        (
            DetectedPluginFormat::KimiCode,
            root.join("kimi.plugin.json"),
        ),
        (
            DetectedPluginFormat::KimiCode,
            root.join(".kimi-plugin").join("plugin.json"),
        ),
    ]
    .into_iter()
    .filter(|(_, marker)| marker.is_file())
    .collect();
    markers.extend(
        opencode_markers(root)
            .into_iter()
            .map(|marker| (DetectedPluginFormat::OpenCode, marker)),
    );
    let formats: BTreeSet<DetectedPluginFormat> =
        markers.iter().map(|(format, _)| *format).collect();
    let marker_paths: Vec<PathBuf> = markers.iter().map(|(_, marker)| marker.clone()).collect();
    if formats.len() > 1 {
        return PluginFormatDetection {
            source_format: DetectedPluginFormat::Ambiguous,
            marker_paths,
            unsupported_schema: None,
        };
    }
    if let Some((format, _)) = markers.first() {
        return PluginFormatDetection {
            source_format: *format,
            marker_paths,
            unsupported_schema: None,
        };
    }
    PluginFormatDetection {
        source_format: DetectedPluginFormat::Unknown,
        marker_paths: if native_manifest.exists() {
            vec![native_manifest]
        } else {
            Vec::new()
        },
        unsupported_schema: None,
    }
}

/// An author's display name from a string or an object's `name`. Reference
/// `author_display_name`.
#[must_use]
pub fn author_display_name(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => {
            let trimmed = python_strip(text);
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        Value::Object(map) => author_display_name(map.get("name")),
        _ => None,
    }
}

/// Python's `str.strip()` with no argument, which strips Unicode whitespace.
#[must_use]
pub fn python_strip(text: &str) -> &str {
    text.trim_matches(|character: char| character.is_whitespace() || is_python_space(character))
}

fn is_python_space(character: char) -> bool {
    matches!(character, '\u{1c}'..='\u{1f}' | '\u{85}')
}

/// Every character outside ASCII letters, digits, `_` and `$` becomes `_`,
/// and a leading digit is prefixed with `_`. Reference `typescript_identifier`.
#[must_use]
pub fn typescript_identifier(value: &str) -> String {
    let normalized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '$' {
                character
            } else {
                '_'
            }
        })
        .collect();
    match normalized.chars().next() {
        None => "_".to_owned(),
        Some(first) if first.is_ascii_alphabetic() || first == '_' || first == '$' => normalized,
        Some(_) => format!("_{normalized}"),
    }
}

/// A skill name made of lowercase ASCII words joined by hyphens, at most 64
/// characters, a longer one shortened behind a BLAKE2s suffix. Reference
/// `portable_skill_name`.
///
/// # Errors
///
/// A value with no ASCII letter or digit and no prefix.
pub fn portable_skill_name(value: &str, prefix: &str) -> Result<String, String> {
    let lowered = value.to_lowercase();
    let mut collapsed = String::new();
    let mut in_run = false;
    for character in lowered.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            collapsed.push(character);
            in_run = false;
        } else if !in_run {
            collapsed.push('-');
            in_run = true;
        }
    }
    let normalized = collapsed.trim_matches('-');
    let normalized = if normalized.is_empty() {
        prefix.trim_end_matches('-').to_owned()
    } else {
        format!("{prefix}{normalized}")
    };
    if normalized.is_empty() {
        return Err("skill name must contain an ASCII letter or digit".to_owned());
    }
    if normalized.chars().count() <= MAX_SKILL_NAME_LENGTH {
        return Ok(normalized);
    }
    let digest = blake2s_hex(normalized.as_bytes(), 5);
    let prefix_length = MAX_SKILL_NAME_LENGTH - digest.len() - 1;
    let head: String = normalized.chars().take(prefix_length).collect();
    Ok(format!("{}-{digest}", head.trim_end_matches('-')))
}

/// BLAKE2s with a digest of `size` bytes, in hex. Reference
/// `hashlib.blake2s(..., digest_size=size)`.
#[must_use]
pub fn blake2s_hex(payload: &[u8], size: usize) -> String {
    let Ok(mut hasher) = Blake2sVar::new(size) else {
        return String::new();
    };
    hasher.update(payload);
    let mut output = vec![0_u8; size];
    if hasher.finalize_variable(&mut output).is_err() {
        return String::new();
    }
    hex::encode(output)
}

/// A path a manifest declares, resolved inside the plugin root. Reference
/// `resolve_declared_path`.
///
/// # Errors
///
/// An empty, backslashed, drive-lettered, absolute or escaping value, or one
/// that does not exist.
pub fn resolve_declared_path(root: &Path, value: &str) -> Result<PathBuf, String> {
    if value.is_empty() {
        return Err("declared path cannot be empty".to_owned());
    }
    let bytes = value.as_bytes();
    if value.contains('\\')
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'/')
    {
        return Err("declared path must use portable forward slashes".to_owned());
    }
    let relative = value.strip_prefix("./").unwrap_or(value);
    let pure = Path::new(relative);
    if pure.is_absolute() || pure.components().any(|part| part == Component::ParentDir) {
        return Err("declared path must stay inside the plugin root".to_owned());
    }
    let candidate = relative
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .fold(root.to_path_buf(), |path, part| path.join(part));
    let resolved = resolve_strict(&candidate).map_err(|error| error.to_string())?;
    if !is_relative_to(&resolved, root) {
        return Err("declared path must resolve inside the plugin root".to_owned());
    }
    Ok(resolved)
}

/// A path inside a plugin spelled relative to its root, `.` for the root and
/// `<outside-plugin>` for anything elsewhere. Reference `relative_plugin_path`.
#[must_use]
pub fn relative_plugin_path(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) => {
            let value = super::content::posix_relative(relative);
            if value.is_empty() {
                ".".to_owned()
            } else {
                value
            }
        }
        Err(_) => "<outside-plugin>".to_owned(),
    }
}

/// The text of a file, decoded the way reference `read_safe` decodes it.
///
/// # Errors
///
/// The file cannot be read.
pub fn read_text(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(crate::workspace::text_file::decode(&bytes).text)
}

fn read_schema_id(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    let text = read_text(path).ok()?;
    match serde_json::from_str::<Value>(&text).ok()? {
        Value::Object(map) => match map.get("$schema") {
            Some(Value::String(schema)) => Some(schema.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn opencode_markers(root: &Path) -> Vec<PathBuf> {
    let mut markers: Vec<PathBuf> = Vec::new();
    for directory in [
        root.join(".opencode").join("plugins"),
        root.join(".opencode").join("tools"),
    ] {
        if !directory.is_dir() {
            continue;
        }
        let mut found = Vec::new();
        walk_files(&directory, &mut found);
        found.sort();
        markers.extend(found.into_iter().filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        OPENCODE_CODE_SUFFIXES.contains(&extension.to_lowercase().as_str())
                    })
        }));
    }
    let package_path = root.join("package.json");
    if let Some(marker) = opencode_package_marker(root, &package_path)
        && !markers.contains(&marker)
    {
        markers.push(marker);
    }
    for path in [
        root.join("opencode.json"),
        root.join("opencode.jsonc"),
        root.join(".opencode").join("opencode.json"),
        root.join(".opencode").join("opencode.jsonc"),
    ] {
        if path.is_file() && !markers.contains(&path) {
            markers.push(path);
        }
    }
    let skill_dir = root.join(".opencode").join("skills");
    if skill_dir.is_dir() {
        let mut skills: Vec<PathBuf> = std::fs::read_dir(&skill_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path().join("SKILL.md"))
            .filter(|path| path.is_file())
            .collect();
        skills.sort();
        for path in skills {
            if !markers.contains(&path) {
                markers.push(path);
            }
        }
    }
    markers
}

/// Every path below `directory` as `Path.rglob("*")` lists it: without
/// descending into a symbolic link to a directory.
pub fn walk_files(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir());
        found.push(path.clone());
        if is_dir {
            walk_files(&path, found);
        }
    }
}

fn opencode_package_marker(root: &Path, path: &Path) -> Option<PathBuf> {
    if !path.is_file() {
        return None;
    }
    let resolved = resolve_strict(path).ok()?;
    if !is_relative_to(&resolved, root) {
        return None;
    }
    let value: Value = serde_json::from_str(&read_text(&resolved).ok()?).ok()?;
    let Value::Object(map) = value else {
        return None;
    };
    let is_opencode = match map.get("keywords") {
        Some(Value::Array(keywords)) => keywords
            .iter()
            .any(|keyword| matches!(keyword.as_str(), Some("opencode" | "opencode-plugin"))),
        _ => false,
    };
    if !is_opencode {
        return None;
    }
    if let Some(Value::String(module)) = map.get("module")
        && let Ok(module_path) = resolve_declared_path(root, module)
        && module_path.is_file()
    {
        return Some(module_path);
    }
    Some(path.to_path_buf())
}
