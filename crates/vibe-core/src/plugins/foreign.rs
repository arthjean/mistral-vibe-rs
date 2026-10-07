//! The helpers every foreign adapter shares, and the OpenCode adapter.
//!
//! Reference `vibe/core/plugins/_foreign.py`. Skills, static commands and MCP
//! server declarations read the same way whichever format declares them, so
//! the Claude Code and Kimi Code adapters build on the functions here. An
//! OpenCode package is read declaratively only: its JavaScript and TypeScript
//! modules are recorded as unsupported and never evaluated, and the package
//! survives on its portable skills and MCP servers alone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::compatibility::{
    AdaptedMcpServer, AdaptedPluginPackage, AdaptedSkill, AdaptedUnsupportedComponent,
    DetectedPluginFormat, PluginAdapterDiagnostic, PluginAdapterResult, PluginMcpServer,
    author_display_name, portable_skill_name, python_strip, read_text, resolve_declared_path,
    typescript_identifier, walk_files,
};
use super::diagnostics::Severity;
use super::native::python_repr;
use super::paths::{is_relative_to, resolve_lax, resolve_strict};
use crate::hooks::python_json_dumps;
use crate::skills::SkillScope;
use crate::skills::parser::parse_skill_markdown;
use crate::skills::schema::SkillMetadata;

const DISABLE_MODEL_INVOCATION_FIELD: &str = "disable-model-invocation";
/// The three spellings Kimi accepts for the explicit-only invocation policy.
const KIMI_MODEL_INVOCATION_FIELDS: [&str; 3] = [
    "disableModelInvocation",
    DISABLE_MODEL_INVOCATION_FIELD,
    "disable_model_invocation",
];
const OPENCODE_CODE_SUFFIXES: [&str; 8] = ["js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts"];
const MAX_DESCRIPTION_CHARS: usize = 1024;

#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static DYNAMIC_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)(?:^|\s)!`[^`]+`").expect("dynamic command pattern compiles")
});
#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static ARGUMENT_REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$(?:ARGUMENTS(?:\[\d+\])?|\d+)\b").expect("argument pattern compiles")
});
#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static OPENCODE_ENV_REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{env:[A-Za-z_][A-Za-z0-9_]*\}").expect("env reference pattern compiles")
});

/// A diagnostic coded `plugin.compatibility.<format>.<suffix>`. Reference
/// `_diagnostic`.
pub(super) fn diagnostic(
    source_format: &str,
    suffix: &str,
    path: &Path,
    message: impl Into<String>,
    component: &str,
    severity: Severity,
    fatal: bool,
) -> PluginAdapterDiagnostic {
    PluginAdapterDiagnostic {
        severity,
        code: format!("plugin.compatibility.{source_format}.{suffix}"),
        path: path.to_path_buf(),
        message: message.into(),
        fatal,
        component: component.to_owned(),
    }
}

/// An unsupported component record.
pub(super) fn unsupported_component(
    kind: &str,
    path: &Path,
    reason: &str,
) -> AdaptedUnsupportedComponent {
    AdaptedUnsupportedComponent {
        kind: kind.to_owned(),
        path: path.to_path_buf(),
        reason: reason.to_owned(),
    }
}

