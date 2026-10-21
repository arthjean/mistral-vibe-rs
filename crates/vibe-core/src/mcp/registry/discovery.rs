//! One discovery over the whole configuration: which servers are retired,
//! which answer from a cache, which have to be reached, and what each one
//! publishes.

use futures_util::future::join_all;

use super::credentials::{Authorized, authorized, publish_required, resolve_authorization};
use super::tools::healthy_view;
use super::*;
use crate::mcp::descriptor_cache::{self, descriptor_cache_key};

/// What one server's discovery came to.
enum Outcome {
    Disabled,
    Cached(Vec<RemoteTool>),
    Discovered(Vec<RemoteTool>),
    /// Why the server publishes nothing, and the status its view reports.
    Unavailable(McpServerStatus, McpError),
}

/// How far one server's discovery got: settled without reaching the server,
/// or `R`, what reaching it takes and then what it answered.
enum Step<R> {
    Settled(Outcome),
    Reach(R),
}

/// A server whose descriptors have to be fetched.
struct Pending {
    peer: Arc<dyn McpPeer>,
    snapshot: McpAuthorizationSnapshot,
    reference: McpAuthorizationRef,
    key: String,
}

impl McpRegistry {
    /// Reference `get_tools_async` over the whole configuration.
    pub(super) async fn discover_locked(
        &self,
        configs: Vec<McpServerConfig>,
        runtime: &McpRuntime,
    ) -> Vec<String> {
        let mut diagnostics = Vec::new();
        let (replaced, setup, forced) = self.sync_active_servers(&configs).await;
        for alias in replaced {
            diagnostics.extend(self.retire_alias(&alias).await);
        }
        let setup = setup.as_ref();
        let now = UtcTimestamp::now();
        let mut steps = Vec::with_capacity(configs.len());
        for config in &configs {
            let forced = forced.contains(&config.alias);
            steps.push(self.plan(config, setup, forced, now, runtime).await);
        }
        let reached = join_all(steps.into_iter().map(|step| async move {
            let pending = match step {
                Step::Settled(outcome) => return Step::Settled(outcome),
                Step::Reach(pending) => pending,
            };
            // Reference `_discover_server`.
            let peer = &pending.peer;
            let discover = |headers: BTreeMap<String, String>| -> McpFuture<'_, Vec<RemoteTool>> {
                Box::pin(async move { peer.discover(&headers).await })
            };
            let answer = authorized(
                setup,
                &pending.reference,
                pending.snapshot.clone(),
                discover,
            )
            .await;
            Step::Reach((pending, answer))
        }))
        .await;
        let mut outcomes = Vec::with_capacity(configs.len());
        for (config, step) in configs.iter().zip(reached) {
            outcomes.push(match step {
                Step::Settled(outcome) => outcome,
                Step::Reach((pending, answer)) => {
                    self.settle(&config.alias, &pending, answer, setup).await
                }
            });
        }
        // Reference `get_tools_async` answers the servers served from a cache
        // first and the ones it had to discover after them, each group in
        // configuration order, and that is the order the tools publish in.
        let (cached, rest): (Vec<_>, Vec<_>) = configs
            .into_iter()
            .zip(outcomes)
            .partition(|(_, outcome)| matches!(outcome, Outcome::Cached(_)));
        let mut published = Vec::new();
        for (config, outcome) in cached.into_iter().chain(rest) {
            diagnostics.extend(self.apply(config, outcome, runtime, &mut published).await);
        }
        self.state.lock().await.published_tools = published;
        diagnostics
    }

    /// Reference `sync_active_servers`, plus retiring what a changed or a
    /// removed entry published: the aliases to retire, the setup the discovery
    /// resolves authorizations under, and the servers it must reach whatever
    /// is cached.
    async fn sync_active_servers(
        &self,
        configs: &[McpServerConfig],
    ) -> (Vec<String>, Option<AuthorizationSetup>, BTreeSet<String>) {
        let aliases = configs
            .iter()
            .map(|config| config.alias.as_str())
            .collect::<BTreeSet<_>>();
        let oauth = configs
            .iter()
            .filter(|config| authorization_kind(config) == McpAuthorizationKind::Oauth)
            .map(|config| config.alias.as_str())
            .collect::<BTreeSet<_>>();
        let mut state = self.state.lock().await;
        let mut replaced = Vec::new();
        let mut forced = BTreeSet::new();
        for (alias, server) in &mut state.servers {
            server.needs_auth &= oauth.contains(alias.as_str());
            server.force_refresh &= aliases.contains(alias.as_str());
            if server.force_refresh {
                forced.insert(alias.clone());
            }
            if server.published.is_some() && !aliases.contains(alias.as_str()) {
                server.memory.clear();
                server.descriptor_revision = None;
                replaced.push(alias.clone());
            }
        }
        replaced.extend(
            configs
                .iter()
                .filter(|config| {
                    state
                        .published(&config.alias)
                        .is_some_and(|previous| previous.config != **config)
                })
                .map(|config| config.alias.clone()),
        );
        (replaced, state.authorization.clone(), forced)
    }

    /// Settles one server from what is known without reaching it, or names
    /// what reaching it takes.
    async fn plan(
        &self,
        config: &McpServerConfig,
        setup: Option<&AuthorizationSetup>,
        forced: bool,
        now: UtcTimestamp,
        runtime: &McpRuntime,
    ) -> Step<Pending> {
        if let Err(error) = validate_config(config) {
            return Step::Settled(Outcome::Unavailable(McpServerStatus::Failed, error));
        }
        let alias = config.alias.as_str();
        if !config.enabled && !forced {
            let mut state = self.state.lock().await;
            let server = state.server(alias);
            if let Some(reference) = setup.and_then(|setup| setup.references.get(alias)) {
                server.descriptor_revision = Some(reference.descriptor_revision.clone());
            }
            server.needs_auth = false;
            return Step::Settled(Outcome::Disabled);
        }
        let (snapshot, reference) = match resolve_authorization(config, setup).await {
            Ok(resolved) => resolved,
            Err(required) => {
                publish_required(&self.state, setup, alias, &required).await;
                return Step::Settled(Outcome::Unavailable(
                    McpServerStatus::AuthRequired,
                    McpError::AuthRequired,
                ));
            }
        };
        let key =
            descriptor_cache_key(&reference.server_fingerprint, &snapshot.descriptor_revision);
        {
            let mut state = self.state.lock().await;
            state.server(alias).descriptor_revision = Some(snapshot.descriptor_revision.clone());
            if !forced {
                if let Some(descriptors) = memory_hit(&mut state, &key, alias, now) {
                    return Step::Settled(Outcome::Cached(descriptors));
                }
                if let Some(record) = state
                    .cache
                    .clone()
                    .and_then(|cache| cache.read(&key, alias, now))
                {
                    let server = state.server(alias);
                    server.memory.insert(
                        key,
                        MemoryRecord {
                            discovered_at: record.discovered_at,
                            descriptors: record.descriptors.clone(),
                        },
                    );
                    server.needs_auth = false;
                    return Step::Settled(Outcome::Cached(record.descriptors));
                }
            }
        }
        match self.peer_for(config, runtime).await {
            Ok(peer) => Step::Reach(Pending {
                peer,
                snapshot,
                reference,
                key,
            }),
            Err(error) => Step::Settled(Outcome::Unavailable(McpServerStatus::Failed, error)),
        }
    }

    /// Records what reaching one server answered.
    async fn settle(
        &self,
        alias: &str,
        pending: &Pending,
        answer: Authorized<Vec<RemoteTool>>,
        setup: Option<&AuthorizationSetup>,
    ) -> Outcome {
        match answer {
            Authorized::Done(descriptors, effective) => {
                let effective_key = descriptor_cache_key(
                    &pending.reference.server_fingerprint,
                    &effective.descriptor_revision,
                );
                let now = UtcTimestamp::now();
                let mut state = self.state.lock().await;
                if let Some(cache) = &state.cache {
                    cache.write(&effective_key, alias, &descriptors, now);
                }
                let server = state.server(alias);
                server.descriptor_revision = Some(effective.descriptor_revision);
                if pending.key != effective_key {
                    server.memory.remove(&pending.key);
                }
                server.memory.insert(
                    effective_key,
                    MemoryRecord {
                        discovered_at: now,
                        descriptors: descriptors.clone(),
                    },
                );
                server.needs_auth = false;
                server.force_refresh = false;
                Outcome::Discovered(descriptors)
            }
            Authorized::Refused(required) => {
                publish_required(&self.state, setup, alias, &required).await;
                Outcome::Unavailable(McpServerStatus::AuthRequired, McpError::AuthRequired)
            }
            Authorized::Failed(error) => {
                let message = crate::integrations::redact(&error.to_string());
                self.state
                    .lock()
                    .await
                    .failed
                    .insert(alias.to_owned(), message.clone());
                Outcome::Unavailable(McpServerStatus::Failed, McpError::Transport(message))
            }
        }
    }

    /// The server's peer: the one already connected when its entry did not
    /// change, which keeps a pooled stdio session across rediscoveries.
    async fn peer_for(
        &self,
        config: &McpServerConfig,
        runtime: &McpRuntime,
    ) -> Result<Arc<dyn McpPeer>, McpError> {
        {
            let state = self.state.lock().await;
            if let Some(server) = state.servers.get(&config.alias)
                && server
                    .published
                    .as_ref()
                    .is_some_and(|published| published.config == *config)
                && let Some(peer) = &server.peer
            {
                return Ok(peer.clone());
            }
        }
        self.connect(config, runtime).await
    }

    async fn connect(
        &self,
        config: &McpServerConfig,
        runtime: &McpRuntime,
    ) -> Result<Arc<dyn McpPeer>, McpError> {
        let peer = within(
            runtime.factory.connect(config),
            Duration::from_millis(config.startup_timeout_ms),
        )
        .await?;
        self.state.lock().await.server(&config.alias).peer = Some(peer.clone());
        Ok(peer)
    }

    /// Publishes what one server's discovery came to: its tools, and the view
    /// the host reads.
    async fn apply(
        &self,
        config: McpServerConfig,
        outcome: Outcome,
        runtime: &McpRuntime,
        published: &mut Vec<String>,
    ) -> Vec<String> {
        let previous = self
            .state
            .lock()
            .await
            .published(&config.alias)
            .map(|previous| previous.view.tools.clone())
            .unwrap_or_default();
        let _ = set_all(
            &runtime.tools,
            ToolSource::Mcp,
            &previous,
            ToolAvailability::Unavailable,
        );
        let (view, diagnostics) = match outcome {
            Outcome::Cached(descriptors) | Outcome::Discovered(descriptors) => {
                let peer = self.state.lock().await.server(&config.alias).peer.clone();
                let peer = match peer {
                    Some(peer) => Ok(peer),
                    None => self.connect(&config, runtime).await,
                };
                match peer {
                    Ok(peer) => {
                        healthy_view(&self.state, &config, peer, descriptors, runtime, published)
                    }
                    Err(error) => unavailable_view(&config, McpServerStatus::Failed, &error),
                }
            }
            Outcome::Disabled => (
                McpServerView {
                    alias: config.alias.clone(),
                    transport: transport_name(&config.transport).to_owned(),
                    enabled: false,
                    status: McpServerStatus::Disabled,
                    tools: Vec::new(),
                    disabled_tools: config.disabled_tools.clone(),
                    diagnostic: None,
                    remote_names: BTreeMap::new(),
                },
                Vec::new(),
            ),
            Outcome::Unavailable(status, error) => unavailable_view(&config, status, &error),
        };
        let alias = config.alias.clone();
        self.state.lock().await.server(&alias).published = Some(Published { config, view });
        diagnostics
    }

    async fn retire_alias(&self, alias: &str) -> Vec<String> {
        let (peer, tool_names, tools) = {
            let mut state = self.state.lock().await;
            let tools = state.tools();
            let server = state.server(alias);
            server.end_running_calls();
            let tool_names = server
                .published
                .take()
                .map(|published| published.view.tools)
                .unwrap_or_default();
            (server.peer.take(), tool_names, tools)
        };
        if let Some(tools) = tools {
            let _ = set_all(
                &tools,
                ToolSource::Mcp,
                &tool_names,
                ToolAvailability::Unavailable,
            );
        }
        match peer {
            Some(peer) => within(peer.close(), MCP_OPERATION_TIMEOUT)
                .await
                .err()
                .map(|error| canonical_diagnostic(alias, &error))
                .into_iter()
                .collect(),
            None => Vec::new(),
        }
    }
}

