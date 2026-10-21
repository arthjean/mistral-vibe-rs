//! The MCP servers a native plugin declares in `mcp.json`, and the private
//! routing alias every plugin-owned server is renamed to. Reference
//! `PluginResolver._load_mcp_servers` and `_MCPConfiguration`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::super::compatibility::{
    PluginMcpServer, blake2s_hex, check_header_name, read_text, resolve_stdio_command,
    typescript_identifier,
};
use super::super::diagnostics::{self as codes, PluginConfigIssue, Severity};
use super::super::paths::{exists_or_link, is_relative_to, path_text, resolve_strict};
use super::super::strict::{self, ValidationErrors};
use super::{MCP_SCHEMA, PluginDescriptor, PluginMcpServerDefinition, Resolve};
use crate::platform::resolve_lenient;
use crate::pycompat;

#[expect(clippy::expect_used, reason = "compile-time constant pattern")]
static PLUGIN_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{PLUGIN_(?:ROOT|DATA)\}").expect("placeholder pattern compiles")
});

impl Resolve<'_> {
    pub(super) fn load_mcp_servers(
        &mut self,
        plugin: &PluginDescriptor,
    ) -> Vec<PluginMcpServerDefinition> {
        let config_path = plugin.root.join("mcp.json");
        if !exists_or_link(&config_path) {
            return Vec::new();
        }
        let loaded = resolve_strict(&config_path)
            .map_err(|error| error.to_string())
            .and_then(|resolved| {
                if !is_relative_to(&resolved, &plugin.root) || !resolved.is_file() {
                    return Err(
                        "mcp.json has to resolve to a regular file inside the plugin".to_owned(),
                    );
                }
                let text = read_text(&resolved).map_err(|error| error.to_string())?;
                let raw: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
                Ok((resolved, parse_mcp_configuration(&raw)?))
            });
        let (resolved_config, servers) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                self.issues.push(PluginConfigIssue::plain(
                    &config_path,
                    format!("Could not load the MCP configuration: {error}"),
                ));
                return Vec::new();
            }
        };
        let mut definitions = Vec::new();
        for (source_id, raw_server) in servers {
            let parsed = parse_mcp_server(&raw_server);
            let server = parsed.and_then(|parsed| {
                let transport = parsed.kind.clone();
                to_plugin_mcp_server(plugin, &source_id, parsed).map(|server| (server, transport))
            });
            match server {
                Err(error) => self.issues.push(PluginConfigIssue::plain(
                    &resolved_config,
                    format!(
                        "Could not load the MCP server {}: {error}",
                        pycompat::repr_str(&source_id)
                    ),
                )),
                Ok((None, transport)) => self.issues.push(PluginConfigIssue::plain(
                    &resolved_config,
                    format!(
                        "The MCP server {} uses the unsupported transport {}",
                        pycompat::repr_str(&source_id),
                        pycompat::repr_str(&transport)
                    ),
                )),
                Ok((Some(server), _)) => definitions.push(plugin_mcp_server(
                    plugin,
                    source_id,
                    &server,
                    resolved_config.clone(),
                )),
            }
        }
        definitions
    }

    pub(super) fn remove_config_shadowed_mcp_servers(
        &mut self,
        definitions: Vec<PluginMcpServerDefinition>,
        plugins: &BTreeMap<String, PluginDescriptor>,
    ) -> Vec<PluginMcpServerDefinition> {
        let Some(configured) = &self.resolver.configured_mcp_names else {
            return definitions;
        };
        let mut selected = Vec::new();
        for definition in definitions {
            if !configured.contains(&definition.source_id) {
                selected.push(definition);
                continue;
            }
            self.issues.push(PluginConfigIssue {
                file: definition.config_file.clone(),
                message: format!(
                    "The MCP server {} of plugin {} is disabled because a configured MCP server \
                     already has that name and the two would share credentials; rename one of them",
                    pycompat::repr_str(&definition.source_id),
                    pycompat::repr_str(&definition.plugin_name),
                ),
                severity: Severity::Warning,
                code: Some(codes::MCP_SERVER_SHADOWED.to_owned()),
                fatal: false,
                source_format: plugins
                    .get(&definition.plugin_name)
                    .map(|plugin| plugin.source_format),
                component: Some("mcp_server".to_owned()),
            });
        }
        selected
    }
}

