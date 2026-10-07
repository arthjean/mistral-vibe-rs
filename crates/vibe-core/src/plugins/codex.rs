//! Reading a Codex plugin as a native package.
//!
//! Reference `CodexPluginAdapter` in `vibe/core/plugins/_codex.py`. A Codex
//! package keeps its manifest at `.codex-plugin/plugin.json`; its skill roots
//! and MCP servers carry over, while interface metadata, OpenAI Apps, hooks
//! and agent metadata are kept as package data and reported as unsupported.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::compatibility::{
    AdaptedMcpServer, AdaptedPluginPackage, AdaptedUnsupportedComponent, DetectedPluginFormat,
    PluginAdapterDiagnostic, PluginAdapterResult, PluginMcpHttpAuth, PluginMcpServer,
    author_display_name, python_strip, read_text, resolve_declared_path, typescript_identifier,
};
use super::diagnostics::Severity;
use super::native::python_repr;
use super::paths::{is_relative_to, resolve_lax, resolve_strict};
use super::strict::char_len;
use crate::config::mcp::{normalize_mcp_server_name, normalize_mcp_server_url};
use crate::skills::SkillScope;

const MANIFEST: &str = ".codex-plugin/plugin.json";
const OPENAI_METADATA_FILENAME: &str = "openai.yaml";
/// Only `openai.yaml` is metadata Codex executes; `openai.yml` is scanned so
/// it can be reported as kept but unsupported.
const AGENT_METADATA_FILENAMES: [&str; 2] = [OPENAI_METADATA_FILENAME, "openai.yml"];
const MAX_NAME_LENGTH: usize = 256;

#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static HEADER_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[!#$%&'*+.^_`|~0-9A-Za-z-]+$").expect("header pattern compiles")
});
#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static ENV_VAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("env var pattern compiles"));

/// The validated manifest. Reference `_CodexPluginManifest`, which allows
/// undeclared keys.
struct Manifest {
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    author: Option<Value>,
    skills: Option<Value>,
    mcp_servers: Option<Value>,
    apps: Option<Value>,
    hooks: Option<Value>,
    interface: Option<Value>,
}

/// Adapts the Codex plugin at `root`. Reference `CodexPluginAdapter.adapt`.
#[must_use]
pub fn adapt(root: &Path, data_root_base: &Path, scope: SkillScope) -> PluginAdapterResult {
    let manifest_path = root.join(".codex-plugin").join("plugin.json");
    let loaded = resolve_declared_path(root, MANIFEST).and_then(|resolved| {
        if !resolved.is_file() {
            return Err("the Codex manifest has to be a regular file".to_owned());
        }
        let text = read_text(&resolved).map_err(|error| error.to_string())?;
        let raw: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        let manifest = parse_manifest(&raw)?;
        Ok((resolved, raw, manifest))
    });
    let (resolved_manifest, raw_manifest, manifest) = match loaded {
        Ok(loaded) => loaded,
        Err(error) => {
            return PluginAdapterResult {
                package: None,
                diagnostics: vec![diagnostic(
                    Severity::Error,
                    "plugin.compatibility.codex.manifest_invalid",
                    manifest_path,
                    format!("Could not load the Codex manifest: {error}"),
                    true,
                    "manifest",
                )],
                unsupported_components: Vec::new(),
            };
        }
    };

    let mut diagnostics = Vec::new();
    let mut unsupported = Vec::new();
    let name = match &manifest.name {
        Some(name) if !python_strip(name).is_empty() => name.clone(),
        _ => file_name(root),
    };
    let version = manifest
        .version
        .as_deref()
        .map(|version| python_strip(version).to_owned())
        .filter(|version| !version.is_empty());
    let mut private_metadata = BTreeMap::from([("codexManifest".to_owned(), raw_manifest)]);
    let namespace = typescript_identifier(&name);
    let data_root = data_root_base.join(&namespace);
    let skill_roots = skill_roots(root, &manifest, &mut diagnostics);
    let (mcp_servers, mcp_metadata) = load_mcp_servers(
        root,
        &data_root,
        &resolved_manifest,
        &manifest,
        &mut diagnostics,
    );
    if let Some(metadata) = mcp_metadata {
        private_metadata.insert("codexMcp".to_owned(), metadata);
    }

    report_interface(
        &resolved_manifest,
        &manifest,
        &mut diagnostics,
        &mut unsupported,
    );
    report_apps(
        root,
        &resolved_manifest,
        &manifest,
        &mut diagnostics,
        &mut unsupported,
    );
    report_hooks(
        root,
        &resolved_manifest,
        &manifest,
        &mut diagnostics,
        &mut unsupported,
    );
    report_agent_metadata(root, &skill_roots, &mut diagnostics, &mut unsupported);

    let description = match &manifest.description {
        Some(description) if !description.is_empty() => description.clone(),
        _ => format!("Capabilities provided by {name}."),
    };
    let package = AdaptedPluginPackage {
        source_format: DetectedPluginFormat::Codex,
        manifest_path: resolved_manifest,
        name,
        version,
        description,
        namespace,
        data_root,
        scope,
        skill_roots,
        mcp_servers,
        tool_overrides: BTreeMap::new(),
        private_metadata,
        author: author_display_name(manifest.author.as_ref()),
        adapted_skills: Vec::new(),
        adapted_hooks: Vec::new(),
    };
    PluginAdapterResult {
        package: Some(package),
        diagnostics,
        unsupported_components: unsupported,
    }
}

