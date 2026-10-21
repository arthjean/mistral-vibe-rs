//! The tool groups a unified session's host provides: one per MCP server and
//! one per connector, which a `run_typescript` program reaches as
//! `tools.<group>.<tool>` and the model never calls directly.
//!
//! Reference `_integration_tool_groups`
//! (`harness/runtimes/python/python/mistralai_vibe_local_harness/vibe/_host.py`)
//! publishes them with `exposure="programmatic"`, `build_route_snapshot`
//! (`_mcp_naming.py`) and `build_connector_snapshot` (`_connector_naming.py`)
//! name them, and `execute_provided_tool_action` (`_provided_tool_actions.py`)
//! maps what a call returns. A call runs through the session's own registry,
//! the same proxy the legacy backend runs, once the unified gate let it
//! through.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use vibe_core::events::RemoteToolKind;
use vibe_core::mcp::{DefaultMcpPeerFactory, McpPeerFactory, McpServerConfig};
use vibe_core::tools::{
    ToolApproval, ToolApprovalSource, ToolApprovalType, ToolAvailability, ToolExecutionOutput,
    ToolInvocation, ToolRegistry, ToolVerdict,
};
use vibe_harness::programmatic::{Callable, ProvidedTools};
use vibe_harness::state::ToolResult;

use crate::plugins::SessionPlugins;

/// Reference `execute_provided_tool_action`: what a call that failed before
/// its tool answered tells the model.
const CALL_FAILED: &str = "The provided tool call failed";

/// The block types an MCP result may carry, reference `_normalize_result`.
const CONTENT_TYPES: [&str; 5] = ["text", "image", "audio", "resource_link", "resource"];

/// The provided groups of one session, and what runs their calls.
pub(crate) struct Provided {
    pub(crate) callables: Vec<Callable>,
    pub(crate) routes: Arc<ProvidedRoutes>,
}

/// Each `group.tool` route a session's provided groups offer, with the tool
/// it reaches.
pub(crate) struct ProvidedRoutes {
    registry: Option<ToolRegistry>,
    routes: BTreeMap<String, Route>,
}

#[derive(Clone)]
struct Route {
    /// The name the session publishes and configures the tool under.
    published: String,
    kind: RemoteToolKind,
    target: Target,
}

/// What runs a route's calls.
#[derive(Clone)]
enum Target {
    /// The session's registry, under the route's published name.
    Registry,
    /// A plugin's server, which the session's registry does not hold:
    /// reference `merge_plugin_mcp_into_catalog` runs it beside the
    /// configured ones under its catalog name.
    Plugin {
        config: Arc<McpServerConfig>,
        remote: String,
    },
}

/// One tool a server or connector publishes.
struct Remote {
    published: String,
    remote: String,
    target: Target,
    description: String,
    hint: Option<String>,
    input: Value,
}

impl ProvidedRoutes {
    /// The name the tool `route` reaches is published under, which is the
    /// name its permission is configured under: reference
    /// `ProvidedToolNames.resolve`.
    pub(crate) fn published(&self, route: &str) -> Option<&str> {
        self.routes.get(route).map(|route| route.published.as_str())
    }
}

impl ProvidedTools for ProvidedRoutes {
    fn routes(&self, route: &str) -> bool {
        self.routes.contains_key(route)
    }