/// A value as Python's truth test reads it.
pub(super) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
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
fn child_skill_files(directory: &Path) -> Vec<PathBuf> {
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

/// Whether the model may invoke a skill by its OpenAI metadata, which has to
/// resolve inside the plugin. Reference `load_openai_skill_metadata` with a
/// root.
pub(super) fn openai_policy(path: &Path, root: &Path) -> Result<bool, String> {
    if let Some(metadata) = crate::skills::openai_metadata_path(path)
        && metadata.is_file()
    {
        let resolved = resolve_strict(&metadata)
            .map_err(|error| format!("agents/openai.yaml cannot be read: {error}"))?;
        if !is_relative_to(&resolved, &resolve_lax(root)) {
            return Err("agents/openai.yaml has to resolve inside the plugin".to_owned());
        }
    }
    crate::skills::openai_invocation_policy(path)
}

/// Reads one portable Agent Skill. Reference `adapt_skill_file`.
pub(super) fn adapt_skill_file(
    path: &Path,
    root: &Path,
    source_format: &str,
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
                diagnostics.push(diagnostic(
                    source_format,
                    "skill_tools_normalized",
                    path,
                    "The skill's vendor tool list became its allowed-tools metadata; runtime approvals still decide.",
                    "skill",
                    Severity::Info,
                    false,
                ));
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
                false,
            ));
            return None;
        }
    };
    let openai_allows = match openai_policy(path, root) {
        Ok(allowed) => allowed,
        Err(error) => {
            diagnostics.push(diagnostic(
                source_format,
                "skill_openai_metadata_invalid",
                &crate::skills::openai_metadata_path(path).unwrap_or_default(),
                format!("The OpenAI skill metadata is invalid, so the model cannot invoke the skill: {error}"),
                "skill",
                Severity::Warning,
                false,
            ));
            false
        }
    };
    let prompt = with_tool_guidance(python_strip(&body), &metadata.allowed_tools);
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
    source_format: &str,
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
    let body = python_strip(&body).to_owned();
    if body.is_empty() {
        diagnostics.push(diagnostic(
            source_format,
            "command_empty",
            path,
            "The static command has no prompt body, so it was not converted.",
            "command",
            Severity::Warning,
            false,
        ));
        return None;
    }
    if DYNAMIC_COMMAND.is_match(&body) {
        diagnostics.push(diagnostic(
            source_format,
            "command_dynamic_execution_unsupported",
            path,
            "The static command injects shell output into its prompt, so it was neither run nor converted.",
            "command",
            Severity::Warning,
            false,
        ));
        return None;
    }
    if has_argument_reference(&body) {
        diagnostics.push(diagnostic(
            source_format,
            "command_arguments_unsupported",
            path,
            "The static command substitutes arguments, which a skill invocation cannot supply, so it was not converted.",
            "command",
            Severity::Warning,
            false,
        ));
        return None;
    }
    let command_name = match frontmatter.get("name") {
        Some(Value::String(name)) => python_strip(name).to_owned(),
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
        Some(Value::String(text)) if !python_strip(text).is_empty() => {
            python_strip(text).to_owned()
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
            false,
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
            false,
        ));
    }
    if !allowed_tools.is_empty() {
        diagnostics.push(diagnostic(
            source_format,
            "command_tool_policy_preserved_as_guidance",
            path,
            "The command's tool allowlist is kept in the generated skill and does not bypass runtime approvals.",
            "command",
            Severity::Info,
            false,
        ));
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

fn command_invalid(source_format: &str, path: &Path, error: &str) -> PluginAdapterDiagnostic {
    diagnostic(
        source_format,
        "command_invalid",
        path,
        format!("Could not import the static command: {error}"),
        "command",
        Severity::Error,
        false,
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
    source_format: &str,
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
                    python_repr(source_id)
                ),
                "mcp_server",
                Severity::Error,
                false,
            )),
        }
    }
    servers
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
        let env: BTreeMap<String, String> = string_mapping(raw.get("env"))?
            .into_iter()
            .map(|(name, value)| (name, expand(&value, placeholders)))
            .collect();
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
        let mut env = env;
        env.insert(
            "PLUGIN_ROOT".to_owned(),
            root.to_string_lossy().into_owned(),
        );
        env.insert(
            "PLUGIN_DATA".to_owned(),
            data_root.to_string_lossy().into_owned(),
        );
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
            cwd: Some(cwd.to_string_lossy().into_owned()),
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
    source_format: &str,
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
                Value::String(python_json_dumps(value)),
            );
        }
        normalized.insert("metadata".to_owned(), Value::Object(metadata));
        diagnostics.push(diagnostic(
            source_format,
            "skill_vendor_metadata_normalized",
            path,
            "Kimi-specific skill frontmatter was kept as metadata and mapped onto the portable skill fields.",
            "skill",
            Severity::Info,
            false,
        ));
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
        .map(python_strip)
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
    let resolved = resolve_lax(&candidate);
    if !is_relative_to(&resolved, root) {
        return Err("a stdio command has to resolve inside the plugin".to_owned());
    }
    Ok(resolved.to_string_lossy().into_owned())
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
    let resolved = resolve_lax(&candidate);
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

