//! The helpers the foreign adapters share.
//!
//! Reference `vibe/core/plugins/_foreign.py`, which also holds the OpenCode
//! adapter (`opencode` here). Skills, static commands, MCP server
//! declarations and command hooks read the same way whichever format declares
//! them, so the Claude Code, Kimi Code and OpenCode adapters build on the
//! functions here, and the Codex adapter on the few its own module does not
//! redefine.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::compatibility::{
    AdaptedMcpServer, AdaptedSkill, AdaptedUnsupportedComponent, DetectedPluginFormat,
    PluginAdapterDiagnostic, PluginMcpServer, openai_policy, portable_skill_name, read_text,
    resolve_declared_path, walk_files,
};
use super::diagnostics::Severity;
use super::paths::{is_relative_to, path_text, resolve_strict};
use super::strict::ValidationErrors;
use crate::hooks::HookType;
use crate::platform::resolve_lenient;
use crate::pycompat::{self, Dumps, truthy};
use crate::skills::parser::parse_skill_markdown;
use crate::skills::schema::SkillMetadata;

const DISABLE_MODEL_INVOCATION_FIELD: &str = "disable-model-invocation";
/// The three spellings Kimi accepts for the explicit-only invocation policy.
const KIMI_MODEL_INVOCATION_FIELDS: [&str; 3] = [
    "disableModelInvocation",
    DISABLE_MODEL_INVOCATION_FIELD,
    "disable_model_invocation",
];
const MAX_DESCRIPTION_CHARS: usize = 1024;

#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static DYNAMIC_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)(?:^|\s)!`[^`]+`").expect("dynamic command pattern compiles")
});
#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static ARGUMENT_REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$(?:ARGUMENTS(?:\[\d+\])?|\d+)\b").expect("argument pattern compiles")
});

/// A diagnostic coded `plugin.compatibility.<format>.<suffix>` that keeps the
/// plugin; [`PluginAdapterDiagnostic::fatal`] marks one that drops it.
/// Reference `_diagnostic`.
pub(super) fn diagnostic(
    source_format: DetectedPluginFormat,
    suffix: &str,
    path: impl Into<PathBuf>,
    message: impl Into<String>,
    component: &str,
    severity: Severity,
) -> PluginAdapterDiagnostic {
    PluginAdapterDiagnostic {
        severity,
        code: format!("plugin.compatibility.{source_format}.{suffix}"),
        path: path.into(),
        message: message.into(),
        fatal: false,
        component: component.to_owned(),
    }
}

/// An unsupported component record.
pub(super) fn unsupported_component(
    kind: &str,
    path: impl Into<PathBuf>,
    reason: &str,
) -> AdaptedUnsupportedComponent {
    AdaptedUnsupportedComponent {
        kind: kind.to_owned(),
        path: path.into(),
        reason: reason.to_owned(),
    }
}

/// A key of a model declared with an alias and `populate_by_name`: the alias
/// wins, and the field name is read only when the alias is absent.
pub(super) fn aliased<'a>(
    map: &'a Map<String, Value>,
    alias: &str,
    name: &str,
) -> Option<&'a Value> {
    map.get(alias).or_else(|| map.get(name))
}

/// An optional string field read under its alias or its name: `Some(None)`
/// for an absent or `null` value, `None` with an error for anything but a
/// string.
pub(super) fn optional_aliased_string(
    map: &Map<String, Value>,
    alias: &str,
    name: &str,
    errors: &mut ValidationErrors,
) -> Option<Option<String>> {
    match aliased(map, alias, name) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(text)) => Some(Some(text.clone())),
        Some(_) => {
            errors.push(alias, "must be a string or null");
            None
        }
    }
}

/// A manifest as a model that declares aliased fields under
/// `populate_by_name` and keeps undeclared keys reads it. Reference
/// `_ClaudeManifest` and `_KimiManifest`.
pub(super) struct AliasedObject {
    pub(super) raw: Map<String, Value>,
    /// Each declared field: the key a document spells, and the field name
    /// `populate_by_name` also accepts when it differs.
    fields: &'static [(&'static str, Option<&'static str>)],
}

impl AliasedObject {
    pub(super) const fn new(
        raw: Map<String, Value>,
        fields: &'static [(&'static str, Option<&'static str>)],
    ) -> Self {
        Self { raw, fields }
    }