fn parse_manifest(raw: &Value) -> Result<Manifest, String> {
    let Value::Object(map) = raw else {
        return Err("the manifest has to be an object".to_owned());
    };
    let name = optional_string(map, "name")?;
    if name
        .as_deref()
        .is_some_and(|name| char_len(name) > MAX_NAME_LENGTH)
    {
        return Err("name holds more than 256 characters".to_owned());
    }
    let json = |key: &str| map.get(key).filter(|value| !value.is_null()).cloned();
    Ok(Manifest {
        name,
        version: optional_string(map, "version")?,
        description: optional_string(map, "description")?,
        author: json("author"),
        skills: json("skills"),
        mcp_servers: map
            .get("mcpServers")
            .or_else(|| map.get("mcp_servers"))
            .filter(|value| !value.is_null())
            .cloned(),
        apps: json("apps"),
        hooks: json("hooks"),
        interface: json("interface"),
    })
}

/// A strict optional string: absent and `null` read as `None`.
fn optional_string(map: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("{key} must be a string or null")),
    }
}

/// Reference `CodexPluginAdapter._skill_roots`.
fn skill_roots(
    root: &Path,
    manifest: &Manifest,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Vec<PathBuf> {
    let mut values = if let Some(values) = declared_paths(manifest.skills.as_ref()) {
        values
    } else {
        diagnostics.push(diagnostic(
            Severity::Error,
            "plugin.compatibility.codex.skills_declaration_invalid",
            root.join(".codex-plugin").join("plugin.json"),
            "The Codex skills declaration has to be a relative path or a list of relative paths.",
            false,
            "skill",
        ));
        Vec::new()
    };
    if exists_or_link(&root.join("skills")) {
        values.push("./skills/".to_owned());
    }
    let mut roots = BTreeSet::new();
    for value in values {
        let resolved = resolve_declared_path(root, &value).and_then(|resolved| {
            if resolved.is_dir() {
                Ok(resolved)
            } else {
                Err("the Codex skills path has to resolve to a directory".to_owned())
            }
        });
        match resolved {
            Ok(resolved) => {
                roots.insert(resolved);
            }
            Err(error) => diagnostics.push(diagnostic(
                Severity::Error,
                "plugin.path.outside_root",
                posix_join(root, &value),
                format!("Could not load the Codex skills: {error}"),
                false,
                "skill",
            )),
        }
    }
    roots.into_iter().collect()
}

/// One configuration source: where it was read, its servers, and the raw
/// value kept as package data.
type McpSource = (PathBuf, Map<String, Value>, Value);

/// Reference `CodexPluginAdapter._mcp_servers`.
fn load_mcp_servers(
    root: &Path,
    data_root: &Path,
    manifest_path: &Path,
    manifest: &Manifest,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> (Vec<AdaptedMcpServer>, Option<Value>) {
    let mut sources: Vec<McpSource> = Vec::new();
    if exists_or_link(&root.join(".mcp.json"))
        && let Some(loaded) = load_mcp_config(root, "./.mcp.json", diagnostics)
    {
        sources.push(loaded);
    }
    match &manifest.mcp_servers {
        None => {}
        Some(Value::Object(declared)) => {
            sources.push((
                manifest_path.to_path_buf(),
                declared.clone(),
                Value::Object(declared.clone()),
            ));
        }
        Some(Value::String(declared)) => {
            if let Some(loaded) = load_mcp_config(root, declared, diagnostics)
                && sources.iter().all(|(path, _, _)| *path != loaded.0)
            {
                sources.push(loaded);
            }
        }
        Some(_) => diagnostics.push(diagnostic(
            Severity::Error,
            "plugin.compatibility.codex.mcp_declaration_invalid",
            root.join(".codex-plugin").join("plugin.json"),
            "The Codex mcpServers declaration has to be a relative path or an inline object.",
            false,
            "mcp",
        )),
    }
    let loaded: Vec<AdaptedMcpServer> = sources
        .iter()
        .flat_map(|(config_path, raw_servers, _)| {
            load_mcp_server_definitions(root, data_root, raw_servers, config_path, diagnostics)
        })
        .collect();
    let servers = remove_mcp_server_collisions(loaded, diagnostics);
    let mut metadata = Map::new();
    for (config_path, _, raw) in sources {
        let key = match config_path.strip_prefix(root) {
            Ok(relative) => super::content::posix_relative(relative),
            Err(_) => MANIFEST.to_owned(),
        };
        metadata.insert(key, raw);
    }
    let metadata = (!metadata.is_empty()).then_some(Value::Object(metadata));
    (servers, metadata)
}

/// Reference `_load_mcp_config`.
fn load_mcp_config(
    root: &Path,
    value: &str,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Option<McpSource> {
    let loaded = resolve_declared_path(root, value).and_then(|resolved| {
        if !resolved.is_file() {
            return Err("the Codex MCP configuration has to be a regular file".to_owned());
        }
        let text = read_text(&resolved).map_err(|error| error.to_string())?;
        let raw: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        let servers = match &raw {
            Value::Object(map) => match map.get("mcpServers").or_else(|| map.get("mcp_servers")) {
                Some(Value::Object(servers)) => servers.clone(),
                None => return Err("mcpServers is required".to_owned()),
                Some(_) => return Err("mcpServers has to be an object".to_owned()),
            },
            _ => return Err("the configuration has to be an object".to_owned()),
        };
        Ok((resolved, servers, raw))
    });
    match loaded {
        Ok(loaded) => Some(loaded),
        Err(error) => {
            diagnostics.push(diagnostic(
                Severity::Error,
                "plugin.compatibility.codex.mcp_config_invalid",
                posix_join(root, value),
                format!("Could not load the Codex MCP configuration: {error}"),
                false,
                "mcp",
            ));
            None
        }
    }
}

/// Reference `_load_mcp_server_definitions`.
fn load_mcp_server_definitions(
    root: &Path,
    data_root: &Path,
    raw_servers: &Map<String, Value>,
    config_path: &Path,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Vec<AdaptedMcpServer> {
    let mut entries: Vec<(&String, &Value)> = raw_servers.iter().collect();
    entries.sort_by(|left, right| left.0.cmp(right.0));
    let mut servers = Vec::new();
    for (source_id, raw) in entries {
        match codex_mcp_server(root, data_root, source_id, raw, config_path) {
            Ok((server, server_diagnostics)) => {
                diagnostics.extend(server_diagnostics);
                servers.push(AdaptedMcpServer {
                    source_id: source_id.clone(),
                    server,
                    config_file: config_path.to_path_buf(),
                });
            }
            Err(error) => diagnostics.push(diagnostic(
                Severity::Error,
                "plugin.compatibility.codex.mcp_server_invalid",
                config_path.to_path_buf(),
                format!(
                    "Could not load the Codex MCP server {}: {error}",
                    python_repr(source_id)
                ),
                false,
                "mcp_server",
            )),
        }
    }
    servers
}

/// Drops every server a source id is declared for more than once. Reference
/// `_remove_mcp_server_collisions`.
fn remove_mcp_server_collisions(
    servers: Vec<AdaptedMcpServer>,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Vec<AdaptedMcpServer> {
    let mut grouped: BTreeMap<String, Vec<AdaptedMcpServer>> = BTreeMap::new();
    for server in servers {
        grouped
            .entry(server.source_id.clone())
            .or_default()
            .push(server);
    }
    let mut selected = Vec::new();
    for (source_id, mut matches) in grouped {
        if matches.len() == 1 {
            selected.append(&mut matches);
            continue;
        }
        diagnostics.push(diagnostic(
            Severity::Error,
            "plugin.compatibility.codex.mcp_server_collision",
            matches[0].config_file.clone(),
            format!(
                "More than one configuration source declares the Codex MCP server {}, so every \
                 declaration of it is disabled.",
                python_repr(&source_id)
            ),
            false,
            "mcp_server",
        ));
    }
    selected
}

/// Reference `_codex_mcp_server`.
fn codex_mcp_server(
    root: &Path,
    data_root: &Path,
    source_id: &str,
    raw: &Value,
    config_path: &Path,
) -> Result<(PluginMcpServer, Vec<PluginAdapterDiagnostic>), String> {
    let Value::Object(map) = raw else {
        return Err("a server definition has to be an object".to_owned());
    };
    if map.contains_key("command") {
        match map.get("type") {
            None | Some(Value::Null) => {}
            Some(Value::String(kind)) if kind == "stdio" => {}
            Some(_) => return Err("type has to be stdio".to_owned()),
        }
        let command = match map.get("command") {
            Some(Value::String(command)) if !command.is_empty() => command.clone(),
            Some(Value::String(_)) => return Err("command cannot be empty".to_owned()),
            _ => return Err("command has to be a string".to_owned()),
        };
        let args = string_list(map, "args")?;
        let declared_env = string_map(map, "env")?;
        let cwd = optional_string(map, "cwd")?;
        let command = resolve_command(root, &command)?;
        let cwd = resolve_cwd(root, cwd.as_deref())?;
        let mut env: BTreeMap<String, String> = declared_env.into_iter().collect();
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
        require_server_name(source_id)?;
        let server = PluginMcpServer::Stdio {
            name: source_id.to_owned(),
            command: vec![command],
            args,
            env,
            cwd: Some(path_text(&cwd)),
        };
        let extras = extra_keys(map, &["type", "command", "args", "env", "cwd"]);
        return Ok((
            server,
            unsupported_field_diagnostics(extras, source_id, config_path),
        ));
    }

    match map.get("type") {
        Some(Value::String(kind)) if kind == "http" => {}
        None => return Err("type is required".to_owned()),
        Some(_) => return Err("type has to be http".to_owned()),
    }
    let url = match map.get("url") {
        Some(Value::String(url)) if !url.is_empty() => url.clone(),
        Some(Value::String(_)) => return Err("url cannot be empty".to_owned()),
        None => return Err("url is required".to_owned()),
        Some(_) => return Err("url has to be a string".to_owned()),
    };
    let headers = string_map(map, "headers")?;
    let bearer_token_env_var = optional_string(map, "bearer_token_env_var")?;
    let oauth_resource = optional_string(map, "oauth_resource")?;
    let headers: BTreeMap<String, String> = headers.into_iter().collect();
    let auth = if let Some(variable) = bearer_token_env_var {
        validate_static_headers(&headers)?;
        if !variable.is_empty() && !ENV_VAR.is_match(&variable) {
            return Err("bearer_token_env_var has to name an environment variable".to_owned());
        }
        Some(PluginMcpHttpAuth::BearerTokenEnv(variable))
    } else if oauth_resource.is_some() {
        Some(PluginMcpHttpAuth::OAuth)
    } else {
        validate_static_headers(&headers)?;
        None
    };
    let url = normalize_mcp_server_url(&url).map_err(|error| error.to_string())?;
    require_server_name(source_id)?;
    let server = match auth {
        None => PluginMcpServer::Http {
            name: source_id.to_owned(),
            transport: "http".to_owned(),
            url,
            headers,
        },
        // OAuth carries no static headers: the declared ones are dropped.
        Some(PluginMcpHttpAuth::OAuth) => PluginMcpServer::AuthenticatedHttp {
            name: source_id.to_owned(),
            url,
            headers: BTreeMap::new(),
            auth: PluginMcpHttpAuth::OAuth,
        },
        Some(auth) => PluginMcpServer::AuthenticatedHttp {
            name: source_id.to_owned(),
            url,
            headers,
            auth,
        },
    };
    let mut extras = extra_keys(
        map,
        &[
            "type",
            "url",
            "headers",
            "bearer_token_env_var",
            "oauth_resource",
        ],
    );
    if oauth_resource.is_some() {
        extras.insert("oauth_resource".to_owned());
    }
    Ok((
        server,
        unsupported_field_diagnostics(extras, source_id, config_path),
    ))
}

/// The keys a model allowing extras keeps outside its declared fields.
fn extra_keys(map: &Map<String, Value>, declared: &[&str]) -> BTreeSet<String> {
    map.keys()
        .filter(|key| !declared.contains(&key.as_str()))
        .cloned()
        .collect()
}

/// Reference `_unsupported_mcp_field_diagnostics`.
fn unsupported_field_diagnostics(
    mut extras: BTreeSet<String>,
    source_id: &str,
    config_path: &Path,
) -> Vec<PluginAdapterDiagnostic> {
    extras.remove("note");
    if extras.is_empty() {
        return Vec::new();
    }
    vec![diagnostic(
        Severity::Info,
        "plugin.compatibility.codex.mcp_metadata_partially_supported",
        config_path.to_path_buf(),
        format!(
            "The Codex MCP server {} declares {}, kept as package data with no runtime equivalent.",
            python_repr(source_id),
            extras.into_iter().collect::<Vec<_>>().join(", ")
        ),
        false,
        "mcp_server",
    )]
}

/// The name check `MCPStdio` and `MCPHttp` apply to their `name`.
fn require_server_name(source_id: &str) -> Result<(), String> {
    if normalize_mcp_server_name(source_id).is_empty() {
        return Err("the MCP server name needs a letter or a digit".to_owned());
    }
    Ok(())
}

/// Header validation `MCPStaticAuth` applies. Reference
/// `MCPStaticAuth._validate_headers`.
fn validate_static_headers(headers: &BTreeMap<String, String>) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for name in headers.keys() {
        if !HEADER_NAME.is_match(name) {
            return Err(format!(
                "{} is not a valid HTTP header name",
                python_repr(name)
            ));
        }
        if !seen.insert(name.to_lowercase()) {
            return Err(format!("the HTTP header {} is repeated", python_repr(name)));
        }
    }
    Ok(())
}

fn string_list(map: &Map<String, Value>, key: &str) -> Result<Vec<String>, String> {
    match map.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => Ok(text.clone()),
                _ => Err(format!("{key} has to be a list of strings")),
            })
            .collect(),
        Some(_) => Err(format!("{key} has to be a list of strings")),
    }
}

fn string_map(map: &Map<String, Value>, key: &str) -> Result<Vec<(String, String)>, String> {
    match map.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Object(entries)) => entries
            .iter()
            .map(|(name, item)| match item {
                Value::String(text) => Ok((name.clone(), text.clone())),
                _ => Err(format!("{key} has to map strings to strings")),
            })
            .collect(),
        Some(_) => Err(format!("{key} has to map strings to strings")),
    }
}