/// The keys of the object at `pointer` inside a JSON document, in document
/// order, the first occurrence of a repeated key keeping its place as a Python
/// `dict` keeps it. `serde_json::Map` sorts its keys in this build, so an
/// adapter whose output follows a document's key order reads it here.
pub(super) fn object_key_order(text: &str, pointer: &[&str]) -> Option<Vec<String>> {
    let mut node: OrderedJson = serde_json::from_str(text).ok()?;
    for key in pointer {
        let OrderedJson::Object(entries) = node else {
            return None;
        };
        node = entries.into_iter().rev().find(|(name, _)| name == key)?.1;
    }
    let OrderedJson::Object(entries) = node else {
        return None;
    };
    let mut keys: Vec<String> = Vec::new();
    for (key, _) in entries {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Some(keys)
}

/// A JSON value whose objects keep their entries in document order.
enum OrderedJson {
    Scalar,
    Array,
    Object(Vec<(String, OrderedJson)>),
}

impl<'de> serde::Deserialize<'de> for OrderedJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = OrderedJson;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("any JSON value")
            }

            fn visit_bool<E>(self, _: bool) -> Result<OrderedJson, E> {
                Ok(OrderedJson::Scalar)
            }

            fn visit_i64<E>(self, _: i64) -> Result<OrderedJson, E> {
                Ok(OrderedJson::Scalar)
            }

            fn visit_u64<E>(self, _: u64) -> Result<OrderedJson, E> {
                Ok(OrderedJson::Scalar)
            }

            fn visit_f64<E>(self, _: f64) -> Result<OrderedJson, E> {
                Ok(OrderedJson::Scalar)
            }

            fn visit_str<E>(self, _: &str) -> Result<OrderedJson, E> {
                Ok(OrderedJson::Scalar)
            }

            fn visit_unit<E>(self) -> Result<OrderedJson, E> {
                Ok(OrderedJson::Scalar)
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut items: A,
            ) -> Result<OrderedJson, A::Error> {
                while items.next_element::<OrderedJson>()?.is_some() {}
                Ok(OrderedJson::Array)
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut entries: A,
            ) -> Result<OrderedJson, A::Error> {
                let mut object = Vec::new();
                while let Some((key, value)) = entries.next_entry::<String, OrderedJson>()? {
                    object.push((key, value));
                }
                Ok(OrderedJson::Object(object))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// Reads an OpenCode package. Reference `OpenCodePluginAdapter.adapt`.
#[must_use]
pub fn adapt(root: &Path, data_root_base: &Path, scope: SkillScope) -> PluginAdapterResult {
    let mut diagnostics = Vec::new();
    let mut unsupported = Vec::new();
    let (package_path, package) = opencode_package(root, &mut diagnostics);
    let raw_name = match package.get("name") {
        Some(Value::String(name)) if !python_strip(name).is_empty() => name.clone(),
        _ => root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    let name = match portable_skill_name(
        &raw_name
            .strip_prefix('@')
            .unwrap_or(&raw_name)
            .replace('/', "-"),
        "",
    ) {
        Ok(name) => name,
        Err(error) => {
            // The reference lets this escape the adapter; the port drops the
            // package with a fatal report instead.
            diagnostics.push(diagnostic(
                "opencode",
                "package_invalid",
                package_path.as_deref().unwrap_or(root),
                format!("The OpenCode package has no usable name: {error}"),
                "package",
                Severity::Error,
                true,
            ));
            return PluginAdapterResult {
                package: None,
                diagnostics,
                unsupported_components: unsupported,
            };
        }
    };
    let namespace = typescript_identifier(&name);
    let data_root = data_root_base.join(&namespace);
    let executable_paths = opencode_executable_paths(root, &package, &mut diagnostics);
    let custom_tool_paths = opencode_contained_code_paths(
        root,
        &[root.join(".opencode").join("tools")],
        &mut diagnostics,
        "custom_tool",
    );
    let skills = opencode_skills(root, &mut diagnostics);
    let (mcp_servers, config_paths) =
        opencode_mcp_servers(root, &data_root, &mut diagnostics, &mut unsupported);
    let portable = !skills.is_empty() || !mcp_servers.is_empty();
    report_opencode_code(
        &executable_paths,
        portable,
        &mut diagnostics,
        &mut unsupported,
        (
            "executable_plugin",
            "foreign_code_execution_unsupported",
            "executable_format_unsupported",
        ),
        "The OpenCode JavaScript and TypeScript modules are kept as package data and never imported or evaluated.",
    );
    report_opencode_code(
        &custom_tool_paths,
        portable,
        &mut diagnostics,
        &mut unsupported,
        (
            "custom_tool",
            "opencode_executable_tool_unsupported",
            "custom_tools_unsupported",
        ),
        "The OpenCode custom tools are kept as package data and never imported or evaluated.",
    );
    if !portable {
        if executable_paths.is_empty() && custom_tool_paths.is_empty() {
            diagnostics.push(diagnostic(
                "opencode",
                "no_portable_capabilities",
                package_path.as_deref().unwrap_or(root),
                "The OpenCode package holds no portable skills and no declarative MCP servers.",
                "package",
                Severity::Error,
                true,
            ));
        }
        return PluginAdapterResult {
            package: None,
            diagnostics,
            unsupported_components: unsupported,
        };
    }
    let manifest_path = package_path
        .clone()
        .or_else(|| config_paths.first().cloned())
        .or_else(|| executable_paths.first().cloned())
        .or_else(|| custom_tool_paths.first().cloned())
        .or_else(|| skills.first().map(|skill| skill.source_path.clone()))
        .unwrap_or_else(|| root.to_path_buf());
    let description = match package.get("description") {
        Some(Value::String(text)) if !python_strip(text).is_empty() => {
            python_strip(text).to_owned()
        }
        _ => format!("Portable capabilities imported from OpenCode package {name}."),
    };
    let private_metadata = BTreeMap::from([
        ("package".to_owned(), Value::Object(package.clone())),
        (
            "configPaths".to_owned(),
            Value::Array(
                config_paths
                    .iter()
                    .map(|path| Value::String(path.to_string_lossy().into_owned()))
                    .collect(),
            ),
        ),
    ]);
    PluginAdapterResult {
        package: Some(AdaptedPluginPackage {
            source_format: DetectedPluginFormat::OpenCode,
            manifest_path,
            name,
            version: package
                .get("version")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            description,
            namespace,
            data_root,
            scope,
            skill_roots: Vec::new(),
            mcp_servers,
            tool_overrides: BTreeMap::new(),
            private_metadata,
            author: author_display_name(package.get("author")),
            adapted_skills: skills,
            adapted_hooks: Vec::new(),
        }),
        diagnostics,
        unsupported_components: unsupported,
    }
}

fn opencode_package(
    root: &Path,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> (Option<PathBuf>, Map<String, Value>) {
    let path = root.join("package.json");
    if !path.is_file() {
        return (None, Map::new());
    }
    let loaded = resolve_strict(&path)
        .map_err(|error| error.to_string())
        .and_then(|resolved| {
            if !is_relative_to(&resolved, root) {
                return Err("package.json has to resolve inside the plugin".to_owned());
            }
            let text = read_text(&resolved).map_err(|error| error.to_string())?;
            match serde_json::from_str::<Value>(&text).map_err(|error| error.to_string())? {
                Value::Object(map) => Ok((resolved, map)),
                _ => Err("package.json has to hold an object".to_owned()),
            }
        });
    match loaded {
        Ok((resolved, map)) => (Some(resolved), map),
        Err(error) => {
            diagnostics.push(diagnostic(
                "opencode",
                "package_invalid",
                &path,
                format!("Could not read the OpenCode package metadata: {error}"),
                "package",
                Severity::Warning,
                false,
            ));
            (None, Map::new())
        }
    }
}

fn opencode_executable_paths(
    root: &Path,
    package: &Map<String, Value>,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Vec<PathBuf> {
    let mut paths: BTreeSet<PathBuf> = opencode_contained_code_paths(
        root,
        &[root.join(".opencode").join("plugins"), root.join("plugins")],
        diagnostics,
        "executable_plugin",
    )
    .into_iter()
    .collect();
    let mut entrypoints = Vec::new();
    for field in ["main", "module", "browser", "exports"] {
        collect_strings(package.get(field), &mut entrypoints);
    }
    for value in entrypoints {
        if let Ok(path) = resolve_declared_path(root, &value)
            && path.is_file()
            && is_opencode_code_path(&path)
        {
            paths.insert(path);
        }
    }
    paths.into_iter().collect()
}

fn collect_strings(value: Option<&Value>, output: &mut Vec<String>) {
    match value {
        Some(Value::String(text)) => output.push(text.clone()),
        Some(Value::Object(map)) => map
            .values()
            .for_each(|child| collect_strings(Some(child), output)),
        Some(Value::Array(items)) => items
            .iter()
            .for_each(|child| collect_strings(Some(child), output)),
        _ => {}
    }
}

fn opencode_contained_code_paths(
    root: &Path,
    directories: &[PathBuf],
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    component: &str,
) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    for directory in directories {
        if !directory.is_dir() {
            continue;
        }
        let mut found = Vec::new();
        walk_files(directory, &mut found);
        for path in found {
            if !path.is_file() || !is_opencode_code_path(&path) {
                continue;
            }
            match resolve_strict(&path) {
                Err(error) => diagnostics.push(diagnostic(
                    "opencode",
                    "component_unreadable",
                    &path,
                    format!("Could not resolve the OpenCode {component}: {error}"),
                    component,
                    Severity::Error,
                    false,
                )),
                Ok(resolved) if !is_relative_to(&resolved, root) => diagnostics.push(diagnostic(
                    "opencode",
                    "path_outside_root",
                    &path,
                    format!("The OpenCode {component} has to resolve inside the plugin."),
                    component,
                    Severity::Error,
                    false,
                )),
                Ok(resolved) => {
                    paths.insert(resolved);
                }
            }
        }
    }
    paths.into_iter().collect()
}

fn is_opencode_code_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let lowered = name.to_lowercase();
    if [".d.ts", ".d.mts", ".d.cts"]
        .iter()
        .any(|suffix| lowered.ends_with(suffix))
    {
        return false;
    }
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            OPENCODE_CODE_SUFFIXES.contains(&extension.to_lowercase().as_str())
        })
}