/// `server` as `plugin` owns it: renamed to its private routing alias.
pub(super) fn plugin_mcp_server(
    plugin: &PluginDescriptor,
    source_id: String,
    server: &PluginMcpServer,
    config_file: PathBuf,
) -> PluginMcpServerDefinition {
    let private_alias = private_server_alias(&plugin.name, &source_id);
    PluginMcpServerDefinition {
        plugin_name: plugin.name.clone(),
        plugin_namespace: plugin.namespace.clone(),
        source_id,
        server: server.with_name(&private_alias),
        private_alias,
        config_file,
    }
}

/// The private routing alias of a plugin-owned MCP server: a BLAKE2s digest of
/// the plugin name and the source id, then the identifier-safe source id.
/// Reference `_private_server_alias`.
pub(super) fn private_server_alias(plugin_name: &str, source_id: &str) -> String {
    let identity = format!("{plugin_name}\0{source_id}");
    let digest = blake2s_hex(identity.as_bytes(), 8);
    let source = typescript_identifier(if source_id.is_empty() {
        "server"
    } else {
        source_id
    });
    let source: String = source.chars().take(80).collect();
    format!("plugin_{digest}_{source}")
}

/// One validated `mcpServers` entry. Reference `_MCPStdioServer`,
/// `_MCPStreamableHTTPServer` and `_MCPSSEServer`.
struct ParsedMcpServer {
    kind: String,
    command: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<String>,
    url: String,
    headers: Vec<(String, String)>,
}

fn parse_mcp_configuration(raw: &Value) -> Result<Vec<(String, Value)>, String> {
    let mut errors = ValidationErrors::default();
    let allowed = ["$schema", "schema_id", "mcpServers", "mcp_servers"];
    let Some(map) = strict::object(raw, "", &allowed, &mut errors) else {
        return Err(errors.render("MCP configuration"));
    };
    match map.get("$schema").or_else(|| map.get("schema_id")) {
        Some(Value::String(schema)) if schema == MCP_SCHEMA => {}
        None => errors.push("$schema", "is required"),
        Some(_) => errors.push("$schema", "must name the Agent Plugins 1.0 MCP schema"),
    }
    let servers = match map.get("mcpServers").or_else(|| map.get("mcp_servers")) {
        None => {
            errors.push("mcpServers", "is required");
            None
        }
        Some(Value::Object(servers)) => Some(
            servers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        ),
        Some(_) => {
            errors.push("mcpServers", "must be an object");
            None
        }
    };
    match servers {
        Some(servers) if errors.is_empty() => Ok(servers),
        _ => Err(errors.render("MCP configuration")),
    }
}

fn parse_mcp_server(raw: &Value) -> Result<ParsedMcpServer, String> {
    let mut errors = ValidationErrors::default();
    let kind = match raw {
        Value::Object(map) => match map.get("type") {
            Some(Value::String(kind))
                if ["stdio", "streamable-http", "sse"].contains(&kind.as_str()) =>
            {
                kind.clone()
            }
            Some(_) => {
                return Err("the server type must be stdio, streamable-http or sse".to_owned());
            }
            None => return Err("the server declares no type".to_owned()),
        },
        _ => return Err("a server must be an object".to_owned()),
    };
    let allowed: &[&str] = if kind == "stdio" {
        &["type", "command", "args", "env", "cwd"]
    } else {
        &["type", "url", "headers"]
    };
    let Some(map) = strict::object(raw, "", allowed, &mut errors) else {
        return Err(errors.render("MCP server"));
    };
    let mut parsed = ParsedMcpServer {
        kind: kind.clone(),
        command: String::new(),
        args: Vec::new(),
        env: Vec::new(),
        cwd: None,
        url: String::new(),
        headers: Vec::new(),
    };
    if kind == "stdio" {
        if let Some(command) = strict::required_string(map, "", "command", &mut errors) {
            if command.is_empty() {
                errors.push("command", "cannot be empty");
            }
            parsed.command = command;
        }
        if let Some(args) = strict::string_list(map, "", "args", false, &mut errors) {
            parsed.args = args.unwrap_or_default();
        }
        if let Some(env) = strict::string_map(map, "", "env", &mut errors) {
            let env = env.unwrap_or_default();
            let mut collision: Vec<&str> = env
                .iter()
                .map(|(name, _)| name.as_str())
                .filter(|name| {
                    let reserved = ["PLUGIN_ROOT", "PLUGIN_DATA"];
                    reserved.contains(name)
                        || (cfg!(windows) && reserved.contains(&name.to_uppercase().as_str()))
                })
                .collect();
            collision.sort_unstable();
            if !collision.is_empty() {
                errors.push(
                    "env",
                    format!(
                        "cannot define the reserved variables {}",
                        collision.join(", ")
                    ),
                );
            }
            parsed.env = env;
        }
        if let Some(cwd) = strict::optional_string(map, "", "cwd", &mut errors) {
            parsed.cwd = cwd;
        }
    } else {
        if let Some(url) = strict::required_string(map, "", "url", &mut errors) {
            if url.is_empty() {
                errors.push("url", "cannot be empty");
            }
            parsed.url = url;
        }
        if let Some(headers) = strict::string_map(map, "", "headers", &mut errors) {
            let headers = headers.unwrap_or_default();
            if let Err(error) = validate_http_headers(&headers) {
                errors.push("headers", error);
            }
            parsed.headers = headers;
        }
    }
    if errors.is_empty() {
        Ok(parsed)
    } else {
        Err(errors.render("MCP server"))
    }
}

