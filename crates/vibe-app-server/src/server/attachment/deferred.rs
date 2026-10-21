//! The work a dispatch deferred, run off the request loop: tool calls,
//! resource and catalog commands, the requests answered over the network,
//! and the MCP servers a session still has to connect.

use std::pin::Pin;

use super::*;

/// What [`AppServer::execute_deferred`] made of one unit of deferred work.
pub(crate) enum Execution {
    /// The work ran in place.
    Done,
    /// The work settles on its own, with the batch that answers it, and hands
    /// what it publishes before then to the sink it was given. The host runs
    /// it beside its request loop or awaits it.
    Pending(Pin<Box<dyn Future<Output = DispatchBatch> + Send>>),
    /// The work is the host's own: a control of the turn driver the server
    /// does not hold, or a session's resource teardown, which a host orders
    /// against the requests it reads next.
    Host(Box<DeferredWork>),
}

/// Reference `SkillsVersionsParams` and `SkillsDetailParams`: the skill a
/// registry read is about, and for a detail the version it names.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkillParams {
    skill_id: String,
    #[serde(default)]
    version: Option<i64>,
}

impl AppServer {
    /// Runs the part of `work` the server owns, handing every frame it
    /// publishes before it answers to `sink`.
    pub(crate) fn execute_deferred(&self, work: DeferredWork, sink: FrameSink) -> Execution {
        let server = self.clone();
        match work {
            DeferredWork::AttachUnified { session_id } => {
                self.attach_unified(&session_id, sink);
                Execution::Done
            }
            DeferredWork::ReleaseUnifiedCallback {
                session_id,
                callback_id,
                cancel_turn,
            } => {
                self.release_unified_callback(&session_id, &callback_id, cancel_turn);
                Execution::Done
            }
            DeferredWork::RunUnifiedTurn(work) => pending(async move {
                drive_unified_turn(work, sink).await;
                DispatchBatch::default()
            }),
            DeferredWork::ResourceRequest {
                request_id,
                session_id,
                command,
            } => pending(async move {
                server
                    .execute_resource_request(request_id, session_id, command)
                    .await
            }),
            DeferredWork::McpCatalog {
                request_id,
                call,
                target,
                publish,
            } => {
                // A sign-in hands out its URL while the call is still waiting
                // on the browser.
                let notify = mcp_catalog_notifier(publish, move |frame| sink(frame));
                pending(async move {
                    server
                        .execute_mcp_catalog(request_id, call, target, notify)
                        .await
                })
            }
            // A Teleport start answers, then its run publishes events long
            // after this call returned.
            DeferredWork::VibeCode {
                request_id,
                session_id,
                method,
                params,
                launch,
            } => pending(async move {
                server
                    .execute_vibe_code(request_id, session_id, method, params, launch, sink)
                    .await
            }),
            DeferredWork::ShellCommand {
                request_id,
                session_id,
                operation_id,
                command,
                cwd,
                timeout_ms,
            } => pending(async move {
                server
                    .execute_shell_command(
                        request_id,
                        session_id,
                        operation_id,
                        command,
                        cwd,
                        timeout_ms,
                        move |frame| sink(frame),
                    )
                    .await
            }),
            DeferredWork::ShellInterrupt {
                request_id,
                session_id,
                operation_id,
            } => pending(async move {
                server
                    .answer_shell_interrupt(request_id, session_id, operation_id)
                    .await
            }),
            DeferredWork::CloudRequest {
                request_id,
                method,
                params,
            } => pending(async move {
                server
                    .execute_cloud_request(request_id, method, params)
                    .await
            }),
            DeferredWork::ConfigureMcp {
                session_id,
                configs,
            } => pending(async move {
                DispatchBatch {
                    outbound: server.configure_mcp_servers(&session_id, configs).await,
                    ..DispatchBatch::default()
                }
            }),
            work @ (DeferredWork::RunTurn { .. }
            | DeferredWork::InterruptTurn { .. }
            | DeferredWork::SteerTurn { .. }
            | DeferredWork::InjectContext { .. }
            | DeferredWork::ResolveCallback { .. }
            | DeferredWork::CompactSession { .. }
            | DeferredWork::CloseResources { .. }) => Execution::Host(Box::new(work)),
        }
    }

