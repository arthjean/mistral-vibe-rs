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
            ConnectorCommand::Read => {
                let views = session.connectors.views().map_err(integration_error)?;
                Ok(read_only([
                    ("counts", connector_counts_value(&views)),
                    ("connectors", connector_view(views)),
                ]))
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
                let state = connector_view(session.connectors.views().map_err(integration_error)?);
                Ok(canonical_mutation(
                    "connectors",
                    state,
                    "connectors/updated",
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
                let state = connector_view(session.connectors.views().map_err(integration_error)?);
                Ok(canonical_mutation(
                    "connectors",
                    state,
                    "connectors/updated",
                    Vec::new(),
                ))
            }
        }
    }
}

impl CoreResourceBackend {
    async fn ensure_connectors(&self, session: &CoreResourceSession) -> Result<(), ResourceError> {
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
        if let Some(store) = session.config() {
            let preferences = store.connector_preferences().map_err(config_error)?;
            for view in session.connectors.views().map_err(integration_error)? {
                let Some(preference) = preferences
                    .get(&view.alias)
                    .or_else(|| preferences.get(&view.id))
                else {
                    continue;
                };
                if view.enabled != preference.enabled {
                    session
                        .connectors
                        .toggle(&view.id, None, preference.enabled)
                        .await
                        .map_err(integration_error)?;
                }
                for configured_tool in &preference.disabled_tools {
                    let tool = view
                        .tool_names
                        .iter()
                        .find(|tool| {
                            *tool == configured_tool
                                || tool.ends_with(&format!("_{configured_tool}"))
                        })
                        .ok_or_else(|| {
                            ResourceError::Unavailable(format!(
                                "connector `{}` has no configured tool `{configured_tool}`",
                                view.alias
                            ))
                        })?;
                    session
                        .connectors
                        .toggle(&view.id, Some(tool), false)
                        .await
                        .map_err(integration_error)?;
                }
            }
        }
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