    /// A declared field, `None` for an absent or `null` value.
    pub(super) fn field(&self, alias: &str) -> Option<&Value> {
        let name = self
            .fields
            .iter()
            .find(|(key, _)| *key == alias)
            .and_then(|(_, name)| *name)
            .unwrap_or(alias);
        aliased(&self.raw, alias, name).filter(|value| !value.is_null())
    }

    /// Keys that populate no declared field, sorted. Reference `model_extra`
    /// minus the declared spellings: a field name whose alias is also present
    /// stays extra, as pydantic leaves it.
    pub(super) fn extras(&self) -> Vec<String> {
        let mut extras: Vec<String> = self
            .raw
            .keys()
            .filter(|key| {
                !self.fields.iter().any(|(alias, name)| {
                    key.as_str() == *alias
                        || (Some(key.as_str()) == *name && !self.raw.contains_key(*alias))
                })
            })
            .cloned()
            .collect();
        extras.sort();
        extras
    }
}

/// Whether Python's `re` would compile `pattern`, checked with the `regex`
/// crate after rewriting the constructs `re` has and it lacks: look-around
/// and atomic groups become plain groups, backreferences a literal, and `\Z`
/// an end anchor. Only whether the pattern compiles is decided here; the
/// hook layer matches it.
pub(super) fn pattern_compiles(pattern: &str) -> bool {
    let characters: Vec<char> = pattern.chars().collect();
    let mut rewritten = String::with_capacity(pattern.len());
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if character == '\\' {
            match characters.get(index + 1) {
                Some('Z') => rewritten.push('$'),
                Some(digit) if digit.is_ascii_digit() && *digit != '0' => rewritten.push('x'),
                Some(next) => {
                    rewritten.push('\\');
                    rewritten.push(*next);
                }
                None => rewritten.push('\\'),
            }
            index += 2;
            continue;
        }
        if character == '(' && characters.get(index + 1) == Some(&'?') {
            let rest: String = characters[index + 2..].iter().take(3).collect();
            let prefix = ["<=", "<!", "=", "!", ">"]
                .into_iter()
                .find(|prefix| rest.starts_with(prefix));
            if let Some(prefix) = prefix {
                rewritten.push_str("(?:");
                index += 2 + prefix.len();
                continue;
            }
            if rest.starts_with("P=") {
                let close = characters[index..].iter().position(|next| *next == ')');
                rewritten.push('x');
                index += close.map_or(characters.len() - index, |close| close + 1);
                continue;
            }
        }
        rewritten.push(character);
        index += 1;
    }
    Regex::new(&rewritten).is_ok()
}

/// The runtime lifecycle a Claude Code or Kimi Code hook event maps onto.
/// Reference `_claude_hook_type` and `_kimi_hook_type`.
pub(super) fn hook_type(event: &str) -> Option<HookType> {
    match event {
        "PreToolUse" => Some(HookType::PreTool),
        "PostToolUse" => Some(HookType::PostTool),
        "Stop" => Some(HookType::PostAgent),
        _ => None,
    }
}

/// The tool name pattern a vendor hook matcher becomes: `*` for none, empty
/// or `*`, the matcher itself when it compiles, and `None` otherwise.
/// Reference `_claude_matcher` and `_kimi_matcher`.
pub(super) fn hook_matcher(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => Some("*".to_owned()),
        Some(Value::String(text)) if text.is_empty() || text == "*" => Some("*".to_owned()),
        Some(Value::String(text)) => pattern_compiles(text).then(|| text.clone()),
        Some(_) => None,
    }
}

/// Reports a vendor hook that has no runtime equivalent. Reference
/// `_unsupported_claude_hook` and `_unsupported_kimi_hook`.
pub(super) fn unsupported_hook(
    source_format: DetectedPluginFormat,
    path: &Path,
    message: impl Into<String>,
    reason: &str,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) {
    diagnostics.push(diagnostic(
        source_format,
        "hooks_partially_unsupported",
        path,
        message,
        "hook",
        Severity::Warning,
    ));
    unsupported.push(unsupported_component("hooks", path, reason));
}

/// The paths a declaration names: none for an absent value, one for a string,
/// every entry of a list of strings, and `None` for anything else. Reference
/// `declared_paths`.
pub(super) fn declared_paths(value: Option<&Value>) -> Option<Vec<String>> {
    match value {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::String(text)) => Some(vec![text.clone()]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(ToOwned::to_owned))
            .collect(),
        Some(_) => None,
    }
}