    fn call(&self, route: &str, arguments: Value) -> BoxFuture<'static, ToolResult> {
        let target = self.routes.get(route).cloned();
        let registry = self.registry.clone();
        Box::pin(async move {
            let Some(target) = target else {
                return ToolResult::failed("mcp_unknown_route", CALL_FAILED);
            };
            let codes = Codes::of(target.kind);
            let invoked = match (&target.target, registry) {
                (Target::Plugin { config, remote }, _) => {
                    call_plugin(config, remote, arguments).await
                }
                (Target::Registry, Some(registry)) => {
                    let invocation = ToolInvocation {
                        call_id: String::new(),
                        arguments,
                    };
                    // The unified gate already decided the call; the
                    // registry's guard takes its answer instead of asking
                    // again.
                    let approval = ToolApproval {
                        decision: ToolVerdict::Execute,
                        approval_type: ToolApprovalType::Ask,
                        approval_source: ToolApprovalSource::User,
                    };
                    vibe_core::tools::run_gated(
                        approval,
                        registry.invoke(&target.published, invocation),
                    )
                    .await
                    .map_err(|error| error.to_string())
                }
                (Target::Registry, None) => {
                    return ToolResult::failed("mcp_unknown_route", CALL_FAILED);
                }
            };
            match invoked {
                Ok(output) if output.failure.is_none() && output.skip.is_none() => {
                    match output.remote_result {
                        Some(result) => normalized(&result, codes),
                        None => ToolResult::Success {
                            content: vec![json!({"type": "text", "text": output.model_text})],
                            structured_content: None,
                            meta: None,
                        },
                    }
                }
                Ok(output) => failed(codes.failure(output.failure.as_deref().unwrap_or(""))),
                Err(error) => failed(codes.failure(&error)),
            }
        })
    }
}

/// One call to a plugin's server, over a connection of its own.
async fn call_plugin(
    config: &McpServerConfig,
    remote: &str,
    arguments: Value,
) -> Result<ToolExecutionOutput, String> {
    let peer = DefaultMcpPeerFactory::default()
        .connect(config)
        .await
        .map_err(|error| error.to_string())?;
    let headers = vibe_core::mcp::authorization::http_headers(config);
    let answer = peer.call(remote, arguments, &headers).await;
    let _ = peer.close().await;
    answer.map_err(|error| error.to_string())
}

/// The failure codes one kind of provider reports.
#[derive(Clone, Copy)]
struct Codes {
    prefix: &'static str,
    tool_error: &'static str,
}

impl Codes {
    const fn of(kind: RemoteToolKind) -> Self {
        match kind {
            RemoteToolKind::Mcp => Self {
                prefix: "mcp",
                tool_error: "The MCP tool returned an error",
            },
            RemoteToolKind::Connector => Self {
                prefix: "connector",
                tool_error: "The connector tool returned an error",
            },
        }
    }

    /// The code a call that never got a result fails with, read off the
    /// error the proxy raised: a deadline it ran out of, or the transport.
    fn failure(self, message: &str) -> (String, bool) {
        let lowered = message.to_lowercase();
        if lowered.contains("timed out") || lowered.contains("timeout") {
            (format!("{}_timeout", self.prefix), true)
        } else if self.prefix == "connector" {
            ("connector_gateway_failed".to_owned(), false)
        } else {
            ("mcp_transport_failed".to_owned(), false)
        }
    }
}

fn failed((code, retryable): (String, bool)) -> ToolResult {
    ToolResult::Failure {
        content: Vec::new(),
        code,
        message: CALL_FAILED.to_owned(),
        details: None,
        retryable,
        meta: None,
    }
}

/// Reference `_normalize_result` then `execute_provided_tool_action`.
fn normalized(result: &Value, codes: Codes) -> ToolResult {
    let invalid = || failed((format!("{}_invalid_result", codes.prefix), false));
    let content = match result.get("content") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(blocks)) => blocks.clone(),
        Some(_) => return invalid(),
    };
    let known = content.iter().all(|block| {
        block
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| CONTENT_TYPES.contains(&kind))
    });
    if !known {
        return invalid();
    }
    let meta = match result.get("_meta") {
        None | Some(Value::Null) => None,
        Some(Value::Object(meta)) => Some(meta.clone()),
        Some(_) => return invalid(),
    };
    let is_error = match result.get("isError") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return invalid(),
    };
    let structured_content = result
        .get("structuredContent")
        .filter(|structured| !structured.is_null())
        .cloned();
    if is_error {
        return ToolResult::Failure {
            content,
            code: format!("{}_tool_error", codes.prefix),
            message: codes.tool_error.to_owned(),
            details: None,
            retryable: false,
            meta,
        };
    }
    ToolResult::Success {
        content,
        structured_content,
        meta,
    }
}