/// Reference `_resolve_command`.
fn resolve_command(root: &Path, command: &str) -> Result<String, String> {
    if let Some(relative) = command.strip_prefix("./") {
        let resolved = resolve_lax(&root.join(relative));
        if !is_relative_to(&resolved, root) {
            return Err("a stdio command has to resolve inside the plugin".to_owned());
        }
        return Ok(path_text(&resolved));
    }
    if command.contains('/') || command.contains('\\') || command == "." || command == ".." {
        return Err("a stdio command must be a bare executable or start with './'".to_owned());
    }
    Ok(command.to_owned())
}

/// Reference `_resolve_cwd`.
fn resolve_cwd(root: &Path, value: Option<&str>) -> Result<PathBuf, String> {
    let Some(value) = value.filter(|value| !matches!(*value, "." | "./")) else {
        return Ok(root.to_path_buf());
    };
    let relative = value.strip_prefix("./").unwrap_or(value);
    let parts = posix_parts(relative);
    if value.contains('\\') || relative.starts_with('/') || parts.contains(&"..") {
        return Err("a stdio cwd has to resolve inside the plugin".to_owned());
    }
    let resolved = resolve_lax(
        &parts
            .iter()
            .fold(root.to_path_buf(), |path, part| path.join(part)),
    );
    if !is_relative_to(&resolved, root) {
        return Err("a stdio cwd has to resolve inside the plugin".to_owned());
    }
    Ok(resolved)
}

