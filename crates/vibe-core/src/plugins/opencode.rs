//! Reading an OpenCode package, declaratively only.
//!
//! Reference `OpenCodePluginAdapter` in `vibe/core/plugins/_foreign.py`. Its
//! JavaScript and TypeScript modules are recorded as unsupported and never
//! evaluated, and the package survives on its portable skills and the MCP
//! servers its `opencode.json` declares alone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::compatibility::{
    AdaptedMcpServer, AdaptedPluginPackage, AdaptedSkill, AdaptedUnsupportedComponent,
    DetectedPluginFormat, PluginAdapterDiagnostic, PluginAdapterResult, author_display_name,
    portable_skill_name, read_text, resolve_declared_path, typescript_identifier, walk_files,
};
use super::diagnostics::Severity;
use super::foreign::{
    adapt_mcp_servers, adapt_skill_file, child_skill_files, diagnostic, string_mapping,
    unsupported_component,
};
use super::paths::{is_relative_to, resolve_strict};
use crate::pycompat;
use crate::skills::SkillScope;

const FORMAT: DetectedPluginFormat = DetectedPluginFormat::OpenCode;
/// The suffixes of the modules OpenCode runs. Reference
/// `_OPENCODE_CODE_SUFFIXES`, which `_compatibility.py` and `_foreign.py`
/// each spell.
const CODE_SUFFIXES: [&str; 8] = ["cjs", "cts", "js", "jsx", "mjs", "mts", "ts", "tsx"];

#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static ENV_REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{env:[A-Za-z_][A-Za-z0-9_]*\}").expect("env reference pattern compiles")
});

/// Whether `path` names a JavaScript or TypeScript module: its last suffix,
/// lowercased, is one OpenCode runs.
pub(super) fn has_code_suffix(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| CODE_SUFFIXES.contains(&extension.to_lowercase().as_str()))
}

