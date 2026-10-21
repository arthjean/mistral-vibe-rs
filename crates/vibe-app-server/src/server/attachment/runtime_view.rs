//! A session's runtime as a client reads it: `runtime/read`, which
//! `runtime/updated` also publishes, and the summary of its log.

use super::*;

impl AppServer {
    /// The workspace as `session` sees its agents: its directory, trust and
    /// roots, and the profiles its start forced.
    pub(crate) fn agent_workspace(&self, session: &SessionRuntime) -> WorkspaceService {
        self.workspace
            .scoped_to_agents(
                PathBuf::from(&session.working_directory),
                session.intent.trusted,
                &session.intent.add_directories,
                session.intent.project_file_trust,
                &session.intent.forced_agents,
            )
            .with_seeded_skills(self.plugin_skill_seed_of(session))
    }

    /// Replaces the connector rows of `mcp` with the ones the session's
    /// accepted catalog, or the configuration alone, publishes (reference
    /// `_project_mcp_connectors`), answering the session's connector counts.
    pub(crate) fn overlay_session_connectors(
        &self,
        session_id: &str,
        mcp: Option<&mut Value>,
    ) -> Option<Value> {
        let settings = self
            .workspace
            .config_snapshot()
            .map(|snapshot| crate::connector_catalog::connector_settings(&snapshot))
            .unwrap_or_default();
        let unified = self.unified_session(session_id).is_some();
        let (connector_sources, counts, load_error) = {
            let sessions = self.lock_sessions().ok()?;
            let session = sessions.get(session_id)?;
            let (sources, counts) = if unified {
                session.connectors.unified_sources()
            } else {
                (
                    session.connectors.mcp_sources(&settings),
                    session.connectors.runtime_counts(),
                )
            };
            (sources, counts, session.connectors.load_error.clone())
        };
        if let Some(Value::Object(mcp)) = mcp {
            if let Some(error) = load_error {
                mcp.insert("connectorError".to_owned(), Value::String(error));
            }
            let mut replaced = Vec::new();
            if let Some(Value::Array(sources)) = mcp.get_mut("sources") {
                sources.retain(|source| {
                    let connector = source["kind"] == "connector";
                    if connector && let Some(name) = source["name"].as_str() {
                        replaced.push(name.to_owned());
                    }
                    !connector
                });
                sources.extend(connector_sources);
            }
            if let Some(Value::Object(errors)) = mcp.get_mut("discoveryErrors") {
                errors.retain(|name, _| !replaced.contains(name));
            }
        }
        Some(counts)
    }

    /// What a unified session's runtime lists differently: its servers' and
    /// connectors' tools run inside the core, so the session's own tool rows
    /// leave them out, and a server lists its tools in the order it published
    /// them (reference `project_mcp_sources` over the harness runtime's
    /// `SessionMCPSourceState`, where the legacy projection sorts them).
    fn unified_tool_rows(&self, session_id: &str, snapshot: &mut serde_json::Map<String, Value>) {
        let Some(registry) = self
            .resources
            .lock()
            .ok()
            .and_then(|resources| resources.session_tool_registry(session_id))
        else {
            return;
        };
        let specs = registry.list().unwrap_or_default();
        if let Some(Value::Array(tools)) = snapshot.get_mut("tools") {
            tools.retain(|tool| {
                specs
                    .iter()
                    .find(|spec| tool["name"] == spec.name.as_str())
                    .is_none_or(|spec| spec.source == vibe_core::tools::ToolSource::BuiltIn)
            });
        }
        let published: Vec<(String, String)> = specs
            .iter()
            .filter(|spec| spec.source == vibe_core::tools::ToolSource::Mcp)
            .filter_map(|spec| registry.remote_origin(&spec.name))
            .map(|origin| (origin.provider, origin.remote_name))
            .collect();
        let Some(Value::Array(sources)) = snapshot
            .get_mut("mcp")
            .and_then(|mcp| mcp.get_mut("sources"))
        else {
            return;
        };
        for source in sources
            .iter_mut()
            .filter(|source| source["kind"] == "server" && source["pluginName"].is_null())
        {
            let name = source["name"].as_str().unwrap_or_default().to_owned();
            if let Some(Value::Array(rows)) = source.get_mut("tools") {
                rows.sort_by_key(|row| {
                    published
                        .iter()
                        .position(|(provider, remote)| {
                            *provider == name && row["name"] == remote.as_str()
                        })
                        .unwrap_or(usize::MAX)
                });
            }
        }
    }