/// Reference `CodexPluginAdapter._report_interface`.
fn report_interface(
    manifest_path: &Path,
    manifest: &Manifest,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) {
    let Some(interface) = &manifest.interface else {
        return;
    };
    if !interface.is_object() {
        diagnostics.push(diagnostic(
            Severity::Warning,
            "plugin.compatibility.codex.interface_metadata_invalid",
            manifest_path.to_path_buf(),
            "The Codex interface metadata has to be an object.",
            false,
            "interface",
        ));
    }
    diagnostics.push(diagnostic(
        Severity::Info,
        "plugin.compatibility.codex.interface_metadata_unsupported",
        manifest_path.to_path_buf(),
        "The Codex interface and branding metadata is kept as package data and published as no capability.",
        false,
        "interface",
    ));
    unsupported.push(unsupported_component(
        "interface_metadata",
        manifest_path.to_path_buf(),
        "codex_interface_metadata_runtime_private",
    ));
}

/// Reference `CodexPluginAdapter._report_apps`.
fn report_apps(
    root: &Path,
    manifest_path: &Path,
    manifest: &Manifest,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) {
    if manifest.apps.is_none() && !exists_or_link(&root.join(".app.json")) {
        return;
    }
    let path = match &manifest.apps {
        Some(declared) if !declared.is_string() => {
            diagnostics.push(diagnostic(
                Severity::Warning,
                "plugin.compatibility.codex.app_invalid",
                manifest_path.to_path_buf(),
                "The Codex Apps declaration has to be a relative path.",
                false,
                "app",
            ));
            manifest_path.to_path_buf()
        }
        declared => {
            let value = declared
                .as_ref()
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("./.app.json");
            let resolved = resolve_declared_path(root, value).and_then(|resolved| {
                if resolved.is_file() {
                    Ok(resolved)
                } else {
                    Err("the Codex Apps declaration has to resolve to a regular file".to_owned())
                }
            });
            match resolved {
                Ok(resolved) => resolved,
                Err(error) => {
                    diagnostics.push(diagnostic(
                        Severity::Warning,
                        "plugin.compatibility.codex.app_invalid",
                        manifest_path.to_path_buf(),
                        format!("Could not read the Codex Apps declaration: {error}"),
                        false,
                        "app",
                    ));
                    manifest_path.to_path_buf()
                }
            }
        }
    };
    diagnostics.push(diagnostic(
        Severity::Warning,
        "plugin.compatibility.codex.openai_app_unsupported",
        path.clone(),
        "OpenAI Apps and ChatGPT interface extensions cannot run in a headless session.",
        false,
        "app",
    ));
    unsupported.push(unsupported_component(
        "openai_app",
        path,
        "openai_apps_ui_unsupported",
    ));
}

