//! The `connector_catalog/*` methods and the `connectors/*` methods the
//! reference answers through the same service (reference
//! `ConnectorCatalogService`, `vibe/app_server/connector_catalog.py`).
//!
//! A session resolves the catalog it opens with before its first connector
//! operation, as the reference resolves it while building the session, and
//! accepting a catalog reroutes the session's connectors: its route revision
//! advances and its runtime is published after the answer.

use crate::connector_catalog::{
    self as catalog, CACHE_FILE, CatalogProvider, ConnectorSelection, Disposition, ResolvedCatalog,
};

use super::*;

/// How long a cached catalog stays fresh, in seconds.
const CATALOG_TTL_SECONDS: i64 = 600;

impl AppServer {
    /// The provider the configuration resolves, with the snapshot it read.
    fn connector_provider(
        &self,
    ) -> Result<(vibe_core::config::ConfigSnapshot, Option<CatalogProvider>), ProtocolFault> {
        let snapshot = self
            .workspace
            .config_snapshot()
            .ok_or_else(|| ProtocolFault::internal("the configuration could not be read"))?;
        let provider = catalog::resolve_provider(&snapshot, |variable| {
            self.workspace.resolve_credential(variable)
        });
        Ok((snapshot, provider))
    }

    fn connector_cache_path(&self) -> PathBuf {
        self.workspace.vibe_home().join(CACHE_FILE)
    }

    /// Reference `_read_catalog_for_provider`: memory while fresh, then the
    /// cache, never the network.
    fn cached_connector_catalog(
        &self,
        provider: Option<&CatalogProvider>,
    ) -> (Option<ResolvedCatalog>, Disposition) {
        let Some(provider) = provider else {
            return (None, Disposition::NotLoaded);
        };
        let now = connector_clock();
        if let Ok(memory) = self.connector_catalogs.lock()
            && let Some((catalog, stored_at)) = memory.get(&provider.fingerprint)
            && *stored_at <= now
            && *stored_at > now - CATALOG_TTL_SECONDS
        {
            return (Some(catalog.clone()), Disposition::Memory);
        }
        match catalog::read_cache(&self.connector_cache_path(), &provider.fingerprint, now) {
            Some((catalog, stored_at)) => {
                if let Ok(mut memory) = self.connector_catalogs.lock() {
                    memory.insert(provider.fingerprint.clone(), (catalog.clone(), stored_at));
                }
                (Some(catalog), Disposition::FreshCache)
            }
            None => (None, Disposition::NotLoaded),
        }
    }

    /// Reference `resolve_catalog`: the cached catalog unless `force`, the
    /// bootstrap fetched, resolved and cached otherwise.
    async fn resolve_connector_catalog(
        &self,
        provider: &CatalogProvider,
        force: bool,
    ) -> Result<ResolvedCatalog, catalog::CatalogError> {
        if !force && let (Some(cached), _) = self.cached_connector_catalog(Some(provider)) {
            return Ok(cached);
        }
        let payload = catalog::fetch_bootstrap(provider).await?;
        let resolved = catalog::resolve_catalog(&payload, &provider.fingerprint)?;
        let stored_at = connector_clock();
        let _ = catalog::write_cache(&self.connector_cache_path(), &resolved, stored_at);
        if let Ok(mut memory) = self.connector_catalogs.lock() {
            memory.insert(provider.fingerprint.clone(), (resolved.clone(), stored_at));
        }
        Ok(resolved)
    }

    /// Resolves the catalog `session_id` opens with, once: a catalog that
    /// cannot be had leaves the session with none, as the reference logs it
    /// and starts anyway.
    async fn open_session_connectors(&self, session_id: &str) -> Result<(), ProtocolFault> {
        {
            let mut sessions = self.lock_sessions()?;
            let session = sessions
                .get_mut(session_id)
                .ok_or_else(|| session_missing("Session not found"))?;
            if session.connectors.opened {
                return Ok(());
            }
            session.connectors.opened = true;
        }
        let (snapshot, provider) = self.connector_provider()?;
        let Some(provider) = provider else {
            return Ok(());
        };
        let Ok(resolved) = self.resolve_connector_catalog(&provider, false).await else {
            return Ok(());
        };
        let selection = catalog::resolve_selection(&snapshot, Some(&resolved));
        if let Some(session) = self.lock_sessions()?.get_mut(session_id) {
            session.connectors.accept(resolved, selection);
        }
        Ok(())
    }