    /// The session's runtime as `runtime/read` answers and `runtime/updated`
    /// publishes it, or `None` when the resource state cannot be read.
    ///
    /// Three owners contribute: the resource service holds the tool surface and
    /// the integrations, the workspace service holds the configuration and the
    /// catalogs, and the session itself holds its accounting. Composing here is
    /// what makes the answer live rather than a fixed payload, and it is the
    /// same composition the notification publishes.
    pub(crate) fn runtime_snapshot(&self, session_id: &str) -> Option<Value> {
        let mut snapshot = self.resources.lock().ok()?.runtime(session_id).ok()?;
        let (agents, active_agent, stats, context_window, pinned, hooks, plugins, tool_filters) =
            match self.lock_sessions() {
                Ok(sessions) => {
                    let session = sessions.get(session_id);
                    (
                        session.map(|session| self.agent_workspace(session)),
                        session.and_then(|session| session.intent.agent.clone()),
                        public_stats(session),
                        session.map_or(0, |session| session.context_window),
                        session.is_some_and(|session| session.pinned_model.is_some()),
                        session
                            .map(|session| session.hooks.clone())
                            .unwrap_or_default(),
                        session.and_then(|session| self.plugins_of(session)),
                        session.map(|session| {
                            (
                                session.intent.enabled_tools.clone(),
                                session.intent.disabled_tools.clone(),
                            )
                        }),
                    )
                }
                Err(_) => (
                    None,
                    None,
                    public_stats(None),
                    0,
                    false,
                    Default::default(),
                    None,
                    None,
                ),
            };
        if let (Some((enabled, disabled)), Some(Value::Array(tools))) =
            (&tool_filters, snapshot.get_mut("tools"))
        {
            publish_session_tools(tools, enabled, disabled);
        }
        if let (Some(plugins), Some(mcp)) = (&plugins, snapshot.get_mut("mcp")) {
            crate::resources::overlay_plugin_sources(mcp, &plugins.mcp_sources);
        }
        if self.unified_session(session_id).is_some() {
            self.unified_tool_rows(session_id, &mut snapshot);
        }
        if let Some(counts) = self.overlay_session_connectors(session_id, snapshot.get_mut("mcp")) {
            snapshot.insert("connectors".to_owned(), counts);
        }
        let stats = priced(stats, self.workspace.active_model_pricing());
        let projection = agents
            .as_ref()
            .unwrap_or(&self.workspace)
            .runtime_projection(active_agent.as_deref());
        // Discovery issues and configuration diagnostics are the same fact to a
        // client: a file the session could not read cleanly.
        // Reference `project_diagnostics` lists the hook issues first.
        if let Some(Value::Array(issues)) = snapshot.get_mut("issues") {
            // A unified session lists the plugin issues first and the hook
            // issues last (`vibe/app_server/_runtime.py`, the unified
            // derivation's `issues`).
            if let Some(plugins) = &plugins {
                let mut ordered = crate::plugins::runtime_plugin_issues(plugins);
                ordered.append(issues);
                ordered.extend(projection.issues);
                ordered.extend(hooks.issues());
                *issues = ordered;
            } else {
                let mut hook_issues: Vec<Value> = hooks.issues();
                hook_issues.append(issues);
                *issues = hook_issues;
                issues.extend(projection.issues);
            }
        }
        let mut config = projection.config;
        // Reference `active_model_is_pinned`: a session that pinned its model
        // holds it in its overrides.
        if pinned && let Some(fields) = config.as_object_mut() {
            fields.insert("activeModelPinned".to_owned(), json!(true));
        }
        snapshot.insert("config".to_owned(), config);
        snapshot.insert(
            "bypassToolPermissions".to_owned(),
            json!(projection.bypass_tool_permissions),
        );
        snapshot.insert(
            "experimentalHarness".to_owned(),
            json!(self.harness_selection().use_unified),
        );
        snapshot.insert("activeAgent".to_owned(), projection.active_agent);
        snapshot.insert("agents".to_owned(), Value::Array(projection.agents));
        let skills = match &plugins {
            Some(plugins) => crate::plugins::order_skill_rows(plugins, projection.skills),
            None => projection.skills,
        };
        snapshot.insert("skills".to_owned(), Value::Array(skills));
        snapshot.insert("hooksCount".to_owned(), json!(hooks.count()));
        snapshot.insert("stats".to_owned(), stats);
        snapshot.insert("contextWindow".to_owned(), json!(context_window));
        Some(Value::Object(snapshot))
    }