/// Reference `CodexPluginAdapter._report_hooks`.
fn report_hooks(
    root: &Path,
    manifest_path: &Path,
    manifest: &Manifest,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) {
    let declared = manifest.hooks.as_ref();
    let Some(mut values) = declared_paths(declared) else {
        let inline = match declared {
            Some(Value::Object(_)) => true,
            Some(Value::Array(items)) => !items.is_empty() && items.iter().all(Value::is_object),
            _ => false,
        };
        if inline {
            diagnostics.push(diagnostic(
                Severity::Warning,
                "plugin.compatibility.codex.hooks_unsupported",
                manifest_path.to_path_buf(),
                "Inline Codex hooks do not run here; the plugin's other valid skills and MCP servers stay active.",
                false,
                "hook",
            ));
            unsupported.push(unsupported_component(
                "codex_hooks",
                manifest_path.to_path_buf(),
                "codex_hook_semantics_unsupported",
            ));
        } else {
            diagnostics.push(diagnostic(
                Severity::Warning,
                "plugin.compatibility.codex.hooks_invalid",
                manifest_path.to_path_buf(),
                "The Codex hooks declaration has to be a relative path, a list of relative paths, an \
                 object, or a list of objects.",
                false,
                "hook",
            ));
        }
        return;
    };
    if declared.is_none() && exists_or_link(&root.join("hooks.json")) {
        values = vec!["hooks.json".to_owned()];
    }
    for value in values {
        let resolved = resolve_declared_path(root, &value).and_then(|resolved| {
            if resolved.is_file() {
                Ok(resolved)
            } else {
                Err("Codex hooks have to resolve to a regular file".to_owned())
            }
        });
        match resolved {
            Ok(resolved) => {
                diagnostics.push(diagnostic(
                    Severity::Warning,
                    "plugin.compatibility.codex.hooks_unsupported",
                    resolved.clone(),
                    "Codex hooks do not run here; the plugin's other valid skills and MCP servers stay active.",
                    false,
                    "hook",
                ));
                unsupported.push(unsupported_component(
                    "codex_hooks",
                    resolved,
                    "codex_hook_semantics_unsupported",
                ));
            }
            Err(error) => diagnostics.push(diagnostic(
                Severity::Warning,
                "plugin.compatibility.codex.hooks_invalid",
                manifest_path.to_path_buf(),
                format!("Could not read the Codex hook configuration: {error}"),
                false,
                "hook",
            )),
        }
    }
}

