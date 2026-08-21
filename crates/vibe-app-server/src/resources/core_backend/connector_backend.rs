use super::*;

impl CoreResourceBackend {
    pub(super) async fn dispatch_connectors(
        &self,
        session: &CoreResourceSession,
        session_id: &str,
        command: &ConnectorCommand,
    ) -> Result<ResourceDispatch, ResourceError> {
        self.ensure_connectors(session).await?;
        let _mutation = session.connector_mutation.lock().await;
        match command {
            // The counts are the whole answer: the sources themselves are
            // published in the MCP state, where a client reads every source it
            // can call a tool through in one list.
            ConnectorCommand::Read => {
                let views = session.connectors.views().map_err(integration_error)?;
                Ok(read_only([("counts", connector_counts_value(&views))]))
            }
            ConnectorCommand::AuthRead { name } => {
                let view = resolve_connector(session, name)?;
                let backend = self.connector_auth.as_ref().ok_or_else(|| {
                    ResourceError::Unavailable(
                        "connector authentication backend is not configured".to_owned(),
                    )
                })?;
                let url = backend
                    .auth_url(session_id, &view.id)
                    .await?
                    .map(|url| validate_auth_url(url, "connector authentication"))
                    .transpose()?;
                Ok(read_only([("url", json!(url))]))
            }
            ConnectorCommand::Refresh { name } => {
                let view = resolve_connector(session, name)?;
                let definitions = self.connector_catalog().await?.definitions;
                if !definitions
                    .iter()
                    .any(|definition| definition.id == view.id)
                {
                    return Err(ResourceError::NotFound(format!(
                        "connector `{name}` was not found"
                    )));
                }
                session
                    .connectors
                    .invalidate_cache()
                    .map_err(integration_error)?;
                session
                    .connectors
                    .discover(
                        definitions,
                        &self.connector_credential_reference,
                        self.connector_base_url.as_ref().ok_or_else(|| {
                            ResourceError::Unavailable(
                                "connector catalog backend is not configured".to_owned(),
                            )
                        })?,
                        now_millis(),
                    )
                    .await
                    .map_err(integration_error)?;
                let backend = self.connector_backend.clone().ok_or_else(|| {
                    ResourceError::Unavailable(
                        "connector transport backend is not configured".to_owned(),
                    )
                })?;
                // Re-discovery carries the previous enabled state and disabled
                // list into the fresh views, so the entry is read again here:
                // otherwise a configuration edited between two publications
                // would lose to the runtime state it was meant to correct.
                apply_connector_preferences(session).await?;
                session
                    .connectors
                    .register_tools(
                        &session.tools,
                        backend,
                        session.policy.clone(),
                        self.approval.clone(),
                    )
                    .map_err(integration_error)?;
                if view.auth_kind != ConnectorAuthKind::None {
                    let backend = self.connector_auth.as_ref().ok_or_else(|| {
                        ResourceError::Unavailable(
                            "connector authentication backend is not configured".to_owned(),
                        )
                    })?;
                    let connected = backend.refresh(session_id, &view.id).await?;
                    session
                        .connectors
                        .set_auth(&view.id, connected)
                        .await
                        .map_err(integration_error)?;
                }
                Ok(runtime_mutation(
                    [(
                        "toolCount",
                        json!(session.tools.list().map_or(0, |tools| tools.len())),
                    )],
                    Vec::new(),
                ))
            }
            ConnectorCommand::Toggle {
                name,
                disabled,
                tool_name,
            } => {
                let previous = resolve_connector(session, name)?;
                let connector_id = previous.id.clone();
                let mut desired = previous.clone();
                if let Some(tool_name) = tool_name {
                    if !previous.tool_names.iter().any(|tool| tool == tool_name) {
                        return Err(ResourceError::NotFound(format!(
                            "connector `{name}` has no tool `{tool_name}`"
                        )));
                    }
                    if *disabled {
                        desired.disabled_tools.insert(tool_name.clone());
                    } else {
                        desired.disabled_tools.remove(tool_name);
                    }
                } else {
                    desired.enabled = !disabled;
                }
                let persisted = if let Some(store) = session.config() {
                    Some((
                        store.clone(),
                        persist_connector_view(&store, &desired).map_err(config_error)?,
                    ))
                } else {
                    None
                };
                let mutation = session
                    .connectors
                    .toggle(&connector_id, tool_name.as_deref(), !disabled)
                    .await
                    .map_err(integration_error);
                if let Err(error) = mutation {
                    if let Some((store, committed)) = persisted
                        && let Err(rollback) =
                            rollback_connector_view(&store, &previous, &committed)
                    {
                        return Err(config_rollback_error("connector toggle", error, rollback));
                    }
                    return Err(error);
                }
                Ok(runtime_mutation([], Vec::new()))
            }
        }
    }
}