fn validate_http_headers(headers: &[(String, String)]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for (name, value) in headers {
        check_header_name(name, &mut seen)?;
        if value.chars().any(|character| {
            character != '\t'
                && (u32::from(character) < 0x20
                    || u32::from(character) == 0x7f
                    || u32::from(character) > 0xff)
        }) {
            return Err(format!(
                "the value of the HTTP header {} is invalid",
                pycompat::repr_str(name)
            ));
        }
    }
    Ok(())
}

fn to_plugin_mcp_server(
    plugin: &PluginDescriptor,
    source_id: &str,
    parsed: ParsedMcpServer,
) -> Result<Option<PluginMcpServer>, String> {
    let private_alias = private_server_alias(&plugin.name, source_id);
    match parsed.kind.as_str() {
        "stdio" => {
            let command = resolve_stdio_command(&plugin.root, &parsed.command)?;
            let args = parsed
                .args
                .iter()
                .map(|value| expand_plugin_variables(value, &plugin.root, &plugin.data_root))
                .collect();
            let mut env: BTreeMap<String, String> = parsed
                .env
                .iter()
                .map(|(name, value)| {
                    (
                        name.clone(),
                        expand_plugin_variables(value, &plugin.root, &plugin.data_root),
                    )
                })
                .collect();
            env.insert("PLUGIN_ROOT".to_owned(), path_text(&plugin.root));
            env.insert("PLUGIN_DATA".to_owned(), path_text(&plugin.data_root));
            let cwd = resolve_plugin_cwd(plugin, parsed.cwd.as_deref())?;
            Ok(Some(PluginMcpServer::Stdio {
                name: private_alias,
                command: vec![command],
                args,
                env,
                cwd: Some(path_text(&cwd)),
            }))
        }
        "streamable-http" => {
            let url = crate::config::mcp::normalize_mcp_server_url(&parsed.url)
                .map_err(|error| error.to_string())?;
            Ok(Some(PluginMcpServer::Http {
                name: private_alias,
                transport: "streamable-http".to_owned(),
                url,
                headers: parsed.headers.into_iter().collect(),
            }))
        }
        _ => Ok(None),
    }
}

fn expand_plugin_variables(value: &str, plugin_root: &Path, data_root: &Path) -> String {
    PLUGIN_PLACEHOLDER
        .replace_all(value, |captures: &regex::Captures<'_>| {
            if &captures[0] == "${PLUGIN_ROOT}" {
                path_text(plugin_root)
            } else {
                path_text(data_root)
            }
        })
        .into_owned()
}

fn resolve_plugin_cwd(plugin: &PluginDescriptor, value: Option<&str>) -> Result<PathBuf, String> {
    let Some(value) = value else {
        return Ok(plugin.root.clone());
    };
    let expanded = expand_plugin_variables(value, &plugin.root, &plugin.data_root);
    let (expected_root, candidate) = if let Some(relative) = value.strip_prefix("./") {
        (plugin.root.clone(), plugin.root.join(relative))
    } else if value == "${PLUGIN_ROOT}" || value.starts_with("${PLUGIN_ROOT}/") {
        (plugin.root.clone(), PathBuf::from(&expanded))
    } else if value == "${PLUGIN_DATA}" || value.starts_with("${PLUGIN_DATA}/") {
        (plugin.data_root.clone(), PathBuf::from(&expanded))
    } else {
        return Err(
            "a stdio cwd has to start with './', '${PLUGIN_ROOT}' or '${PLUGIN_DATA}'".to_owned(),
        );
    };
    let resolved = resolve_lenient(&candidate);
    if !is_relative_to(&resolved, &resolve_lenient(&expected_root)) {
        return Err("the stdio cwd resolves outside the root it names".to_owned());
    }
    Ok(resolved)
}