/// Reference `CodexPluginAdapter._report_agent_metadata`.
fn report_agent_metadata(
    root: &Path,
    skill_roots: &[PathBuf],
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) {
    let mut candidates: BTreeSet<PathBuf> = BTreeSet::new();
    for skill_root in skill_roots {
        let Ok(entries) = std::fs::read_dir(skill_root) else {
            continue;
        };
        for entry in entries.flatten() {
            for filename in AGENT_METADATA_FILENAMES {
                let candidate = entry.path().join("agents").join(filename);
                if exists_or_link(&candidate) {
                    candidates.insert(candidate);
                }
            }
        }
    }
    let agents_root = root.join("agents");
    if agents_root.is_dir()
        && let Ok(entries) = std::fs::read_dir(&agents_root)
    {
        candidates.extend(
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_file()),
        );
    }
    for candidate in candidates {
        let resolved = resolve_strict(&candidate)
            .map_err(|error| error.to_string())
            .and_then(|resolved| {
                if is_relative_to(&resolved, root) && resolved.is_file() {
                    Ok(resolved)
                } else {
                    Err("agent metadata has to resolve inside the plugin".to_owned())
                }
            });
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                diagnostics.push(diagnostic(
                    Severity::Warning,
                    "plugin.path.outside_root",
                    candidate,
                    format!("Could not read the Codex agent metadata: {error}"),
                    false,
                    "agent_metadata",
                ));
                continue;
            }
        };
        if file_name(&candidate) == OPENAI_METADATA_FILENAME
            && read_text(&resolved)
                .ok()
                .and_then(|text| openai_metadata_has_unhandled_fields(&text))
                == Some(false)
        {
            continue;
        }
        diagnostics.push(diagnostic(
            Severity::Info,
            "plugin.compatibility.codex.agent_metadata_unsupported",
            resolved.clone(),
            "Codex agent and per-skill presentation metadata is kept as package data and becomes no capability.",
            false,
            "agent_metadata",
        ));
        unsupported.push(unsupported_component(
            "agent_metadata",
            resolved,
            "codex_agent_metadata_unsupported",
        ));
    }
}

