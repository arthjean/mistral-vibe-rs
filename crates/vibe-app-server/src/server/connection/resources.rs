//! The methods a connection answers from the services the server holds
//! beside its sessions: the MCP catalog, the resource service, a session's
//! live state, telemetry, feedback, and the provider view.

use super::*;

impl ServerConnection {
    /// Reference `MCPCatalogService.dispatch`: the parameters are validated
    /// and the target resolved here, against the sessions this connection
    /// holds, and the call itself runs deferred.
    pub(super) fn mcp_catalog_request(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = request
            .params
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let call = match crate::resources::mcp_catalog::parse(&request.method, &params) {
            Ok(call) => call,
            Err(error) => return catalog_error_batch(request.id, error),
        };
        let target = {
            let sessions = self.server.lock_sessions();
            let attached = |session_id: &str| {
                sessions
                    .key(session_id)
                    .is_some_and(|key| self.attached_sessions.contains(key))
            };
            crate::resources::mcp_catalog::target(
                &call,
                &attached,
                !self.attached_sessions.is_empty(),
            )
        };
        // Reference `_route_notification`: an attached connection receives
        // the URL a sign-in publishes, and so does one with no root session,
        // the reference's `_root is None` (`vibe/app_server/server.py:1436-1441`).
        let publish = !self.attached_sessions.is_empty() || self.root_key().is_none();
        match target {
            Ok(target) => DispatchBatch {
                outbound: Vec::new(),
                deferred: vec![DeferredWork::McpCatalog {
                    request_id: request.id,
                    call,
                    target,
                    publish,
                }],
                close_after_flush: false,
            },
            Err(error) => catalog_error_batch(request.id, error),
        }
    }