/// The groups `registry` provides for the tools `available` names: MCP
/// servers in name order, then connectors in alias order, each tool in its
/// remote name's order.
pub(crate) fn provided(
    registry: Option<&ToolRegistry>,
    available: &[String],
    plugins: Option<&SessionPlugins>,
) -> Provided {
    let mut mcp: BTreeMap<String, Vec<Remote>> = BTreeMap::new();
    let mut connectors: BTreeMap<String, Vec<Remote>> = BTreeMap::new();
    let specs = registry
        .and_then(|registry| registry.list().ok())
        .unwrap_or_default();
    for spec in specs {
        if spec.availability != ToolAvailability::Available || !available.contains(&spec.name) {
            continue;
        }
        let Some(origin) = registry.and_then(|registry| registry.remote_origin(&spec.name)) else {
            continue;
        };
        if origin.provider.is_empty() {
            continue;
        }
        let (description, hint) = registered_description(&origin.provider, &spec.description);
        let remote = Remote {
            published: spec.name.clone(),
            remote: origin.remote_name.clone(),
            target: Target::Registry,
            description,
            hint,
            input: spec.input_schema.clone(),
        };
        match origin.kind {
            RemoteToolKind::Mcp => mcp.entry(origin.provider).or_default().push(remote),
            RemoteToolKind::Connector => {
                connectors.entry(origin.provider).or_default().push(remote)
            }
        }
    }
    if let Some(plugins) = plugins {
        plugin_remotes(plugins, &mut mcp);
    }
    let mut callables = Vec::new();
    let mut routes = BTreeMap::new();
    let mut used = Vec::new();
    let groups: Vec<(RemoteToolKind, String, Vec<Remote>)> = mcp
        .into_iter()
        .map(|(provider, tools)| (RemoteToolKind::Mcp, provider, tools))
        .chain(
            connectors
                .into_iter()
                .map(|(provider, tools)| (RemoteToolKind::Connector, provider, tools)),
        )
        .collect();
    let candidates = groups
        .iter()
        .map(|(kind, provider, _)| group_candidate(*kind, provider.as_str()))
        .collect::<Vec<_>>();
    for (index, (kind, provider, mut tools)) in groups.into_iter().enumerate() {
        let candidate = &candidates[index];
        let shared = candidates
            .iter()
            .filter(|other| *other == candidate)
            .count()
            > 1;
        let group = if !shared && !used.contains(candidate) {
            candidate.clone()
        } else {
            // The reference salts a connector's digest with its raw id, which
            // the registry does not keep; its alias is as unique.
            let salt: &[u8] = match kind {
                RemoteToolKind::Mcp => b"configured\0",
                RemoteToolKind::Connector => b"connector\0",
            };
            unique_digest_name(candidate, &digest(salt, &provider), &used)
        };
        used.push(group.clone());
        tools.sort_by(|left, right| left.remote.cmp(&right.remote));
        let names = programmatic_names(tools.iter().map(|tool| tool.remote.as_str()));
        let guidance = match kind {
            RemoteToolKind::Mcp => tools
                .iter()
                .find_map(|tool| tool.hint.clone())
                .unwrap_or_else(|| format!("Tools from the MCP server {provider}.")),
            RemoteToolKind::Connector => format!("Tools from the {provider} connector."),
        };
        for tool in tools {
            let name = names.get(&tool.remote).cloned().unwrap_or_default();
            let route = format!("{group}.{name}");
            routes.insert(
                route.clone(),
                Route {
                    published: tool.published,
                    kind,
                    target: tool.target,
                },
            );
            callables.push(Callable {
                group: Cow::Owned(group.clone()),
                name: Cow::Owned(name),
                description: Cow::Owned(tool.description),
                input: tool.input,
                output: Value::Null,
                runtime: Cow::Owned(route),
                guidance: Some(guidance.clone()),
            });
        }
    }
    Provided {
        callables,
        routes: Arc::new(ProvidedRoutes {
            registry: registry.cloned(),
            routes,
        }),
    }
}