fn report_opencode_code(
    paths: &[PathBuf],
    portable: bool,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
    (kind, reason, suffix): (&str, &str, &str),
    message: &str,
) {
    unsupported.extend(
        paths
            .iter()
            .map(|path| unsupported_component(kind, path, reason)),
    );
    let Some(first) = paths.first() else {
        return;
    };
    diagnostics.push(diagnostic(
        "opencode",
        suffix,
        first,
        message,
        kind,
        if portable {
            Severity::Warning
        } else {
            Severity::Error
        },
        !portable,
    ));
}

fn opencode_skills(
    root: &Path,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Vec<AdaptedSkill> {
    let mut files = BTreeSet::new();
    for directory in [
        root.join("skills"),
        root.join(".opencode").join("skills"),
        root.join(".agents").join("skills"),
        root.join(".claude").join("skills"),
    ] {
        if !directory.is_dir() {
            continue;
        }
        for path in child_skill_files(&directory) {
            match resolve_strict(&path) {
                Err(error) => diagnostics.push(diagnostic(
                    "opencode",
                    "skill_unreadable",
                    &path,
                    format!("Could not resolve the OpenCode skill: {error}"),
                    "skill",
                    Severity::Error,
                    false,
                )),
                Ok(resolved) if !is_relative_to(&resolved, root) => diagnostics.push(diagnostic(
                    "opencode",
                    "path_outside_root",
                    &path,
                    "The OpenCode skill has to resolve inside the plugin.",
                    "skill",
                    Severity::Error,
                    false,
                )),
                Ok(resolved) => {
                    files.insert(resolved);
                }
            }
        }
    }
    let mut skills = Vec::new();
    for path in files {
        let Some(skill) = adapt_skill_file(&path, root, "opencode", diagnostics, false) else {
            continue;
        };
        let directory = path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if skill.source_name != directory {
            diagnostics.push(diagnostic(
                "opencode",
                "skill_directory_mismatch",
                &path,
                format!(
                    "The OpenCode skill name {} does not match its directory {}.",
                    python_repr(&skill.source_name),
                    python_repr(&directory)
                ),
                "skill",
                Severity::Error,
                false,
            ));
            continue;
        }
        diagnostics.push(diagnostic(
            "opencode",
            "skill_imported",
            &path,
            format!(
                "Imported the portable skill {} without evaluating OpenCode code.",
                python_repr(&skill.source_name)
            ),
            "skill",
            Severity::Info,
            false,
        ));
        skills.push(skill);
    }
    skills
}

fn opencode_mcp_servers(
    root: &Path,
    data_root: &Path,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) -> (Vec<AdaptedMcpServer>, Vec<PathBuf>) {
    let mut servers = Vec::new();
    let config_paths = opencode_config_paths(root, diagnostics);
    for path in &config_paths {
        let loaded = opencode_load_jsonc(path).and_then(|value| {
            report_opencode_config_extensions(path, &value, diagnostics, unsupported);
            match value.get("mcp") {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Object(raw)) => Ok(Some(raw.clone())),
                Some(_) => Err("the OpenCode mcp field has to be an object".to_owned()),
            }
        });
        let raw_servers = match loaded {
            Ok(Some(raw)) => raw,
            Ok(None) => continue,
            Err(error) => {
                diagnostics.push(diagnostic(
                    "opencode",
                    "config_invalid",
                    path,
                    format!("Could not read the declarative OpenCode config: {error}"),
                    "mcp",
                    Severity::Error,
                    false,
                ));
                continue;
            }
        };
        let translated =
            opencode_translate_mcp_mapping(path, &raw_servers, diagnostics, unsupported);
        servers.extend(adapt_mcp_servers(
            root,
            data_root,
            &translated,
            path,
            "opencode",
            diagnostics,
            &[],
        ));
    }
    (servers, config_paths)
}