    pub(super) fn resource_request(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.dispatch_resource(request))
    }

    fn dispatch_resource(
        &mut self,
        request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let session_id = request
            .params
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|session_id| !session_id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| ProtocolFault::invalid_params("sessionId must be a non-empty string"))?;
        if let Some(batch) = self.attachment_error(request.id.clone(), &session_id) {
            return Ok(batch);
        }
        // The three live-state reads are composed here rather than inside the
        // resource service: each one needs the configuration, the catalogs or
        // the session's own accounting, and the service holds none of them.
        if let Some(batch) = self.live_state_request(&request, &session_id) {
            return Ok(batch);
        }
        if request.method == "telemetry/record" {
            return self.record_telemetry(request);
        }
        if request.method.starts_with("feedback/") {
            return Ok(self.feedback_request(request, &session_id));
        }
        let session_active = {
            let sessions = self.server.lock_sessions();
            sessions
                .get(&session_id)
                .is_some_and(|session| session.active_turn.is_some())
        };
        if BACKEND_RESOURCE_METHODS.contains(&request.method.as_str())
            && self.server.resource_backend.is_some()
        {
            let command =
                ResourceBackendCommand::parse(&request.method, &request.params, session_active)?;
            return Ok(DispatchBatch {
                outbound: Vec::new(),
                deferred: vec![DeferredWork::ResourceRequest {
                    request_id: request.id,
                    session_id,
                    command,
                }],
                close_after_flush: false,
            });
        }
        let mut result =
            lock(&self.server.resources).dispatch(&request.method, &request.params, session_active);
        // Reference `project_diagnostics`: the hooks the session loaded, and
        // why the rest did not load, ahead of every other issue.
        if request.method == "diagnostics/list"
            && let Ok(dispatch) = result.as_mut()
        {
            let hooks = self
                .server
                .lock_sessions()
                .get(&session_id)
                .map(|session| session.hooks.clone())
                .unwrap_or_default();
            dispatch
                .result
                .insert("hooksCount".to_owned(), json!(hooks.count()));
            if let Some(Value::Array(issues)) = dispatch.result.get_mut("issues") {
                let mut listed = hooks.issues();
                listed.append(issues);
                *issues = listed;
            }
        }
        Ok(resource_result_batch(
            request.id,
            &self.server,
            &session_id,
            result,
        ))
    }

    /// Answers the three methods that report live session state.
    ///
    /// `runtime/read` composes what a client renders a session from,
    /// `stats/read` publishes the same accounting the sequenced statistics
    /// notification carries, and `account/read` classifies the configured
    /// credential. `None` means the request is not one of them.
    fn live_state_request(
        &self,
        request: &ServerRequest,
        session_id: &str,
    ) -> Option<DispatchBatch> {
        let result = match request.method.as_str() {
            "runtime/read" => {
                let Some(mut runtime) = self.server.runtime_snapshot(session_id) else {
                    return Some(error_batch(
                        request.id.clone(),
                        ProtocolErrorCode::NotFound,
                        "Session was not found",
                    ));
                };
                // A Unified Harness session keeps its accounting beside the
                // translator that publishes it.
                if let Some((stats, context_window)) =
                    self.server.unified_session(session_id).map(|unified| {
                        let host = unified.host();
                        (
                            host.translator.stats.to_json(),
                            host.translator.context_window,
                        )
                    })
                {
                    runtime["stats"] = stats;
                    runtime["contextWindow"] = json!(context_window);
                }
                let ready = lock(&self.server.resources).is_ready();
                result_map([
                    ("runtime", runtime),
                    ("sessionLog", self.server.session_log_summary(session_id)),
                    ("ready", json!(ready)),
                ])
            }
            // Reference `_tools_list` and `runtime/read` both read
            // `project_tools`, so the list is the one the runtime publishes.
            "tools/list" => {
                let tools = self
                    .server
                    .runtime_snapshot(session_id)?
                    .get("tools")?
                    .clone();
                result_map([("tools", tools)])
            }
            // Reference `dispatch_extension` on a Unified Harness session
            // serves no statistics read: its client follows
            // `session/statsUpdated` instead.
            "stats/read" if self.server.unified_session(session_id).is_some() => {
                return Some(
                    route::method_not_found(&request.method).into_batch(request.id.clone()),
                );
            }
            "stats/read" => {
                let (stats, context_window) = {
                    let sessions = self.server.lock_sessions();
                    let session = sessions.get(session_id);
                    (
                        public_stats(session),
                        session.map_or(0, |session| session.context_window),
                    )
                };
                let stats = priced(stats, self.server.workspace.active_model_pricing());
                result_map([("stats", stats), ("contextWindow", json!(context_window))])
            }
            // The plan comes from the console and the identity from the
            // provider, so both answers wait on I/O.
            "account/read" | "identity/read" => {
                return Some(DispatchBatch {
                    outbound: Vec::new(),
                    deferred: vec![DeferredWork::CloudRequest {
                        request_id: request.id.clone(),
                        method: request.method.clone(),
                        params: request.params.clone(),
                    }],
                    close_after_flush: false,
                });
            }
            _ => return None,
        };
        Some(success_batch(request.id.clone(), result))
    }

    /// Records a client-reported event against the attached session.
    ///
    /// The reference forwards the event to the agent loop's telemetry client,
    /// which ships it to the datalake under the open-properties envelope
    /// (`vibe/app_server/_resources.py:488-499`). This port now publishes that
    /// envelope too, so the event travels through the sink an adapter
    /// installed, carrying the client's own name and its properties unmodified.
    /// It is also written to the log file an operator reads back through
    /// `diagnostics/logs/read`, and both are dropped when `enable_telemetry` is
    /// off, which is the decision the reference delegates to the same key.
    fn record_telemetry(&mut self, request: ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<TelemetryRecordParams>(&request.params)?;
        if self.server.workspace.telemetry_enabled() {
            let properties = params
                .properties
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let message = format!(
                "client event {} for session {} (correlateLastRequest={}, properties: {properties})",
                params.name, params.session_id, params.correlate_last_request
            );
            lock(&self.server.resources).record_log(LogLevel::Info, &message);
            self.server.client_telemetry.record_client_event(
                &params.name,
                params.properties.into_iter().collect(),
                Some(&params.session_id),
                params.correlate_last_request,
            );
        }
        Ok(success_batch(request.id, BTreeMap::new()))
    }

    /// Reference `_dispatch_feedback`: whether to ask for a rating, from the
    /// session's conversation and the timestamps in the shared cache, and the
    /// record of what the user did with the prompt.
    fn feedback_request(&self, request: ServerRequest, session_id: &str) -> DispatchBatch {
        let cache = vibe_core::feedback::FeedbackCache::new(self.server.workspace.vibe_home());
        let now = i64::try_from(now_millis() / 1_000).unwrap_or(i64::MAX);
        if request.method == "feedback/record" {
            let action = request
                .params
                .get("action")
                .and_then(Value::as_str)
                .and_then(vibe_core::feedback::FeedbackAction::parse);
            let Some(action) = action else {
                return error_batch(
                    request.id,
                    ProtocolErrorCode::InvalidParams,
                    "action must be asked, given, or snoozed",
                );
            };
            cache.record(action, now);
            return DispatchBatch {
                outbound: vec![success_bytes(request.id, BTreeMap::new())],
                deferred: Vec::new(),
                close_after_flush: false,
            };
        }
        let pending = request
            .params
            .get("pendingUserMessages")
            .and_then(Value::as_u64)
            .and_then(|pending| usize::try_from(pending).ok())
            .unwrap_or(0);
        let user_messages = self
            .server
            .lock_sessions()
            .get(session_id)
            .map_or(0, count_user_messages);
        let is_mistral = self
            .server
            .workspace
            .layered_config()
            .load()
            .is_ok_and(|snapshot| {
                vibe_core::telemetry::is_active_model_mistral(&snapshot.effective)
            });
        let show = cache.should_show(
            self.server.client_telemetry.is_active(),
            is_mistral,
            user_messages.saturating_add(pending),
            now,
            vibe_core::feedback::uniform_roll(),
        );
        DispatchBatch {
            outbound: vec![success_bytes(
                request.id,
                result_map([
                    ("show", json!(show)),
                    (
                        "snoozeDurationSeconds",
                        json!(vibe_core::feedback::FEEDBACK_SNOOZED_COOLDOWN_SECONDS),
                    ),
                ]),
            )],
            deferred: Vec::new(),
            close_after_flush: false,
        }
    }

    /// `providerAuth/read` on a unified session: the redacted view of the
    /// provider the session's active model is sent to
    /// (`_dispatch_session_reads` in
    /// `vibe/app_server/_unified_harness_backend_adapter.py`).
    pub(super) fn provider_auth_request(&self, request: ServerRequest) -> DispatchBatch {
        let Some(session_id) = request
            .params
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|session_id| !session_id.is_empty())
        else {
            return error_batch(
                request.id,
                ProtocolErrorCode::InvalidParams,
                "sessionId must be a non-empty string",
            );
        };
        if let Some(batch) = self.attachment_error(request.id.clone(), session_id) {
            return batch;
        }
        match self.server.workspace.provider_auth() {
            Ok(view) => success_batch(request.id, result_map([("auth", view)])),
            Err(message) => ProtocolFault::internal(message).into_batch(request.id),
        }
    }
}