/// Whether an `agents/openai.yaml` holds metadata Vibe keeps without
/// applying, or `None` when it does not parse. Reference
/// `parse_openai_skill_metadata` and `OpenAISkillMetadata.has_unhandled_fields`.
fn openai_metadata_has_unhandled_fields(text: &str) -> Option<bool> {
    let document = crate::skills::parser::yaml_document(text).ok()?;
    let mut mapping = match document {
        Value::Null => Map::new(),
        Value::Object(mapping) => mapping,
        _ => return None,
    };
    let policy = mapping.remove("policy");
    let has_extra = !mapping.is_empty();
    let products = match policy {
        None | Some(Value::Null) => false,
        Some(Value::Object(policy)) => {
            if policy
                .keys()
                .any(|key| key != "allow_implicit_invocation" && key != "products")
            {
                return None;
            }
            if !matches!(
                policy.get("allow_implicit_invocation"),
                None | Some(Value::Null | Value::Bool(_))
            ) {
                return None;
            }
            match policy.get("products") {
                None => false,
                Some(Value::Array(products)) if products.iter().all(Value::is_string) => {
                    !products.is_empty()
                }
                Some(_) => return None,
            }
        }
        Some(_) => return None,
    };
    Some(has_extra || products)
}

/// Reference `_declared_paths`: `None` for a value that is neither a string
/// nor a list of strings.
fn declared_paths(value: Option<&Value>) -> Option<Vec<String>> {
    match value {
        None => Some(Vec::new()),
        Some(Value::String(path)) => Some(vec![path.clone()]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect(),
        Some(_) => None,
    }
}

/// `root / PurePosixPath(value.removeprefix("./"))`.
fn posix_join(root: &Path, value: &str) -> PathBuf {
    let relative = value.strip_prefix("./").unwrap_or(value);
    let base = if relative.starts_with('/') {
        PathBuf::from("/")
    } else {
        root.to_path_buf()
    };
    posix_parts(relative)
        .into_iter()
        .fold(base, |path, part| path.join(part))
}

/// The parts `PurePosixPath` keeps: empty and `.` segments fold away.
fn posix_parts(value: &str) -> Vec<&str> {
    value
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect()
}

fn diagnostic(
    severity: Severity,
    code: &str,
    path: PathBuf,
    message: impl Into<String>,
    fatal: bool,
    component: &str,
) -> PluginAdapterDiagnostic {
    PluginAdapterDiagnostic {
        severity,
        code: code.to_owned(),
        path,
        message: message.into(),
        fatal,
        component: component.to_owned(),
    }
}

fn unsupported_component(kind: &str, path: PathBuf, reason: &str) -> AdaptedUnsupportedComponent {
    AdaptedUnsupportedComponent {
        kind: kind.to_owned(),
        path,
        reason: reason.to_owned(),
    }
}

fn exists_or_link(path: &Path) -> bool {
    path.exists()
        || std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod codex_tests;