/// The Markdown files a declared path names: the file itself, or every `.md`
/// file below the directory. Reference `markdown_files_for_declared_path`.
pub(super) fn markdown_files_for_declared_path(
    root: &Path,
    value: &str,
) -> Result<Vec<PathBuf>, String> {
    let resolved = resolve_declared_path(root, value)?;
    let is_markdown = resolved
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"));
    if resolved.is_file() && is_markdown {
        return Ok(vec![resolved]);
    }
    if !resolved.is_dir() {
        return Err(
            "the declared path has to resolve to a Markdown file or a directory".to_owned(),
        );
    }
    let mut found = Vec::new();
    walk_files(&resolved, &mut found);
    let mut files: Vec<PathBuf> = found
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".md"))
                && path.is_file()
        })
        .collect();
    files.sort();
    Ok(files)
}

/// The `SKILL.md` files a declared path names: the file, the directory's own
/// `SKILL.md`, or each child directory's. Reference
/// `skill_files_for_declared_path`.
pub(super) fn skill_files_for_declared_path(
    root: &Path,
    value: &str,
) -> Result<Vec<PathBuf>, String> {
    let resolved = resolve_declared_path(root, value)?;
    if resolved.is_file() && resolved.file_name().is_some_and(|name| name == "SKILL.md") {
        return Ok(vec![resolved]);
    }
    let direct = resolved.join("SKILL.md");
    if direct.is_file() {
        return Ok(vec![
            resolve_strict(&direct).map_err(|error| error.to_string())?,
        ]);
    }
    if !resolved.is_dir() {
        return Err("the skills path has to resolve to a skill directory".to_owned());
    }
    let mut files = Vec::new();
    for path in child_skill_files(&resolved) {
        files.push(resolve_strict(&path).map_err(|error| error.to_string())?);
    }
    files.sort();
    Ok(files)
}

/// `directory.glob("*/SKILL.md")` filtered to files.
pub(super) fn child_skill_files(directory: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("SKILL.md"))
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
}

/// Reads one portable Agent Skill. Reference `adapt_skill_file`.
pub(super) fn adapt_skill_file(
    path: &Path,
    root: &Path,
    source_format: DetectedPluginFormat,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    normalize_vendor_fields: bool,
) -> Option<AdaptedSkill> {
    let parsed = read_text(path)
        .map_err(|error| error.to_string())
        .and_then(|text| parse_skill_markdown(&text).map_err(|error| error.to_string()))
        .and_then(|(frontmatter, body)| {
            let mut normalized = frontmatter;
            if normalize_vendor_fields {
                normalized = normalize_vendor_skill_frontmatter(normalized, path, source_format, diagnostics)?;
            } else if normalized.contains_key("tools") && !normalized.contains_key("allowed-tools") {
                let tools = string_list(normalized.get("tools"))?;
                normalized.insert(
                    "allowed-tools".to_owned(),
                    Value::Array(tools.into_iter().map(Value::String).collect()),
                );
                diagnostics.push(diagnostic(source_format, "skill_tools_normalized", path, "The skill's vendor tool list became its allowed-tools metadata; runtime approvals still decide.", "skill", Severity::Info));
            }
            let metadata = SkillMetadata::validate(&normalized).map_err(|error| error.to_string())?;
            Ok((metadata, body))
        });
    let (metadata, body) = match parsed {
        Ok(parsed) => parsed,
        Err(error) => {
            diagnostics.push(diagnostic(
                source_format,
                "skill_invalid",
                path,
                format!("Could not import the skill: {error}"),
                "skill",
                Severity::Error,
            ));
            return None;
        }
    };
    let openai_allows = match openai_policy(path, root) {
        Ok(allowed) => allowed,
        Err(error) => {
            diagnostics.push(diagnostic(source_format, "skill_openai_metadata_invalid", crate::skills::openai_metadata_path(path).unwrap_or_default(), format!("The OpenAI skill metadata is invalid, so the model cannot invoke the skill: {error}"), "skill", Severity::Warning));
            false
        }
    };
    let prompt = with_tool_guidance(pycompat::strip(&body), &metadata.allowed_tools);
    Some(AdaptedSkill {
        source_name: metadata.name,
        description: metadata.description,
        prompt,
        source_path: path.to_path_buf(),
        allowed_tools: metadata.allowed_tools,
        user_invocable: metadata.user_invocable,
        model_invocable: !metadata.disable_model_invocation && openai_allows,
        license: metadata.license,
        compatibility: metadata.compatibility,
        metadata: metadata.metadata,
        translation: None,
    })
}