/// The view of a server that publishes nothing, and the diagnostic it carries.
fn unavailable_view(
    config: &McpServerConfig,
    status: McpServerStatus,
    error: &McpError,
) -> (McpServerView, Vec<String>) {
    let diagnostic = canonical_diagnostic(&config.alias, error);
    let view = McpServerView {
        alias: config.alias.clone(),
        transport: transport_name(&config.transport).to_owned(),
        enabled: config.enabled,
        status,
        tools: Vec::new(),
        disabled_tools: config.disabled_tools.clone(),
        diagnostic: Some(diagnostic.clone()),
        remote_names: BTreeMap::new(),
    };
    (view, vec![diagnostic])
}

/// Reference `_memory_hit`: a record discovered within the TTL, which the
/// persistent cache is told was used.
fn memory_hit(
    state: &mut McpRegistryState,
    key: &str,
    alias: &str,
    now: UtcTimestamp,
) -> Option<Vec<RemoteTool>> {
    let ttl = state
        .cache
        .as_ref()
        .map_or(descriptor_cache::DESCRIPTOR_CACHE_TTL_SECONDS, |cache| {
            cache.ttl_seconds()
        });
    let memory = &mut state.servers.get_mut(alias)?.memory;
    let record = memory.get(key)?;
    if ttl <= 0.0 || record.discovered_at > now || now.seconds_since(record.discovered_at) >= ttl {
        memory.remove(key);
        return None;
    }
    let (discovered_at, descriptors) = (record.discovered_at, record.descriptors.clone());
    if let Some(cache) = &state.cache {
        cache.touch(key, alias, discovered_at, now);
    }
    Some(descriptors)
}