    /// How many images in the session's history the active model cannot read.
    ///
    /// A client shows this after a configuration change so the operator learns
    /// that switching model dropped what the transcript already carries. A model
    /// that reads images strips nothing, so the count is zero without walking
    /// the history.
    pub(crate) fn stripped_history_images(&self, session_id: &str) -> usize {
        if self.workspace.active_model_supports_images() {
            return 0;
        }
        let Ok(sessions) = self.lock_sessions() else {
            return 0;
        };
        sessions
            .get(session_id)
            .and_then(|session| session.snapshot.as_ref())
            .map(|snapshot| {
                snapshot
                    .history
                    .iter()
                    .filter_map(|entry| match entry {
                        PublicHistoryEntry::Message { content, .. } => Some(content),
                        _ => None,
                    })
                    .flatten()
                    .filter(|block| matches!(block, PublicContentBlock::Image { .. }))
                    .count()
            })
            .unwrap_or(0)
    }

    /// The session's logging state as `SessionLogSummary` declares it
    /// (reference `_session_log_summary`): a session is persisted once its
    /// record is on disk, which is also when it has a path.
    pub(crate) fn session_log_summary(&self, session_id: &str) -> Value {
        let enabled = self.workspace.session_logging_enabled();
        if let Some(unified) = self.unified_session(session_id) {
            return unified.session_log(enabled);
        }
        let store = self.workspace.session_store();
        let (held, snapshot_title) = self
            .lock_sessions()
            .ok()
            .and_then(|sessions| {
                sessions.get(session_id).map(|session| {
                    (
                        session.persisted.clone(),
                        session
                            .snapshot
                            .as_ref()
                            .and_then(|snapshot| snapshot.title.clone()),
                    )
                })
            })
            .unwrap_or_default();
        let metadata = store
            .open(session_id)
            .ok()
            .or(held)
            .map(|hydrated| hydrated.metadata);
        let written = metadata.as_ref().filter(|metadata| {
            !self.workspace.is_draft(&metadata.id) && metadata.is_persisted(&store)
        });
        let title = metadata
            .as_ref()
            .and_then(|metadata| metadata.title.clone())
            .or(snapshot_title);
        json!({
            "enabled": enabled,
            "sessionId": metadata.as_ref().map_or(session_id, |metadata| metadata.id.as_str()),
            "persisted": written.is_some(),
            "path": written
                .map(|metadata| metadata.directory.as_str())
                .filter(|directory| !directory.is_empty())
                .map(|directory| self.workspace.session_path(directory)),
            "title": title,
            "needsInitialAutoTitle": title.is_none(),
        })
    }
}