/// Reads an OpenCode package. Reference `OpenCodePluginAdapter.adapt`.
#[must_use]
pub fn adapt(root: &Path, data_root_base: &Path, scope: SkillScope) -> PluginAdapterResult {
    let mut diagnostics = Vec::new();
    let mut unsupported = Vec::new();
    let (package_path, package) = opencode_package(root, &mut diagnostics);
    let raw_name = match package.get("name") {
        Some(Value::String(name)) if !pycompat::strip(name).is_empty() => name.clone(),
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
            diagnostics.push(
                diagnostic(
                    FORMAT,
                    "package_invalid",
                    package_path.as_deref().unwrap_or(root),
                    format!("The OpenCode package has no usable name: {error}"),
                    "package",
                    Severity::Error,
                )
                .fatal(),
            );
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
            diagnostics.push(
                diagnostic(
                    FORMAT,
                    "no_portable_capabilities",
                    package_path.as_deref().unwrap_or(root),
                    "The OpenCode package holds no portable skills and no declarative MCP servers.",
                    "package",
                    Severity::Error,
                )
                .fatal(),
            );
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
        Some(Value::String(text)) if !pycompat::strip(text).is_empty() => {
            pycompat::strip(text).to_owned()
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
                FORMAT,
                "package_invalid",
                &path,
                format!("Could not read the OpenCode package metadata: {error}"),
                "package",
                Severity::Warning,
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
                    FORMAT,
                    "component_unreadable",
                    &path,
                    format!("Could not resolve the OpenCode {component}: {error}"),
                    component,
                    Severity::Error,
                )),
                Ok(resolved) if !is_relative_to(&resolved, root) => diagnostics.push(diagnostic(
                    FORMAT,
                    "path_outside_root",
                    &path,
                    format!("The OpenCode {component} has to resolve inside the plugin."),
                    component,
                    Severity::Error,
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
    has_code_suffix(path)
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
    diagnostics.push(PluginAdapterDiagnostic {
        fatal: !portable,
        ..diagnostic(
            FORMAT,
            suffix,
            first,
            message,
            kind,
            if portable {
                Severity::Warning
            } else {
                Severity::Error
            },
        )
    });
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
                    FORMAT,
                    "skill_unreadable",
                    &path,
                    format!("Could not resolve the OpenCode skill: {error}"),
                    "skill",
                    Severity::Error,
                )),
                Ok(resolved) if !is_relative_to(&resolved, root) => diagnostics.push(diagnostic(
                    FORMAT,
                    "path_outside_root",
                    &path,
                    "The OpenCode skill has to resolve inside the plugin.",
                    "skill",
                    Severity::Error,
                )),
                Ok(resolved) => {
                    files.insert(resolved);
                }
            }
        }
    }
    let mut skills = Vec::new();
    for path in files {
        let Some(skill) = adapt_skill_file(&path, root, FORMAT, diagnostics, false) else {
            continue;
        };
        let directory = path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if skill.source_name != directory {
            diagnostics.push(diagnostic(
                FORMAT,
                "skill_directory_mismatch",
                &path,
                format!(
                    "The OpenCode skill name {} does not match its directory {}.",
                    pycompat::repr_str(&skill.source_name),
                    pycompat::repr_str(&directory)
                ),
                "skill",
                Severity::Error,
            ));
            continue;
        }
        diagnostics.push(diagnostic(
            FORMAT,
            "skill_imported",
            &path,
            format!(
                "Imported the portable skill {} without evaluating OpenCode code.",
                pycompat::repr_str(&skill.source_name)
            ),
            "skill",
            Severity::Info,
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
                    FORMAT,
                    "config_invalid",
                    path,
                    format!("Could not read the declarative OpenCode config: {error}"),
                    "mcp",
                    Severity::Error,
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
            FORMAT,
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
                    FORMAT,
                    "config_unreadable",
                    &path,
                    format!("Could not resolve the OpenCode config: {error}"),
                    "config",
                    Severity::Error,
                )),
                Ok(resolved) if !is_relative_to(&resolved, root) => diagnostics.push(diagnostic(
                    FORMAT,
                    "path_outside_root",
                    &path,
                    "The OpenCode config has to resolve inside the plugin.",
                    "config",
                    Severity::Error,
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
                .find(|next| !pycompat::isspace(**next))
                .is_some_and(|next| matches!(next, '}' | ']'))
        {
            continue;
        }
        output.push(character);
    }
    output
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
            FORMAT,
            &format!("{field}_unsupported"),
            path,
            format!(
                "The OpenCode config field {} has no safe declarative translation and was skipped.",
                pycompat::repr_str(field)
            ),
            kind,
            Severity::Warning,
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
            FORMAT,
            "mcp_server_skipped",
            path,
            format!(
                "Skipped the disabled OpenCode MCP server {}.",
                pycompat::repr_str(name)
            ),
            "mcp_server",
            Severity::Info,
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
            FORMAT,
            "mcp_server_translated",
            path,
            format!(
                "Translated the declarative OpenCode MCP server {} without evaluating plugin code.",
                pycompat::repr_str(name)
            ),
            "mcp_server",
            Severity::Info,
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
        .any(|value| ENV_REFERENCE.is_match(value));
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
            FORMAT,
            "mcp_oauth_unsupported",
            path,
            format!(
                "The OpenCode MCP server {} needs host-managed OAuth and was skipped.",
                pycompat::repr_str(name)
            ),
            "mcp_server",
            Severity::Warning,
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
        .any(|value| ENV_REFERENCE.is_match(value));
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
    diagnostics.push(diagnostic(FORMAT, "mcp_environment_reference_unsupported", path, format!(
            "The OpenCode MCP server {} interpolates environment variables at runtime and was skipped.",
            pycompat::repr_str(name)
        ), "mcp_server", Severity::Warning));
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
        FORMAT,
        "mcp_server_invalid",
        path,
        format!(
            "The OpenCode MCP server {} {reason}.",
            pycompat::repr_str(name)
        ),
        "mcp_server",
        Severity::Error,
    ));
}

#[cfg(test)]
mod opencode_tests;