/// Turns a static Markdown command into a synthesized skill, or reports why
/// it cannot be one. Reference `adapt_static_command`.
pub(super) fn adapt_static_command(
    path: &Path,
    source_name: &str,
    source_format: DetectedPluginFormat,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Option<AdaptedSkill> {
    let parsed = read_text(path)
        .map_err(|error| error.to_string())
        .and_then(|text| parse_skill_markdown(&text).map_err(|error| error.to_string()));
    let (frontmatter, body) = match parsed {
        Ok(parsed) => parsed,
        Err(error) => {
            diagnostics.push(command_invalid(source_format, path, &error));
            return None;
        }
    };
    let body = pycompat::strip(&body).to_owned();
    if body.is_empty() {
        diagnostics.push(diagnostic(
            source_format,
            "command_empty",
            path,
            "The static command has no prompt body, so it was not converted.",
            "command",
            Severity::Warning,
        ));
        return None;
    }
    if DYNAMIC_COMMAND.is_match(&body) {
        diagnostics.push(diagnostic(source_format, "command_dynamic_execution_unsupported", path, "The static command injects shell output into its prompt, so it was neither run nor converted.", "command", Severity::Warning));
        return None;
    }
    if has_argument_reference(&body) {
        diagnostics.push(diagnostic(source_format, "command_arguments_unsupported", path, "The static command substitutes arguments, which a skill invocation cannot supply, so it was not converted.", "command", Severity::Warning));
        return None;
    }
    let command_name = match frontmatter.get("name") {
        Some(Value::String(name)) => pycompat::strip(name).to_owned(),
        _ => source_name.to_owned(),
    };
    let generated_name = match portable_skill_name(&command_name, "command-") {
        Ok(name) => name,
        Err(error) => {
            diagnostics.push(command_invalid(source_format, path, &error));
            return None;
        }
    };
    let description = match frontmatter.get("description") {
        Some(Value::String(text)) if !pycompat::strip(text).is_empty() => {
            pycompat::strip(text).to_owned()
        }
        _ => description_from_body(&body),
    };
    let declared_tools = frontmatter.get("allowed-tools").or_else(|| {
        (!frontmatter.contains_key("allowed-tools"))
            .then(|| frontmatter.get("tools"))
            .flatten()
    });
    // The reference lets a malformed tool list escape the adapter; the port
    // reports it against the command instead.
    let allowed_tools = match string_list(declared_tools) {
        Ok(tools) => tools,
        Err(error) => {
            diagnostics.push(command_invalid(source_format, path, &error));
            return None;
        }
    };
    let mut unsupported: Vec<&str> = frontmatter
        .keys()
        .map(String::as_str)
        .filter(|key| {
            ![
                "name",
                "description",
                "allowed-tools",
                "tools",
                "argument-hint",
            ]
            .contains(key)
        })
        .collect();
    unsupported.sort_unstable();
    if !unsupported.is_empty() {
        diagnostics.push(diagnostic(
            source_format,
            "command_metadata_partially_supported",
            path,
            format!(
                "These static command metadata fields are kept as source data and not applied: {}",
                unsupported.join(", ")
            ),
            "command",
            Severity::Info,
        ));
    }
    if frontmatter.contains_key("argument-hint") {
        diagnostics.push(diagnostic(
            source_format,
            "command_argument_hint_ui_unsupported",
            path,
            "The command's argument hint is interface metadata this client does not show.",
            "command",
            Severity::Info,
        ));
    }
    if !allowed_tools.is_empty() {
        diagnostics.push(diagnostic(source_format, "command_tool_policy_preserved_as_guidance", path, "The command's tool allowlist is kept in the generated skill and does not bypass runtime approvals.", "command", Severity::Info));
    }
    let prompt = with_tool_guidance(&body, &allowed_tools);
    let mut skill = AdaptedSkill::new(
        generated_name,
        description.chars().take(MAX_DESCRIPTION_CHARS).collect(),
        prompt,
        path.to_path_buf(),
    );
    skill.allowed_tools = allowed_tools;
    skill.metadata = BTreeMap::from([("source-command".to_owned(), command_name)]);
    skill.translation = Some("synthetic_skill".to_owned());
    Some(skill)
}

/// The static commands one declared path names, each turned into a
/// synthesized skill: the Markdown file itself, or every `.md` file below the
/// directory, named by its path relative to that directory. Reference the
/// `_commands` of `ClaudePluginAdapter` and `KimiPluginAdapter`.
pub(super) fn adapt_command_path(
    root: &Path,
    value: &str,
    source_format: DetectedPluginFormat,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Result<Vec<AdaptedSkill>, String> {
    let files = markdown_files_for_declared_path(root, value)?;
    let base = resolve_declared_path(root, value)?;
    let base = if base.is_dir() {
        base
    } else {
        base.parent().map_or_else(PathBuf::new, Path::to_path_buf)
    };
    Ok(files
        .iter()
        .filter_map(|path| {
            let source_name = command_source_name(path, &base);
            adapt_static_command(path, &source_name, source_format, diagnostics)
        })
        .collect())
}

fn command_invalid(
    source_format: DetectedPluginFormat,
    path: &Path,
    error: &str,
) -> PluginAdapterDiagnostic {
    diagnostic(
        source_format,
        "command_invalid",
        path,
        format!("Could not import the static command: {error}"),
        "command",
        Severity::Error,
    )
}

/// `$ARGUMENTS`, `$ARGUMENTS[n]` or `$n` not escaped by a backslash.
fn has_argument_reference(body: &str) -> bool {
    ARGUMENT_REFERENCE
        .find_iter(body)
        .any(|found| !body[..found.start()].ends_with('\\'))
}

/// Translates a mapping of MCP server declarations. Reference
/// `adapt_mcp_servers`.
pub(super) fn adapt_mcp_servers(
    root: &Path,
    data_root: &Path,
    raw_servers: &Map<String, Value>,
    config_path: &Path,
    source_format: DetectedPluginFormat,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    placeholders: &[(&str, String)],
) -> Vec<AdaptedMcpServer> {
    let ordered: BTreeMap<&String, &Value> = raw_servers.iter().collect();
    let mut servers = Vec::new();
    for (source_id, raw) in ordered {
        match adapt_mcp_server(root, data_root, source_id, raw, placeholders) {
            Ok(server) => servers.push(AdaptedMcpServer {
                source_id: source_id.clone(),
                server,
                config_file: config_path.to_path_buf(),
            }),
            Err(error) => diagnostics.push(diagnostic(
                source_format,
                "mcp_server_invalid",
                config_path,
                format!(
                    "Could not import the MCP server {}: {error}",
                    pycompat::repr_str(source_id)
                ),
                "mcp_server",
                Severity::Error,
            )),
        }
    }
    servers
}

/// The environment a translated stdio server runs with: the declared one,
/// which cannot name the two reserved variables, plus `PLUGIN_ROOT` and
/// `PLUGIN_DATA`. Reference `_adapt_mcp_server` and the Codex adapter's
/// `_codex_mcp_server`.
pub(super) fn plugin_stdio_env(
    mut env: BTreeMap<String, String>,
    root: &Path,
    data_root: &Path,
) -> Result<BTreeMap<String, String>, String> {
    let reserved: Vec<&str> = ["PLUGIN_DATA", "PLUGIN_ROOT"]
        .into_iter()
        .filter(|name| env.contains_key(*name))
        .collect();
    if !reserved.is_empty() {
        return Err(format!(
            "env cannot define the reserved variables {}",
            reserved.join(", ")
        ));
    }
    env.insert("PLUGIN_ROOT".to_owned(), path_text(root));
    env.insert("PLUGIN_DATA".to_owned(), path_text(data_root));
    Ok(env)
}

/// The server mapping a value carries, unwrapping an `mcpServers` object.
/// Reference `mcp_mapping`.
pub(super) fn mcp_mapping(value: Option<&Value>) -> Option<Map<String, Value>> {
    let Some(Value::Object(map)) = value else {
        return None;
    };
    if let Some(Value::Object(wrapped)) = map.get("mcpServers") {
        return Some(wrapped.clone());
    }
    Some(map.clone())
}

fn adapt_mcp_server(
    root: &Path,
    data_root: &Path,
    source_id: &str,
    raw: &Value,
    placeholders: &[(&str, String)],
) -> Result<PluginMcpServer, String> {
    let Value::Object(raw) = raw else {
        return Err("an MCP server declaration has to be an object".to_owned());
    };
    if let Some(command_value) = raw.get("command").filter(|value| truthy(value)) {
        let Value::String(command_value) = command_value else {
            return Err("a stdio command has to be a string".to_owned());
        };
        let expanded_command = expand(command_value, placeholders);
        let command = resolve_command(root, command_value, &expanded_command)?;
        let args = string_list(raw.get("args"))?;
        let env = plugin_stdio_env(
            string_mapping(raw.get("env"))?
                .into_iter()
                .map(|(name, value)| (name, expand(&value, placeholders)))
                .collect(),
            root,
            data_root,
        )?;
        let cwd = match raw.get("cwd") {
            Some(Value::String(cwd)) => resolve_cwd(root, Some(&expand(cwd, placeholders)))?,
            _ => resolve_cwd(root, None)?,
        };
        return Ok(PluginMcpServer::Stdio {
            name: source_id.to_owned(),
            command: vec![command],
            args: args
                .iter()
                .map(|value| expand(value, placeholders))
                .collect(),
            env,
            cwd: Some(path_text(&cwd)),
        });
    }
    let url = match raw.get("url") {
        Some(Value::String(url)) if !url.is_empty() => url,
        _ => return Err("a remote MCP server has to declare a non-empty url".to_owned()),
    };
    let headers: BTreeMap<String, String> = string_mapping(raw.get("headers"))?
        .into_iter()
        .map(|(name, value)| (name, expand(&value, placeholders)))
        .collect();
    let transport = match raw.get("type") {
        Some(value) => Some(value),
        None => raw.get("transport"),
    };
    let url = crate::config::mcp::normalize_mcp_server_url(&expand(url, placeholders))
        .map_err(|error| error.to_string())?;
    let transport = if transport.and_then(Value::as_str) == Some("streamable-http") {
        "streamable-http"
    } else {
        "http"
    };
    Ok(PluginMcpServer::Http {
        name: source_id.to_owned(),
        transport: transport.to_owned(),
        url,
        headers,
    })
}

fn normalize_vendor_skill_frontmatter(
    raw: Map<String, Value>,
    path: &Path,
    source_format: DetectedPluginFormat,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Result<Map<String, Value>, String> {
    match raw.get("type") {
        None | Some(Value::Null) => {}
        Some(Value::String(kind)) if kind == "prompt" => {}
        Some(other) => return Err(format!("the vendor skill type {other} is not supported")),
    }
    let invocation: Vec<&Value> = KIMI_MODEL_INVOCATION_FIELDS
        .iter()
        .filter_map(|field| raw.get(*field))
        .collect();
    if invocation.iter().any(|value| !value.is_boolean()) {
        return Err("disableModelInvocation has to be a boolean".to_owned());
    }
    let kept = [
        "name",
        "description",
        "license",
        "compatibility",
        "metadata",
        "allowed-tools",
        "user-invocable",
    ];
    let mut normalized: Map<String, Value> = raw
        .iter()
        .filter(|(key, _)| kept.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if !invocation.is_empty() {
        // Several spellings combine restrictively: one opt-out wins.
        normalized.insert(
            DISABLE_MODEL_INVOCATION_FIELD.to_owned(),
            Value::Bool(invocation.iter().any(|value| value.as_bool() == Some(true))),
        );
    }
    if !normalized.contains_key("allowed-tools")
        && let Some(tools) = raw.get("tools")
    {
        normalized.insert("allowed-tools".to_owned(), tools.clone());
    }
    let vendor: Vec<(&String, &Value)> = raw
        .iter()
        .filter(|(key, _)| {
            key.as_str() == "type"
                || key.as_str() == "whenToUse"
                || KIMI_MODEL_INVOCATION_FIELDS.contains(&key.as_str())
        })
        .collect();
    if !vendor.is_empty() {
        let mut metadata = match normalized.get("metadata") {
            Some(Value::Object(existing)) => existing.clone(),
            _ => Map::new(),
        };
        for (key, value) in vendor {
            metadata.insert(
                format!("vendor.{key}"),
                Value::String(pycompat::dumps(value, Dumps::DEFAULT)),
            );
        }
        normalized.insert("metadata".to_owned(), Value::Object(metadata));
        diagnostics.push(diagnostic(source_format, "skill_vendor_metadata_normalized", path, "Kimi-specific skill frontmatter was kept as metadata and mapped onto the portable skill fields.", "skill", Severity::Info));
    }
    Ok(normalized)
}

/// Prefixes the tools a source declared as guidance. Reference
/// `_with_tool_guidance`; the wording is this port's own.
fn with_tool_guidance(body: &str, allowed_tools: &[String]) -> String {
    if allowed_tools.is_empty() {
        return body.to_owned();
    }
    format!(
        "## Tools declared by the source\n\nThe original capability listed these tools: {}. Follow this as guidance; runtime permissions and approvals still decide what runs.\n\n{body}",
        allowed_tools.join(", ")
    )
}

/// The first non-blank line of a body, or a fixed fallback. Reference
/// `_description_from_body`.
fn description_from_body(body: &str) -> String {
    let first = body
        .lines()
        .map(pycompat::strip)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let description = if first.is_empty() {
        "Imported static command."
    } else {
        first
    };
    description.chars().take(MAX_DESCRIPTION_CHARS).collect()
}

/// A string split on whitespace and commas, or a list of strings. Reference
/// `_string_list`.
pub(super) fn string_list(value: Option<&Value>) -> Result<Vec<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(text
            .split(|character: char| character.is_whitespace() || character == ',')
            .filter(|part| !part.is_empty())
            .map(ToOwned::to_owned)
            .collect()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| "expected a string or a list of strings".to_owned())
            })
            .collect(),
        Some(_) => Err("expected a string or a list of strings".to_owned()),
    }
}