fn opencode_config_paths(
    root: &Path,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for directory in [root.to_path_buf(), root.join(".opencode")] {
        for name in ["opencode.json", "opencode.jsonc"] {
            let path = directory.join(name);
            if !path.is_file() {
                continue;
            }
            match resolve_strict(&path) {
                Err(error) => diagnostics.push(diagnostic(
                    "opencode",
                    "config_unreadable",
                    &path,
                    format!("Could not resolve the OpenCode config: {error}"),
                    "config",
                    Severity::Error,
                    false,
                )),
                Ok(resolved) if !is_relative_to(&resolved, root) => diagnostics.push(diagnostic(
                    "opencode",
                    "path_outside_root",
                    &path,
                    "The OpenCode config has to resolve inside the plugin.",
                    "config",
                    Severity::Error,
                    false,
                )),
                Ok(resolved) => paths.push(resolved),
            }
        }
    }
    paths
}

fn opencode_load_jsonc(path: &Path) -> Result<Map<String, Value>, String> {
    let text = read_text(path).map_err(|error| error.to_string())?;
    let stripped = remove_trailing_commas(&remove_json_comments(&text)?);
    match serde_json::from_str::<Value>(&stripped).map_err(|error| error.to_string())? {
        Value::Object(map) => Ok(map),
        _ => Err("the OpenCode config has to hold an object".to_owned()),
    }
}