impl CoreResourceBackend {
    pub(super) async fn ensure_connectors(
        &self,
        session: &CoreResourceSession,
    ) -> Result<(), ResourceError> {
        if session.connectors_initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        let _initialization = session.connector_mutation.lock().await;
        if session.connectors_initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        let catalog = self.connector_catalog().await?;
        if catalog.definitions.is_empty() {
            session
                .connectors_initialized
                .store(true, Ordering::Release);
            return Ok(());
        }
        let backend = self.connector_backend.clone().ok_or_else(|| {
            ResourceError::Unavailable("connector transport backend is not configured".to_owned())
        })?;
        let base_url = self.connector_base_url.as_ref().ok_or_else(|| {
            ResourceError::Unavailable("connector catalog backend is not configured".to_owned())
        })?;
        session
            .connectors
            .discover(
                catalog.definitions,
                &self.connector_credential_reference,
                base_url,
                now_millis(),
            )
            .await
            .map_err(integration_error)?;
        for view in session.connectors.views().map_err(integration_error)? {
            if view.auth_kind != ConnectorAuthKind::None {
                session
                    .connectors
                    .set_auth(&view.id, catalog.connected.contains(&view.id))
                    .await
                    .map_err(integration_error)?;
            }
        }
        apply_connector_preferences(session).await?;
        session
            .connectors
            .register_tools(
                &session.tools,
                backend,
                session.policy.clone(),
                self.approval.clone(),
            )
            .map_err(integration_error)?;
        session
            .connectors_initialized
            .store(true, Ordering::Release);
        Ok(())
    }

    async fn connector_catalog(&self) -> Result<ConnectorCatalog, ResourceError> {
        match &self.connector_catalog {
            Some(catalog) => catalog.catalog().await,
            None => Ok(ConnectorCatalog {
                definitions: self.connector_definitions.as_ref().clone(),
                connected: BTreeSet::new(),
            }),
        }
    }
}

/// Reconciles every discovered connector against its configuration entry.
///
/// The reference rebuilds its disable index on every publication
/// (`_apply_per_source_filtering`, vibe/core/tools/manager.py), so the file
/// decides the surface each time it is published rather than once per session:
/// a refresh that carries a runtime state forward is reconciled against the
/// entry again before the tools are registered.
async fn apply_connector_preferences(session: &CoreResourceSession) -> Result<(), ResourceError> {
    // Reference `_build_source_disable_index` (vibe/core/tools/manager.py)
    // makes a connector opt-in where an MCP server is opt-out: a connector
    // the registry knows and the configuration never names joins the
    // disabled set exactly like one whose entry carries `disabled`. The
    // entry decides publication outright, so it also outranks the enabled
    // state and the disabled list an earlier discovery carried into the
    // view through its previous-state map.
    let preferences = session
        .config()
        .map(|store| store.connector_preferences())
        .transpose()
        .map_err(config_error)?
        .unwrap_or_default();
    for view in session.connectors.views().map_err(integration_error)? {
        // The alias keeps the case the reference gives it, where this
        // port used to lowercase it. A preference persisted by an
        // older build therefore carries the lowercased alias, so the
        // match ignores case rather than silently dropping it.
        let preference = preferences
            .iter()
            .find(|(key, _)| {
                key.eq_ignore_ascii_case(&view.alias) || key.eq_ignore_ascii_case(&view.id)
            })
            .map(|(_, preference)| preference);
        let enabled = preference.is_some_and(|preference| preference.enabled);
        if view.enabled != enabled {
            session
                .connectors
                .toggle(&view.id, None, enabled)
                .await
                .map_err(integration_error)?;
        }
        let Some(preference) = preference.filter(|_| enabled) else {
            // The index records a per-tool list only for an entry that is
            // not disabled, so a connector withheld whole never consults
            // one.
            continue;
        };
        // The list keys on the remote name: [`persisted_tool_names`] writes
        // it by stripping the published prefix, and the reference tests
        // `get_remote_name()` against the entry. Reading the published
        // names back through that same prefix is what keeps an entry
        // naming a tool this connector does not expose inert instead of
        // failing the whole initialization.
        let prefix = format!("connector_{}_", view.alias);
        for tool in &view.tool_names {
            let remote = tool.strip_prefix(&prefix).unwrap_or(tool);
            let keep = !preference.disabled_tools.contains(remote);
            if view.disabled_tools.contains(tool) == keep {
                session
                    .connectors
                    .toggle(&view.id, Some(tool), keep)
                    .await
                    .map_err(integration_error)?;
            }
        }
    }
    Ok(())
}