/// An object of strings. Reference `_string_mapping`.
pub(super) fn string_mapping(value: Option<&Value>) -> Result<Vec<(String, String)>, String> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, item)| {
                item.as_str()
                    .map(|text| (key.clone(), text.to_owned()))
                    .ok_or_else(|| "expected an object of string values".to_owned())
            })
            .collect(),
        Some(_) => Err("expected an object of string values".to_owned()),
    }
}

fn expand(value: &str, placeholders: &[(&str, String)]) -> String {
    placeholders
        .iter()
        .fold(value.to_owned(), |text, (placeholder, replacement)| {
            text.replace(placeholder, replacement)
        })
}

fn resolve_command(root: &Path, original: &str, expanded: &str) -> Result<String, String> {
    let candidate = if let Some(relative) = original.strip_prefix("./") {
        root.join(relative)
    } else if original.starts_with("${") {
        PathBuf::from(expanded)
    } else {
        if original.contains('/') || original.contains('\\') || original == "." || original == ".."
        {
            return Err(
                "a stdio command has to be a bare executable or a contained path".to_owned(),
            );
        }
        return Ok(original.to_owned());
    };
    let resolved = resolve_lenient(&candidate);
    if !is_relative_to(&resolved, root) {
        return Err("a stdio command has to resolve inside the plugin".to_owned());
    }
    Ok(path_text(&resolved))
}

fn resolve_cwd(root: &Path, value: Option<&str>) -> Result<PathBuf, String> {
    let Some(value) = value.filter(|value| !matches!(*value, "." | "./")) else {
        return Ok(root.to_path_buf());
    };
    let candidate = if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        value
            .strip_prefix("./")
            .unwrap_or(value)
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .fold(root.to_path_buf(), |path, part| path.join(part))
    };
    let resolved = resolve_lenient(&candidate);
    if !is_relative_to(&resolved, root) {
        return Err("a stdio cwd has to resolve inside the plugin".to_owned());
    }
    Ok(resolved)
}

/// `path.relative_to(base).with_suffix("").as_posix()`: the name a command is
/// known by under the directory it was found in.
pub(super) fn command_source_name(path: &Path, base: &Path) -> String {
    let relative = path.strip_prefix(base).unwrap_or(path);
    let mut text = super::content::posix_relative(relative);
    if let Some(name) = relative.file_name().and_then(|name| name.to_str())
        && let Some(dot) = name.rfind('.')
        && dot > 0
        && dot + 1 < name.len()
    {
        text.truncate(text.len() - (name.len() - dot));
    }
    text
}

#[cfg(test)]
mod foreign_tests;