/// Removes `//` and `/* */` comments outside strings, keeping the line breaks
/// a block comment spanned. Reference `_opencode_remove_json_comments`.
fn remove_json_comments(value: &str) -> Result<String, String> {
    let characters: Vec<char> = value.chars().collect();
    let mut output = String::with_capacity(value.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < characters.len() {
        let character = characters[index];
        let following = characters.get(index + 1).copied();
        if in_string {
            output.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if character == '"' {
            in_string = true;
            output.push(character);
            index += 1;
            continue;
        }
        if character == '/' && following == Some('/') {
            index += 2;
            while index < characters.len() && !matches!(characters[index], '\r' | '\n') {
                index += 1;
            }
            continue;
        }
        if character == '/' && following == Some('*') {
            index += 2;
            loop {
                if index + 1 >= characters.len() {
                    return Err("a JSONC block comment is not terminated".to_owned());
                }
                if characters[index] == '*' && characters[index + 1] == '/' {
                    index += 2;
                    break;
                }
                if matches!(characters[index], '\r' | '\n') {
                    output.push(characters[index]);
                }
                index += 1;
            }
            continue;
        }
        output.push(character);
        index += 1;
    }
    Ok(output)
}

/// Drops a comma that only whitespace separates from a closing bracket.
/// Reference `_opencode_remove_trailing_commas`.
fn remove_trailing_commas(value: &str) -> String {
    let characters: Vec<char> = value.chars().collect();
    let mut output = String::with_capacity(value.len());
    let mut in_string = false;
    let mut escaped = false;
    for (index, &character) in characters.iter().enumerate() {
        if in_string {
            output.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        if character == '"' {
            in_string = true;
            output.push(character);
            continue;
        }
        if character == ','
            && characters[index + 1..]
                .iter()
                .find(|next| !is_python_space(**next))
                .is_some_and(|next| matches!(next, '}' | ']'))
        {
            continue;
        }
        output.push(character);
    }
    output
}

fn is_python_space(character: char) -> bool {
    character.is_whitespace() || matches!(character, '\u{1c}'..='\u{1f}')
}

fn report_opencode_config_extensions(
    path: &Path,
    value: &Map<String, Value>,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) {
    for (field, kind, reason) in [
        ("agent", "agent", "opencode_agent_translation_unsupported"),
        (
            "command",
            "command",
            "opencode_command_translation_unsupported",
        ),
        (
            "plugin",
            "plugin_dependency",
            "opencode_plugin_dependency_unsupported",
        ),
        (
            "provider",
            "provider_extension",
            "opencode_provider_extension_unsupported",
        ),
    ] {
        // `raw in (None, False, (), [], {})`: `0 == False` in Python, so a
        // zero is skipped too.
        let skipped = match value.get(field) {
            None | Some(Value::Null | Value::Bool(false)) => true,
            Some(Value::Number(number)) => number.as_f64() == Some(0.0),
            Some(Value::Array(items)) => items.is_empty(),
            Some(Value::Object(map)) => map.is_empty(),
            Some(_) => false,
        };
        if skipped {
            continue;
        }
        unsupported.push(unsupported_component(kind, path, reason));
        diagnostics.push(diagnostic(
            "opencode",
            &format!("{field}_unsupported"),
            path,
            format!(
                "The OpenCode config field {} has no safe declarative translation and was skipped.",
                python_repr(field)
            ),
            kind,
            Severity::Warning,
            false,
        ));
    }
}

fn opencode_translate_mcp_mapping(
    path: &Path,
    raw_servers: &Map<String, Value>,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) -> Map<String, Value> {
    let ordered: BTreeMap<&String, &Value> = raw_servers.iter().collect();
    let mut translated = Map::new();
    for (name, raw) in ordered {
        if let Some(server) =
            opencode_translate_mcp_server(path, name, raw, diagnostics, unsupported)
        {
            translated.insert(name.clone(), Value::Object(server));
        }
    }
    translated
}

fn opencode_translate_mcp_server(
    path: &Path,
    name: &str,
    raw: &Value,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) -> Option<Map<String, Value>> {
    let Value::Object(raw) = raw else {
        opencode_invalid_mcp(diagnostics, path, name, "has to be an object");
        return None;
    };
    if raw.get("enabled") == Some(&Value::Bool(false)) {
        diagnostics.push(diagnostic(
            "opencode",
            "mcp_server_skipped",
            path,
            format!(
                "Skipped the disabled OpenCode MCP server {}.",
                python_repr(name)
            ),
            "mcp_server",
            Severity::Info,
            false,
        ));
        return None;
    }
    let translated = match raw.get("type").and_then(Value::as_str) {
        Some("local") => opencode_translate_local_mcp(path, name, raw, diagnostics, unsupported),
        Some("remote") => opencode_translate_remote_mcp(path, name, raw, diagnostics, unsupported),
        _ => {
            let kind = raw
                .get("type")
                .map_or_else(|| "None".to_owned(), Value::to_string);
            opencode_invalid_mcp(
                diagnostics,
                path,
                name,
                &format!("has the unsupported type {kind}"),
            );
            return None;
        }
    };
    if translated.is_some() {
        diagnostics.push(diagnostic(
            "opencode",
            "mcp_server_translated",
            path,
            format!(
                "Translated the declarative OpenCode MCP server {} without evaluating plugin code.",
                python_repr(name)
            ),
            "mcp_server",
            Severity::Info,
            false,
        ));
    }
    translated
}

fn opencode_translate_local_mcp(
    path: &Path,
    name: &str,
    raw: &Map<String, Value>,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) -> Option<Map<String, Value>> {
    let command: Option<Vec<String>> = match raw.get("command") {
        Some(Value::Array(items)) if !items.is_empty() => items
            .iter()
            .map(|item| item.as_str().map(ToOwned::to_owned))
            .collect(),
        _ => None,
    };
    let Some(command) = command else {
        opencode_invalid_mcp(
            diagnostics,
            path,
            name,
            "needs a non-empty array of string command parts",
        );
        return None;
    };
    let environment = raw
        .get("environment")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let Ok(env) = string_mapping(Some(&environment)) else {
        opencode_invalid_mcp(
            diagnostics,
            path,
            name,
            "has an environment with non-string values",
        );
        return None;
    };
    let referenced = command
        .iter()
        .chain(env.iter().map(|(_, value)| value))
        .any(|value| OPENCODE_ENV_REFERENCE.is_match(value));
    if referenced {
        opencode_unsupported_env_reference(diagnostics, unsupported, path, name);
        return None;
    }
    let mut translated = Map::new();
    translated.insert("command".to_owned(), Value::String(command[0].clone()));
    translated.insert(
        "args".to_owned(),
        Value::Array(command[1..].iter().cloned().map(Value::String).collect()),
    );
    translated.insert(
        "env".to_owned(),
        Value::Object(
            env.into_iter()
                .map(|(key, value)| (key, Value::String(value)))
                .collect(),
        ),
    );
    if let Some(Value::String(cwd)) = raw.get("cwd") {
        translated.insert("cwd".to_owned(), Value::String(cwd.clone()));
    }
    Some(translated)
}

fn opencode_translate_remote_mcp(
    path: &Path,
    name: &str,
    raw: &Map<String, Value>,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) -> Option<Map<String, Value>> {
    if !matches!(
        raw.get("oauth"),
        None | Some(Value::Null | Value::Bool(false))
    ) {
        diagnostics.push(diagnostic(
            "opencode",
            "mcp_oauth_unsupported",
            path,
            format!(
                "The OpenCode MCP server {} needs host-managed OAuth and was skipped.",
                python_repr(name)
            ),
            "mcp_server",
            Severity::Warning,
            false,
        ));
        unsupported.push(unsupported_component(
            "mcp_oauth",
            path,
            "opencode_oauth_translation_unsupported",
        ));
        return None;
    }
    let Ok(headers) = string_mapping(raw.get("headers")) else {
        opencode_invalid_mcp(
            diagnostics,
            path,
            name,
            "has headers with non-string values",
        );
        return None;
    };
    let url = match raw.get("url") {
        Some(Value::String(url)) if !url.is_empty() => url.clone(),
        _ => {
            opencode_invalid_mcp(diagnostics, path, name, "needs a non-empty url");
            return None;
        }
    };
    let referenced = std::iter::once(&url)
        .chain(headers.iter().map(|(_, value)| value))
        .any(|value| OPENCODE_ENV_REFERENCE.is_match(value));
    if referenced {
        opencode_unsupported_env_reference(diagnostics, unsupported, path, name);
        return None;
    }
    let mut translated = Map::new();
    translated.insert("url".to_owned(), Value::String(url));
    translated.insert(
        "headers".to_owned(),
        Value::Object(
            headers
                .into_iter()
                .map(|(key, value)| (key, Value::String(value)))
                .collect(),
        ),
    );
    translated.insert("type".to_owned(), Value::String("http".to_owned()));
    Some(translated)
}

fn opencode_unsupported_env_reference(
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
    path: &Path,
    name: &str,
) {
    diagnostics.push(diagnostic(
        "opencode",
        "mcp_environment_reference_unsupported",
        path,
        format!(
            "The OpenCode MCP server {} interpolates environment variables at runtime and was skipped.",
            python_repr(name)
        ),
        "mcp_server",
        Severity::Warning,
        false,
    ));
    unsupported.push(unsupported_component(
        "mcp_environment_reference",
        path,
        "opencode_environment_interpolation_unsupported",
    ));
}

fn opencode_invalid_mcp(
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    path: &Path,
    name: &str,
    reason: &str,
) {
    diagnostics.push(diagnostic(
        "opencode",
        "mcp_server_invalid",
        path,
        format!("The OpenCode MCP server {} {reason}.", python_repr(name)),
        "mcp_server",
        Severity::Error,
        false,
    ));
}

#[cfg(test)]
mod foreign_tests;