/// The tools of each plugin server the session reached, under its catalog
/// name, unless a configured server already holds that name.
fn plugin_remotes(plugins: &SessionPlugins, mcp: &mut BTreeMap<String, Vec<Remote>>) {
    let configs = crate::plugins::plugin_server_configs(plugins);
    for source in &plugins.mcp_sources {
        let Some(connection) = source
            .connection
            .as_ref()
            .filter(|connection| connection.status == "enabled")
        else {
            continue;
        };
        let Some(config) = configs.get(&source.name) else {
            continue;
        };
        if mcp.contains_key(&source.name) {
            continue;
        }
        let config = Arc::new(config.clone());
        let tools = connection
            .tools
            .iter()
            .map(|(name, description)| {
                let display = format!("{}_{name}", source.name);
                Remote {
                    published: if is_identifier(&display) {
                        display
                    } else {
                        normalize_identifier(&display)
                    },
                    remote: name.clone(),
                    target: Target::Plugin {
                        config: Arc::clone(&config),
                        remote: name.clone(),
                    },
                    description: description.clone().unwrap_or_default(),
                    hint: None,
                    input: connection
                        .schemas
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                }
            })
            .collect();
        mcp.insert(source.name.clone(), tools);
    }
}

/// The tool's own description and its server's hint, out of the description
/// the registry publishes (`[alias] description`, then `\nHint: prompt` for
/// an MCP server that sets one).
fn registered_description(provider: &str, published: &str) -> (String, Option<String>) {
    let bare = published
        .strip_prefix(&format!("[{provider}] "))
        .unwrap_or(published);
    match bare.rsplit_once("\nHint: ") {
        Some((description, hint)) if !hint.is_empty() => {
            (description.to_owned(), Some(hint.to_owned()))
        }
        _ => (bare.to_owned(), None),
    }
}

fn group_candidate(kind: RemoteToolKind, provider: &str) -> String {
    match kind {
        RemoteToolKind::Mcp => format!("mcp_{}", normalize_segment(provider)),
        RemoteToolKind::Connector => format!("connector_{}", normalize_segment(provider)),
    }
}

/// Reference `_resolve_normalized_names` over the sorted remote names: a name
/// that is already an identifier keeps itself, a normalized one that no other
/// name shares takes its normal form, and the rest add a digest.
fn programmatic_names<'a>(remotes: impl Iterator<Item = &'a str>) -> BTreeMap<String, String> {
    let mut raw: Vec<&str> = remotes.collect();
    raw.sort_unstable();
    raw.dedup();
    let candidates: Vec<String> = raw.iter().map(|name| normalize_identifier(name)).collect();
    let mut used: Vec<String> = Vec::new();
    let mut names = BTreeMap::new();
    for (index, name) in raw.iter().enumerate() {
        let candidate = &candidates[index];
        let resolved = if is_identifier(name) && *name == candidate {
            (*name).to_owned()
        } else if candidates
            .iter()
            .filter(|other| *other == candidate)
            .count()
            == 1
            && !used.contains(candidate)
        {
            candidate.clone()
        } else {
            unique_digest_name(candidate, &digest(b"", name), &used)
        };
        used.push(resolved.clone());
        names.insert((*name).to_owned(), resolved);
    }
    names
}

fn identifier_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_' || character == '$'
}

fn normalize_segment(value: &str) -> String {
    let normalized: String = value
        .chars()
        .map(|character| {
            if identifier_character(character) {
                character
            } else {
                '_'
            }
        })
        .collect();
    if normalized.is_empty() {
        "_".to_owned()
    } else {
        normalized
    }
}

fn normalize_identifier(value: &str) -> String {
    let normalized = normalize_segment(value);
    if normalized.starts_with(|character: char| character.is_ascii_digit()) {
        format!("_{normalized}")
    } else {
        normalized
    }
}

fn is_identifier(value: &str) -> bool {
    value
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_' || first == '$')
        && value.chars().all(identifier_character)
}

fn digest(salt: &[u8], value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(value.as_bytes());
    vibe_core::text::hex_encode(&hasher.finalize())
}

fn unique_digest_name(candidate: &str, digest: &str, used: &[String]) -> String {
    (8..=digest.len())
        .map(|length| format!("{candidate}_{}", &digest[..length]))
        .find(|name| !used.contains(name))
        .unwrap_or_else(|| format!("{candidate}_{digest}"))
}

#[cfg(test)]
#[path = "provided_tests.rs"]
mod provided_tests;