    pub async fn invoke_tool(
        &self,
        session_id: &str,
        name: &str,
        invocation: ToolInvocation,
    ) -> Result<ToolExecutionOutput, ServerError> {
        self.tool_registry(session_id)?
            .invoke(name, invocation)
            .await
            .map_err(|error| ServerError::Tool(error.to_string()))
    }

    pub async fn execute_resource_request(
        &self,
        request_id: RequestId,
        session_id: String,
        command: ResourceBackendCommand,
    ) -> DispatchBatch {
        let Some(backend) = &self.resource_backend else {
            return error_batch(
                request_id,
                ProtocolErrorCode::Conflict,
                "Operational resource backend is not attached",
            );
        };
        let request = ResourceBackendRequest {
            session_id,
            command,
        };
        let session_id = request.session_id.clone();
        resource_result_batch(
            request_id,
            self,
            &session_id,
            backend.dispatch(request).await,
        )
    }

    /// Runs one MCP catalog call and answers it: the requirements its
    /// convergence raised, the answer with the runtime a mutation declares,
    /// and the `runtime/updated` that follows a session's.
    pub async fn execute_mcp_catalog(
        &self,
        request_id: RequestId,
        call: McpCatalogCall,
        target: McpCatalogTarget,
        notify: McpCatalogNotify,
    ) -> DispatchBatch {
        let answers_runtime = call.answers_runtime();
        let session_id = match &target {
            McpCatalogTarget::Session(session_id) => Some(session_id.clone()),
            McpCatalogTarget::Sessionless => None,
        };
        let outcome = match &self.resource_backend {
            Some(backend) => backend.mcp_catalog(call, target, notify).await,
            None => match (&call, &session_id) {
                // With no backend nothing but the recorded state is known, so
                // a read answers it and anything else is refused.
                (McpCatalogCall::Read { .. }, Some(session_id)) => lock(&self.resources)
                    .runtime(session_id)
                    .ok()
                    .and_then(|mut runtime| runtime.remove("mcp"))
                    .map(|mcp| McpCatalogOutcome {
                        result: BTreeMap::from([("mcp".to_owned(), mcp)]),
                        runtime_updated: false,
                        integrations: None,
                        auth_required: Vec::new(),
                    })
                    .ok_or_else(|| {
                        McpCatalogError::refused(
                            ProtocolErrorCode::InternalError,
                            "Resource state is unavailable",
                        )
                    }),
                _ => Err(McpCatalogError::refused(
                    ProtocolErrorCode::NotImplemented,
                    "The MCP catalog is not available on this server",
                )),
            },
        };
        let mut outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => return catalog_error_batch(request_id, error),
        };
        if let (Some(session_id), Some(state)) = (&session_id, outcome.integrations.take()) {
            lock(&self.resources).record_integrations(session_id, state);
        }
        if let Some(session_id) = &session_id {
            self.overlay_session_connectors(session_id, outcome.result.get_mut("mcp"));
        }
        let mut outbound = session_id
            .as_deref()
            .map(|session_id| auth_required_frames(self, session_id, &outcome.auth_required))
            .unwrap_or_default();
        if answers_runtime {
            let runtime = session_id
                .as_deref()
                .and_then(|session_id| self.runtime_snapshot(session_id))
                .unwrap_or(Value::Null);
            outcome.result.insert("runtime".to_owned(), runtime);
        }
        outbound.push(success_bytes(request_id, outcome.result));
        if outcome.runtime_updated
            && let Some(session_id) = &session_id
            && let Some(runtime) = self.runtime_snapshot(session_id)
        {
            outbound.push(encode_notification(
                "runtime/updated",
                BTreeMap::from([
                    ("sessionId".to_owned(), json!(session_id)),
                    ("runtime".to_owned(), runtime),
                ]),
            ));
        }
        DispatchBatch {
            outbound,
            deferred: Vec::new(),
            close_after_flush: false,
        }
    }

    pub async fn execute_cloud_request(
        &self,
        request_id: RequestId,
        method: String,
        params: BTreeMap<String, Value>,
    ) -> DispatchBatch {
        if method == "account/read" {
            let account = self.workspace.read_account().await;
            return success_batch(request_id, result_map([("account", account)]));
        }
        if method.starts_with("connector_catalog/") || method.starts_with("connectors/") {
            let mut params = params;
            let root = params
                .remove(CONNECTION_ROOT_PARAM)
                .and_then(|root| root.as_str().map(str::to_owned));
            return self
                .execute_connector_call(request_id, &method, &params, root)
                .await;
        }
        if method == "skills/catalog" {
            let catalog = self.workspace.skills_catalog().await;
            return success_batch(request_id, object(catalog));
        }
        if method == "skills/updates" {
            let updates = self.workspace.skills_updates().await;
            return success_batch(
                request_id,
                updates
                    .as_object()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
            );
        }
        if method == "skills/versions" || method == "skills/detail" {
            let SkillParams { skill_id, version } = match from_params(&params) {
                Ok(params) => params,
                Err(rejection) => return ProtocolFault::from(rejection).into_batch(request_id),
            };
            let answer = if method == "skills/versions" {
                self.workspace.skills_versions(&skill_id).await
            } else {
                self.workspace.skills_detail(&skill_id, version).await
            };
            return success_batch(request_id, object(answer));
        }
        if crate::server::connection::SKILLS_MUTATIONS.contains(&method.as_str()) {
            return self
                .skills_mutation_batch(request_id, &method, params)
                .await;
        }
        if method == "narration/summarize" {
            return self.narrate(request_id, params).await;
        }
        if method == "identity/read" {
            let identity = self.workspace.read_identity().await;
            return success_batch(request_id, result_map([("identity", identity)]));
        }
        if method.starts_with("projectLinks/") {
            return match crate::projects::links::dispatch(&self.workspace, &method, &params).await {
                Ok(result) => success_batch(request_id, object(result)),
                Err(failure) => plain_error_batch(request_id, failure.code(), failure.message()),
            };
        }
        match self.projects.dispatch(&method, &params) {
            Ok(dispatch) => projects_dispatch_batch(request_id, dispatch),
            Err(error) => projects_error_batch(request_id, error),
        }
    }

    /// Reference `NarrationService.summarize`, against the configuration as
    /// it stands. A summary that cannot be made answers `null`.
    async fn narrate(
        &self,
        request_id: RequestId,
        mut params: BTreeMap<String, Value>,
    ) -> DispatchBatch {
        let launch = params
            .remove(CONNECTION_LAUNCH_PARAM)
            .and_then(|launch| serde_json::from_value(launch).ok());
        let text = |key: &str| params.get(key).and_then(Value::as_str);
        let session_id = text("sessionId").unwrap_or_default();
        let metadata = vibe_core::telemetry::TelemetryContext {
            launch,
            ..Default::default()
        }
        .request_metadata(
            Some(session_id),
            vibe_core::telemetry::TelemetryCallType::SecondaryCall,
            text("messageId").map(ToOwned::to_owned),
        )
        .properties();
        let effective = self
            .workspace
            .layered_config()
            .load()
            .map(|snapshot| snapshot.effective.clone())
            .unwrap_or_default();
        let providers =
            vibe_core::provider::config::ModelRouting::from_effective(&effective, None).providers;
        let context = vibe_core::llm::BackendContext::ambient(
            vibe_core::provider::config::ApiSettings::from_table(&effective),
            self.workspace.credentials(),
        );
        let input = vibe_core::narration::TurnSummaryInput {
            user_message: text("userMessage").unwrap_or_default(),
            assistant_text: text("assistantText").unwrap_or_default(),
            error: text("error"),
        };
        let summary =
            vibe_core::narration::summarize(&providers, &context, &input, &metadata).await;
        success_batch(request_id, result_map([("summary", json!(summary))]))
    }

    /// A skills mutation answered with the runtime it produced, then
    /// published as `runtime/updated` (reference `SkillsController`).
    async fn skills_mutation_batch(
        &self,
        request_id: RequestId,
        method: &str,
        mut params: BTreeMap<String, Value>,
    ) -> DispatchBatch {
        let root = params
            .remove(CONNECTION_ROOT_PARAM)
            .and_then(|root| root.as_str().map(str::to_owned))
            .unwrap_or_default();
        let params = params.into_iter().collect::<serde_json::Map<_, _>>();
        let converted = match self.workspace.skills_mutation(method, &params).await {
            Ok(converted) => converted,
            Err((code, message)) => {
                return ProtocolFault::plain(code, message).into_batch(request_id);
            }
        };
        // Reference `_refreshed` rebuilds the runtime with `reload_hooks=True`.
        if !root.is_empty() {
            let _ = self.reload_session_hooks(&root);
        }
        let runtime = self.runtime_snapshot(&root);
        let mut result = result_map([("runtime", json!(runtime)), ("status", json!("applied"))]);
        if let Some(converted) = converted {
            result.insert("converted".to_owned(), json!(converted));
        }
        let mut outbound = vec![success_bytes(request_id, result)];
        if let Some(runtime) = runtime {
            outbound.push(encode_notification(
                "runtime/updated",
                result_map([("sessionId", json!(root)), ("runtime", runtime)]),
            ));
        }
        DispatchBatch {
            outbound,
            deferred: Vec::new(),
            close_after_flush: false,
        }
    }

    pub async fn close_resource_session(
        &self,
        session_id: &str,
        generation: u64,
    ) -> Result<(), ServerError> {
        // A managed shell session outlives the call that started it, so this is
        // the only place left that can stop one. The same holds for a terminal
        // the client opened on our behalf. A failure in either must not skip the
        // backend teardown, so all three run and the first failure is reported.
        let shell = self
            .shell_tools
            .close_session(session_id)
            .await
            .map_err(|error| ServerError::Resource(error.to_string()));
        let delegated = self
            .client_tools
            .close_session(session_id)
            .await
            .map_err(|error| ServerError::Resource(error.to_string()));
        let backend = match &self.resource_backend {
            Some(backend) => backend
                .close_session(session_id, generation)
                .await
                .map_err(|error| ServerError::Resource(error.to_string())),
            None => Ok(()),
        };
        shell.and(delegated).and(backend)
    }

    /// Configures a session's MCP sources and publishes what changed.
    ///
    /// Discovery is best-effort: a source that will not start is reported as a
    /// `warning` rather than failing the session, which is why this answers with
    /// frames instead of a result.
    pub async fn configure_mcp_servers(
        &self,
        session_id: &str,
        configs: Vec<McpServerConfig>,
    ) -> Vec<Vec<u8>> {
        let warning = |message: String| ResourceSignals {
            runtime_updated: false,
            warnings: vec![message],
            auth_required: Vec::new(),
            integrations: None,
        };
        let mut signals = match &self.resource_backend {
            None => warning("MCP transport backend is not configured".to_owned()),
            Some(backend) => match backend.configure_mcp(session_id, configs).await {
                Ok(dispatch) => dispatch.signals,
                Err(error) => warning(redact(&error.to_string())),
            },
        };
        if let Some(state) = signals.integrations.take() {
            lock(&self.resources).record_integrations(session_id, state);
        }
        signal_frames(self, session_id, &signals)
    }
}

fn pending(work: impl Future<Output = DispatchBatch> + Send + 'static) -> Execution {
    Execution::Pending(Box::pin(work))
}