    /// Reference `reconfigure_connectors` on the legacy backend, which
    /// refuses while the session runs a turn. Answers the accepted state.
    fn accept_connector_catalog(
        &self,
        session_id: &str,
        resolved: ResolvedCatalog,
        selection: ConnectorSelection,
    ) -> Result<Value, ProtocolFault> {
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        if let Some(turn_id) = &session.active_turn {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                format!("Session is busy running turn {turn_id}"),
            ));
        }
        Ok(session.connectors.accept(resolved, selection))
    }

    fn session_connector_view(&self, session_id: &str) -> Result<Value, ProtocolFault> {
        let sessions = self.lock_sessions()?;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        Ok(session.connectors.view())
    }

    /// Reference `_mutation_response`, with the session's runtime published
    /// after it.
    fn connector_mutation_batch(
        &self,
        request_id: RequestId,
        session_id: &str,
        catalog_revision: &str,
        selection_revision: &str,
        state: &Value,
    ) -> DispatchBatch {
        let runtime = self.runtime_snapshot(session_id);
        let mut outbound = vec![success_bytes(
            request_id,
            mutation_result(
                Some(catalog_revision),
                Some(selection_revision),
                Some(state),
                runtime.clone(),
                false,
            ),
        )];
        if let Some(runtime) = runtime {
            outbound.push(encode_notification(
                "runtime/updated",
                result_map([("sessionId", json!(session_id)), ("runtime", runtime)]),
            ));
        }
        DispatchBatch {
            outbound,
            deferred: Vec::new(),
            close_after_flush: false,
        }
    }

    /// One `connector_catalog/*` or `connectors/*` call. `root` is the
    /// session the connection is attached to, which a call naming none may
    /// still act on.
    pub(crate) async fn execute_connector_call(
        &self,
        request_id: RequestId,
        method: &str,
        params: &BTreeMap<String, Value>,
        root: Option<String>,
    ) -> DispatchBatch {
        let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
        let outcome = match method {
            "connector_catalog/read" => {
                self.connector_catalog_read(request_id.clone(), text("sessionId"))
                    .await
            }
            "connector_catalog/refresh" => {
                self.connector_catalog_refresh(request_id.clone(), text("sessionId"))
                    .await
            }
            "connector_catalog/toggle" => {
                self.connector_catalog_toggle(
                    request_id.clone(),
                    text("sessionId"),
                    root,
                    &text("alias").unwrap_or_default(),
                    params
                        .get("disabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    text("toolName").as_deref(),
                )
                .await
            }
            "connector_catalog/auth/request" => {
                self.connector_auth_request(
                    request_id.clone(),
                    &text("sessionId").unwrap_or_default(),
                    &text("alias").unwrap_or_default(),
                )
                .await
            }
            "connectors/read" => {
                self.connectors_read(request_id.clone(), &text("sessionId").unwrap_or_default())
                    .await
            }
            "connectors/refresh" => {
                self.connectors_refresh(
                    request_id.clone(),
                    &text("sessionId").unwrap_or_default(),
                    &text("name").unwrap_or_default(),
                )
                .await
            }
            "connectors/auth/read" => {
                self.connectors_auth_read(
                    request_id.clone(),
                    &text("sessionId").unwrap_or_default(),
                    &text("name").unwrap_or_default(),
                )
                .await
            }
            _ => Err(ProtocolFault::plain(
                ProtocolErrorCode::MethodNotFound,
                format!("Method not found: {method}"),
            )),
        };
        outcome.unwrap_or_else(|fault| fault.into_batch(request_id))
    }

    /// Reference `_read`: the cached catalog, the configuration's
    /// selections, the targeted session's accepted state and the console
    /// link managing the account's connectors.
    async fn connector_catalog_read(
        &self,
        request_id: RequestId,
        session_id: Option<String>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        if let Some(session_id) = &session_id {
            self.open_session_connectors(session_id).await?;
        }
        let (snapshot, provider) = self.connector_provider()?;
        let (resolved, disposition) = self.cached_connector_catalog(provider.as_ref());
        let session = match &session_id {
            Some(session_id) => Some(self.session_connector_view(session_id)?),
            None => None,
        };
        let manage_url = match (&provider, catalog::connector_api_base(&snapshot)) {
            (Some(provider), Some(api_base)) => {
                self.workspace
                    .connector_manage_url(&api_base, &provider.api_key)
                    .await
            }
            _ => None,
        };
        Ok(success_batch(
            request_id,
            result_map([
                (
                    "catalog",
                    catalog::catalog_view(resolved.as_ref(), disposition),
                ),
                (
                    "selections",
                    catalog::selections_view(&snapshot, resolved.as_ref()),
                ),
                ("session", json!(session)),
                ("manageUrl", json!(manage_url)),
            ]),
        ))
    }

    /// Reference `_refresh_request`: the bootstrap is fetched again and
    /// cached, and a targeted session accepts it.
    async fn connector_catalog_refresh(
        &self,
        request_id: RequestId,
        session_id: Option<String>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        if let Some(session_id) = &session_id {
            self.open_session_connectors(session_id).await?;
        }
        let (snapshot, provider) = self.connector_provider()?;
        let Some(provider) = provider else {
            return Ok(success_batch(
                request_id,
                mutation_result(None, None, None, None, false),
            ));
        };
        let resolved = self
            .resolve_connector_catalog(&provider, true)
            .await
            .map_err(|error| ProtocolFault::plain(ProtocolErrorCode::InternalError, error.0))?;
        let selection = catalog::resolve_selection(&snapshot, Some(&resolved));
        let catalog_revision = resolved.revision.clone();
        let selection_revision = selection.revision.clone();
        let Some(session_id) = session_id else {
            return Ok(success_batch(
                request_id,
                mutation_result(
                    Some(&catalog_revision),
                    Some(&selection_revision),
                    None,
                    None,
                    false,
                ),
            ));
        };
        let state = self.accept_connector_catalog(&session_id, resolved, selection)?;
        Ok(self.connector_mutation_batch(
            request_id,
            &session_id,
            &catalog_revision,
            &selection_revision,
            &state,
        ))
    }

    /// Reference `_toggle`: the configuration records the toggle, and a
    /// targeted session accepts the selection it makes.
    async fn connector_catalog_toggle(
        &self,
        request_id: RequestId,
        session_id: Option<String>,
        root: Option<String>,
        alias: &str,
        disabled: bool,
        tool_name: Option<&str>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        catalog::validate_toggle(alias, tool_name)
            .map_err(|message| ProtocolFault::plain(ProtocolErrorCode::InvalidParams, message))?;
        if let Some(session_id) = &session_id {
            self.open_session_connectors(session_id).await?;
        }
        let accepted = match &session_id {
            Some(session_id) => {
                let sessions = self.lock_sessions()?;
                let session = sessions
                    .get(session_id)
                    .ok_or_else(|| session_missing("Session not found"))?;
                if session.connectors.source(alias).is_none() {
                    return Err(ProtocolFault::plain(
                        ProtocolErrorCode::NotFound,
                        format!(
                            "Connector alias not found in the accepted session catalog: {alias}"
                        ),
                    ));
                }
                session
                    .connectors
                    .accepted
                    .as_ref()
                    .map(|(catalog, _)| catalog.revision.clone())
            }
            None => None,
        };
        let (snapshot, provider) = self.connector_provider()?;
        let (resolved, _) = self.cached_connector_catalog(provider.as_ref());
        if session_id.is_some()
            && resolved.as_ref().map(|catalog| &catalog.revision) != accepted.as_ref()
        {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "The host connector catalog does not match the target session",
            ));
        }
        let current = catalog::resolve_selection(&snapshot, resolved.as_ref());
        let candidate = catalog::toggled_selection(
            &current,
            alias,
            disabled,
            tool_name,
            &catalog::catalog_sources(resolved.as_ref()),
        );
        // A toggle naming no session may not reroute the one attached.
        if session_id.is_none()
            && let Some(root) = &root
        {
            let (live_sources, live_revision) = {
                let sessions = self.lock_sessions()?;
                let live = sessions
                    .get(root)
                    .map(|session| session.connectors.accepted.clone());
                match live.flatten() {
                    Some((catalog, selection)) => {
                        (catalog::catalog_sources(Some(&catalog)), selection.revision)
                    }
                    None => (Vec::new(), String::new()),
                }
            };
            let live =
                catalog::toggled_selection(&current, alias, disabled, tool_name, &live_sources);
            if live.revision != live_revision {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::Conflict,
                    "The connector selection affects a live session; provide sessionId",
                ));
            }
        }
        self.workspace
            .persist_connector_toggle(alias, disabled, tool_name)
            .map_err(|error| match error {
                error @ vibe_core::config::ConfigError::ConcurrentEdit { .. } => {
                    ProtocolFault::plain(ProtocolErrorCode::Conflict, error.to_string())
                }
                other => ProtocolFault::plain(
                    ProtocolErrorCode::InvalidParams,
                    format!("Failed to persist /connectors: {other}"),
                ),
            })?;
        let pending = resolved.as_ref().is_none_or(|catalog| {
            !catalog
                .connectors
                .iter()
                .any(|connector| connector.alias == alias)
        });
        let (Some(session_id), Some(resolved)) = (session_id, resolved.clone()) else {
            let catalog_revision = resolved.map(|catalog| catalog.revision);
            return Ok(success_batch(
                request_id,
                mutation_result(
                    catalog_revision.as_deref(),
                    Some(&candidate.revision),
                    None,
                    None,
                    pending,
                ),
            ));
        };
        let catalog_revision = resolved.revision.clone();
        let selection_revision = candidate.revision.clone();
        let state = self.accept_connector_catalog(&session_id, resolved, candidate)?;
        Ok(self.connector_mutation_batch(
            request_id,
            &session_id,
            &catalog_revision,
            &selection_revision,
            &state,
        ))
    }

    /// Reference `request_connector_auth` on the legacy backend: the source
    /// has to exist and wait on an authorization the client can act on.
    fn connector_auth_target(
        &self,
        session_id: &str,
        alias: &str,
    ) -> Result<(String, String, &'static str, &'static str), ProtocolFault> {
        let sessions = self.lock_sessions()?;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        let (connector, status) = session.connectors.source(alias).ok_or_else(|| {
            ProtocolFault::plain(
                ProtocolErrorCode::NotFound,
                format!("Connector not found: {alias}"),
            )
        })?;
        if !matches!(status, "needs_auth" | "needs_setup") {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                format!("Connector authorization is not actionable: {alias}"),
            ));
        }
        let revision = session
            .connectors
            .accepted
            .as_ref()
            .map(|(catalog, _)| catalog.revision.clone())
            .unwrap_or_default();
        Ok((
            connector.raw_id.clone(),
            revision,
            connector.auth_action,
            status,
        ))
    }

    /// Reference `_connector_auth_url`: the authorization page for a
    /// connector, or `None` when the provider does not hand one out.
    async fn connector_auth_url(&self, raw_connector_id: &str) -> Option<String> {
        let (_, provider) = self.connector_provider().ok()?;
        let provider = provider?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .ok()?;
        let response = client
            .get(format!(
                "{}/v1/connectors/{raw_connector_id}/auth_url",
                provider.base_url
            ))
            .bearer_auth(&provider.api_key)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let payload = response.json::<Value>().await.ok()?;
        payload
            .get("auth_url")
            .or_else(|| payload.get("authUrl"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// Reference `_request_auth` and `_broker_authorization`: the request is
    /// answered, then `connector_catalog/authRequired` and either
    /// `connector_catalog/authUrl` or `connector_catalog/authFailed` follow.
    async fn connector_auth_request(
        &self,
        request_id: RequestId,
        session_id: &str,
        alias: &str,
    ) -> Result<DispatchBatch, ProtocolFault> {
        catalog::validate_toggle(alias, None)
            .map_err(|message| ProtocolFault::plain(ProtocolErrorCode::InvalidParams, message))?;
        self.open_session_connectors(session_id).await?;
        let (raw_id, revision, action, status) = self.connector_auth_target(session_id, alias)?;
        let key = (
            session_id.to_owned(),
            raw_id.clone(),
            revision.clone(),
            action.to_owned(),
        );
        {
            let mut pending = self
                .connector_auth_requests
                .lock()
                .map_err(|_| ProtocolFault::internal("the authorization ledger is poisoned"))?;
            if !pending.insert(key.clone()) {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::Conflict,
                    "Connector authorization is already pending for this catalog revision",
                ));
            }
        }
        let broker_id = vibe_core::session_id::uuid_v4();
        let required = json!({
            "sessionId": session_id,
            "alias": alias,
            "acceptedCatalogRevision": revision,
            "reason": status,
        });
        let mut outbound = vec![success_bytes(
            request_id,
            result_map([
                ("requestId", json!(broker_id)),
                ("sessionId", json!(session_id)),
                ("alias", json!(alias)),
                ("acceptedCatalogRevision", json!(revision)),
            ]),
        )];
        outbound.push(encode_notification(
            "connector_catalog/authRequired",
            object_map(&required),
        ));
        let still_accepted = self.connector_auth_target(session_id, alias).is_ok_and(
            |(current_id, current_revision, _, _)| {
                current_id == raw_id && current_revision == revision
            },
        );
        let mut settled = object_map(&required);
        settled.insert("requestId".to_owned(), json!(broker_id));
        if !still_accepted {
            settled.insert("code".to_owned(), json!("stale_request"));
            outbound.push(encode_notification("connector_catalog/authFailed", settled));
        } else {
            match self.connector_auth_url(&raw_id).await {
                Some(url) => {
                    settled.insert("url".to_owned(), json!(url));
                    outbound.push(encode_notification("connector_catalog/authUrl", settled));
                }
                None => {
                    settled.insert("code".to_owned(), json!("auth_url_unavailable"));
                    outbound.push(encode_notification("connector_catalog/authFailed", settled));
                }
            }
        }
        if let Ok(mut pending) = self.connector_auth_requests.lock() {
            pending.remove(&key);
        }
        Ok(DispatchBatch {
            outbound,
            deferred: Vec::new(),
            close_after_flush: false,
        })
    }

    /// Reference `_compat_read`: the accepted sources, counted.
    async fn connectors_read(
        &self,
        request_id: RequestId,
        session_id: &str,
    ) -> Result<DispatchBatch, ProtocolFault> {
        self.open_session_connectors(session_id).await?;
        let counts = {
            let sessions = self.lock_sessions()?;
            sessions
                .get(session_id)
                .ok_or_else(|| session_missing("Session not found"))?
                .connectors
                .counts()
        };
        Ok(success_batch(request_id, result_map([("counts", counts)])))
    }

    /// Reference `_compat_refresh`: a refresh of the session, answered with
    /// the tools the named connector publishes enabled.
    async fn connectors_refresh(
        &self,
        request_id: RequestId,
        session_id: &str,
        name: &str,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let refreshed = self
            .connector_catalog_refresh(request_id.clone(), Some(session_id.to_owned()))
            .await?;
        let tool_count = {
            let sessions = self.lock_sessions()?;
            let session = sessions
                .get(session_id)
                .ok_or_else(|| session_missing("Session not found"))?;
            let (connector, _) = session.connectors.source(name).ok_or_else(|| {
                ProtocolFault::plain(
                    ProtocolErrorCode::NotFound,
                    format!("Connector not found: {name}"),
                )
            })?;
            let selection = session
                .connectors
                .accepted
                .as_ref()
                .map(|(_, selection)| selection);
            connector
                .tools
                .iter()
                .filter(|tool| {
                    selection.is_some_and(|selection| selection.tool_enabled(name, &tool.raw_name))
                })
                .count()
        };
        let runtime = self.runtime_snapshot(session_id).ok_or_else(|| {
            ProtocolFault::plain(
                ProtocolErrorCode::InternalError,
                "Connector refresh did not produce a runtime projection",
            )
        })?;
        let mut outbound = vec![success_bytes(
            request_id,
            result_map([("toolCount", json!(tool_count)), ("runtime", runtime)]),
        )];
        // The refresh's own `runtime/updated` follows the answer.
        outbound.extend(refreshed.outbound.into_iter().skip(1));
        Ok(DispatchBatch {
            outbound,
            deferred: Vec::new(),
            close_after_flush: false,
        })
    }

    /// Reference `_compat_auth_read`: the authorization page of a connector
    /// waiting on one.
    async fn connectors_auth_read(
        &self,
        request_id: RequestId,
        session_id: &str,
        name: &str,
    ) -> Result<DispatchBatch, ProtocolFault> {
        self.open_session_connectors(session_id).await?;
        let (raw_id, ..) = self.connector_auth_target(session_id, name)?;
        let url = self.connector_auth_url(&raw_id).await;
        Ok(success_batch(request_id, result_map([("url", json!(url))])))
    }
}

/// Reference `ConnectorCatalogMutationResponse`.
fn mutation_result(
    catalog_revision: Option<&str>,
    selection_revision: Option<&str>,
    state: Option<&Value>,
    runtime: Option<Value>,
    pending_selection: bool,
) -> BTreeMap<String, Value> {
    let accepted = |key: &str| state.map_or(Value::Null, |state| state[key].clone());
    result_map([
        ("catalogRevision", json!(catalog_revision)),
        ("selectionRevision", json!(selection_revision)),
        (
            "acceptedCatalogRevision",
            accepted("acceptedCatalogRevision"),
        ),
        (
            "acceptedSelectionRevision",
            accepted("acceptedSelectionRevision"),
        ),
        ("routeRevision", accepted("routeRevision")),
        ("runtime", json!(runtime)),
        ("pendingSelection", json!(pending_selection)),
    ])
}

fn object_map(value: &Value) -> BTreeMap<String, Value> {
    value
        .as_object()
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Seconds since the epoch, the cache's clock.
fn connector_clock() -> i64 {
    i64::try_from(now_millis() / 1000).unwrap_or(i64::MAX)
}
