//! The reference's request routing, as a stdio client meets it.
//!
//! A stdio client speaks the reference protocol and nothing else, so every
//! request it sends is routed the way `vibe/app_server/server.py`
//! (`_dispatch_request` and the helpers under it) routes one on the legacy
//! backend: which methods the host answers without a session, which need the
//! connection's root, how the parameters are validated before anything reads
//! them, and which session a method may name. Once a request clears that, the
//! handler this port already had answers it with the validated parameters.
//!
//! In-process clients are this port's own adapters and keep the port's
//! dispatch ([`ServerConnection::port_request`]).

use super::saved::{NO_ROOT, reopen_start_params};
use super::turn::SteerAnswer;
use super::*;
use crate::params::object_of;
use crate::server::session_management;
use crate::wire_validation::{self, Json};

/// How the reference reaches a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// `events/read`, which answers an empty batch (`_events_read`).
    Events,
    /// A catalog service the server holds whatever session is attached: the
    /// MCP and connector catalogs, and a unified session's plugin catalog. A
    /// session the parameters name has to be the root.
    Catalog,
    /// The plugin catalog, which checks the session it is given and then
    /// declines: the legacy backend has no plugins.
    DeclinedCatalog,
    /// The host, which answers without a session.
    Host,
    /// The host when no session is named, the root when one is
    /// (`_SESSION_OPTIONAL_METHODS`).
    SessionOptional,
    /// The root: refused before validation when there is none, and the
    /// session named has to be the root.
    Root,
    /// Addressed to the root, which the legacy backend does not serve.
    RootUnserved,
    /// Addressed to the root, which declines it before reading it.
    RootDeclined,
    /// Served by neither the host nor the legacy backend.
    Unserved,
}

/// The `skills/*` methods that change what the session loads, each answered
/// with the runtime it produced (reference `SkillsController.dispatch`).
pub(crate) const SKILLS_MUTATIONS: &[&str] = &[
    "skills/convertLocal",
    "skills/import",
    "skills/remove",
    "skills/setAlias",
    "skills/setEnabled",
    "skills/setLatest",
    "skills/setVersion",
];

/// The methods reference `ConnectorCatalogService` answers.
pub(crate) fn is_connector_method(method: &str) -> bool {
    method.starts_with("connector_catalog/")
        || matches!(
            method,
            "connectors/read" | "connectors/refresh" | "connectors/auth/read"
        )
}

/// The route the reference gives `method`, a name it declares, on the
/// unified host when `unified` and on the legacy one otherwise.
pub(crate) fn route(method: &str, unified: bool) -> Route {
    match method {
        // A unified session serves its plugins and the provider view the
        // legacy one declines, folds a queued turn into the running one, and
        // runs background processes.
        "plugin_catalog/read" | "plugins/read" if unified => Route::Catalog,
        "plugin/info"
        | "plugin/reload"
        | "providerAuth/read"
        | "session/turn/queue/steer"
        | "session/backgroundProcess/output"
        | "session/backgroundProcess/stop"
            if unified =>
        {
            Route::Root
        }
        // The marks a unified host keeps beside each session are its own
        // methods, served for any session it stores.
        "session/pin" | "session/archive" | "session/markAsSeen" if unified => Route::Host,
        "events/read" => Route::Events,
        "plugin_catalog/read" | "plugins/read" => Route::DeclinedCatalog,
        // The onboarding wizard runs before any session, so the setup methods
        // never build or read one (`vibe/app_server/server.py:765-775`).
        "config/schema"
        | "session/history/get"
        | "setup/status"
        | "setup/store-credential"
        | "setup/submit-choices"
        | "workspace/git/checkouts"
        | "workspace/git/worktrees/limit/update"
        | "workspace/git/worktrees/list"
        | "workspace/git/worktrees/prune"
        | "workspace/git/worktrees/reap"
        | "workspace/git/worktrees/reap/cancel"
        | "workspace/git/worktrees/remove"
        | "workspace/trust/status"
        | "workspace/trust/untrustedConfig"
        | "session/list"
        | "session/read"
        | "session/history/list"
        | "session/rename"
        | "session/relocate"
        | "session/fork"
        | "session/delete"
        | "session/start"
        | "session/resume"
        | "session/continue" => Route::Host,
        "agents/list" | "config/read" | "workspace/trust/decision" => Route::SessionOptional,
        // Only a Unified Harness session runs background processes; the legacy
        // one asks for a session first and then knows no such method.
        "shell/run"
        | "shell/interrupt"
        | "session/title/update"
        | "session/backgroundProcess/output"
        | "session/backgroundProcess/stop" => Route::RootUnserved,
        "plugin/info" | "plugin/reload" | "providerAuth/read" | "session/turn/queue/steer" => {
            Route::RootDeclined
        }
        // The legacy session host implements neither the pin, the archive nor
        // the seen marks, and says so before reading the request
        // (`vibe/app_server/server.py:922` and `973-982`).
        "session/pin" | "session/archive" | "session/markAsSeen" => Route::Unserved,
        method if method.starts_with("projectLinks/") => Route::Host,
        method
            if method.starts_with("mcp/")
                || method.starts_with("mcp_catalog/")
                || method.starts_with("connector_catalog/")
                || method.starts_with("connectors/") =>
        {
            Route::Catalog
        }
        _ => Route::Root,
    }
}

impl ServerConnection {
    /// Answers a request from a stdio client, which is routed as the
    /// reference routes it.
    ///
    /// `ordered` carries the parameters in the order the client wrote them,
    /// which is the order pydantic reports extra keys in.
    pub(super) fn reference_request(
        &mut self,
        request: ServerRequest,
        ordered: Option<Json>,
    ) -> DispatchBatch {
        let id = request.id.clone();
        let method = request.method.clone();
        let params = ordered
            .filter(|params| matches!(params, Json::Object(_)))
            .unwrap_or_else(|| Json::from_value(&Value::Object(object_of(&request.params))));
        let mut batch = answered(id, self.route_request(request, &params));
        // `session/stop` closes the server once it is answered, whatever the
        // answer was (`_handle_request_once`).
        if method == "session/stop" {
            self.close();
            batch.close_after_flush = true;
        }
        batch
    }

    fn route_request(
        &mut self,
        request: ServerRequest,
        params: &Json,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let method = request.method.clone();
        let method = method.as_str();
        let rooted = self.root_key().is_some();
        let unified = self.server.harness_selection().use_unified;
        let routed = route(method, unified);
        match routed {
            Route::Unserved => return Err(method_not_found(method)),
            Route::RootUnserved | Route::RootDeclined | Route::Root if !rooted => {
                return Err(ProtocolFault::plain(ProtocolErrorCode::Conflict, NO_ROOT));
            }
            Route::RootUnserved => return Err(method_not_found(method)),
            // Reference `_dispatch` on the legacy handler declines the whole
            // `plugin` namespace in one sentence.
            Route::RootDeclined if method.starts_with("plugin/") => {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::NotImplemented,
                    format!("Plugins are not supported: {method}"),
                ));
            }
            // So does it decline the `providerAuth` namespace, which only a
            // unified session projects.
            Route::RootDeclined if method.starts_with("providerAuth/") => {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::NotImplemented,
                    format!("Provider auth is not supported: {method}"),
                ));
            }
            Route::RootDeclined => {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::NotImplemented,
                    format!("The selected session backend does not support {method}"),
                ));
            }
            _ => {}
        }
        let validated = wire_validation::validate_method(method, params)
            .map_err(|issues| self.rejected(method, &issues))?;
        let named = validated
            .get("sessionId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        match routed {
            Route::Events => {
                return Ok(success_batch(
                    request.id,
                    result_map([("type", json!("events")), ("events", json!([]))]),
                ));
            }
            Route::DeclinedCatalog => {
                self.require_named_root(named.as_deref())?;
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::NotImplemented,
                    "The selected session backend resolves no plugins",
                ));
            }
            // A session-optional method naming a session is the root's, and
            // the host refuses any other (`_read_config`, `_list_agents`).
            // Reference `SessionCoordinator.turns` also lists the turns of a
            // subagent the root delegated to.
            Route::Root
                if method == "session/turns/list"
                    && named
                        .as_deref()
                        .is_some_and(|id| matches!(self.child_turns(id), Ok(Some(_)))) => {}
            // A callback a subagent opened is answered under the
            // subagent's session (`_callback_events` on a child event).
            Route::Root
                if method == "callback/result"
                    && named.as_deref().is_some_and(|id| self.unified_child(id)) => {}
            Route::Catalog | Route::Root | Route::SessionOptional => {
                self.require_named_root(named.as_deref())?;
            }
            _ => {}
        }
        let mut request = ServerRequest {
            params: object(validated),
            ..request
        };
        // The attached session asks the worktree removal questions, and its own
        // holder is never in the way of them (`vibe/app_server/server.py:765-775`).
        if method == "workspace/git/worktrees/remove"
            && let Some(root) = self.root_id()
        {
            request
                .params
                .insert(crate::server::CONNECTION_ROOT_PARAM.to_owned(), json!(root));
        }
        // A unified root serves its turns, their queue, its loops, its
        // compactions and its rewinds itself.
        if let Some(root) = self
            .root_id()
            .and_then(|root| self.server.unified_session(&root))
        {
            match method {
                "turn/start" => return self.unified_turn_start(request, root),
                "turn/interrupt" => return self.unified_turn_interrupt(request, root),
                "turn/steer" => return self.unified_turn_steer(request, root),
                "callback/result" => return self.unified_callback_result(request, root),
                "session/context/inject" => return self.unified_context_inject(&request, &root),
                "session/compact" => return self.unified_compact(&request, root),
                "session/rewind" => return self.unified_rewind(&request, root),
                "session/rewind/read" => return self.unified_rewind_read(&request, &root),
                "session/turn/enqueue" => return self.unified_queue(request, root),
                method if method.starts_with("session/turn/queue/") => {
                    return self.unified_queue(request, root);
                }
                method if method.starts_with("loops/") => {
                    return self.unified_loops(&request, &root);
                }
                _ => {}
            }
        }
        match method {
            "session/start" => self.reference_session_start(request),
            "turn/interrupt" => {
                let turn_id = self.require_turn_route(&request)?;
                self.reference_turn_interrupt(request, turn_id)
            }
            "turn/steer" => {
                self.require_turn_route(&request)?;
                Ok(self.steer_turn(request, SteerAnswer::LastEvent))
            }
            "callback/result" => self.reference_callback_result(request),
            "workspace/prompt/prepare" => self.reference_prompt_prepare(&request),
            "session/shellCommand" => self.reference_shell_command(request),
            "narration/summarize" => Ok(self.narration_deferred(request)),
            "skills/installed" => {
                let result = self.server.workspace.skills_installed();
                Ok(success_batch(request.id, object(result)))
            }
            // Reference `SkillsController._begin_mutation`: a skills mutation
            // waits for the session to be idle.
            method if SKILLS_MUTATIONS.contains(&method) => {
                self.require_root_idle()?;
                Ok(self.cloud_request(request))
            }
            // The registry and the connector bootstrap answer over the network.
            "skills/catalog" | "skills/updates" | "skills/versions" | "skills/detail" => {
                Ok(self.cloud_request(request))
            }
            method if is_connector_method(method) => Ok(self.cloud_request(request)),
            // Reference `_dispatch_backend_host_lifecycle`: the root is the
            // session named, and the connection closes once this is answered.
            "session/stop" => Ok(success_batch(
                request.id,
                result_map([("closed", json!(true))]),
            )),
            "session/agent/update" | "agents/install" | "agents/uninstall" => {
                self.reference_runtime_mutation(request)
            }
            // Reference `dispatch_extension` on a Unified Harness session serves
            // no review: the edits a turn made are not tracked there.
            method
                if method.starts_with("review/")
                    && named
                        .as_deref()
                        .is_some_and(|id| self.server.unified_session(id).is_some()) =>
            {
                Err(method_not_found(method))
            }
            "plugin_catalog/read" | "plugins/read" | "plugin/info" | "plugin/reload" if unified => {
                Ok(self.plugin_request(request))
            }
            "providerAuth/read" if unified => Ok(self.provider_auth_request(request)),
            "session/backgroundProcess/output" | "session/backgroundProcess/stop" if unified => {
                self.unified_background_process(&request)
            }
            "session/list" if unified => self.unified_list(&request),
            "session/history/list" if unified => self.unified_history_list(&request),
            "session/turns/list" if unified => self.unified_turns_list(&request),
            "session/fork" if unified => self.unified_fork(&request),
            "session/delete" if unified => self.unified_delete_session(&request),
            "session/history/clear" if unified => self.unified_clear(&request),
            "session/resume" if unified => self.unified_resume(request),
            "session/continue" if unified => self.unified_continue(request),
            "session/read" if unified => self.unified_read(&request),
            "session/rename" if unified => self.unified_rename(&request),
            "session/pin" | "session/archive" | "session/markAsSeen" if unified => {
                self.unified_mark(&request)
            }
            _ => Ok(self.port_request(request)),
        }
    }

    /// A request the registry or the connector bootstrap answers off the
    /// request loop, told which session is the connection's root when there
    /// is one: a catalog method is served without.
    fn cloud_request(&self, request: ServerRequest) -> DispatchBatch {
        let mut params = request.params;
        if let Some(root) = self.root_id() {
            params.insert(CONNECTION_ROOT_PARAM.to_owned(), json!(root));
        }
        DispatchBatch {
            outbound: Vec::new(),
            deferred: vec![DeferredWork::CloudRequest {
                request_id: request.id,
                method: request.method,
                params,
            }],
            close_after_flush: false,
        }
    }

    /// Reference `SessionExecution.require_idle` for the root: refused while
    /// a turn, a manual shell or a compaction runs on it.
    fn require_root_idle(&self) -> Result<(), ProtocolFault> {
        let root = self.root_id_required()?;
        let sessions = self.server.lock_sessions();
        let Some(session) = sessions.get(&root) else {
            return Ok(());
        };
        let busy = |what: String| {
            ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                format!("Session is busy running {what}"),
            )
        };
        if let Some(turn_id) = &session.active_turn {
            return Err(busy(format!("turn {turn_id}")));
        }
        if let Some(shell) = &session.shell_operation {
            return Err(busy(format!("shell {}", shell.id)));
        }
        if let Some(operation_id) = &session.teleport_operation {
            return Err(busy(format!("teleport {operation_id}")));
        }
        if session.compaction_pending {
            return Err(busy("lifecycle compact".to_owned()));
        }
        Ok(())
    }

    /// Reference `_require_turn_route`: the root runs a turn, and it is the
    /// one the client expects. Answers that turn.
    fn require_turn_route(&self, request: &ServerRequest) -> Result<String, ProtocolFault> {
        let expected = from_params::<ExpectedTurn>(&request.params)?.expected_turn_id;
        let root = self.root_id_required()?;
        let sessions = self.server.lock_sessions();
        let active = sessions
            .get(&root)
            .and_then(|session| session.active_turn.clone());
        match active {
            None => Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "No active turn",
            )),
            Some(active) if active != expected => Err(ProtocolFault::with_data(
                ProtocolErrorCode::StaleTurn,
                "Active turn does not match expectedTurnId",
                json!({"activeTurnId": active}),
            )),
            Some(active) => Ok(active),
        }
    }

    /// Reference `turn/interrupt`: the turn is asked to stop and settles on
    /// its own, as interrupted, once it has.
    fn reference_turn_interrupt(
        &mut self,
        request: ServerRequest,
        turn_id: String,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let root = self.root_id_required()?;
        let sessions = self.server.lock_sessions();
        let session = sessions
            .get(&root)
            .ok_or_else(|| session_missing("Session not found"))?;
        let result = result_map([
            ("accepted", json!(true)),
            ("lastEventId", json!(session.event_watermark)),
        ]);
        let session_id = session.id.clone();
        drop(sessions);
        let mut batch = success_batch(request.id, result);
        batch.deferred.push(DeferredWork::InterruptTurn {
            session_id,
            turn_id,
        });
        Ok(batch)
    }

    /// Reference `ShellRequestHandler.dispatch`: an interrupt is answered at
    /// once, whether or not the operation still runs; a command reserves the
    /// session, runs outside the request loop and answers once it settles.
    fn reference_shell_command(
        &mut self,
        request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let root = self.root_id_required()?;
        let params = from_params::<ShellCommandParams>(&request.params)?;
        let mut sessions = self.server.lock_sessions();
        let session = sessions
            .get_mut(&root)
            .ok_or_else(|| session_missing("Session not found"))?;
        let (command, operation_id, timeout_seconds, cwd) = match params {
            ShellCommandParams::Interrupt { operation_id } => {
                return Ok(Self::shell_interrupt(request.id, session, operation_id));
            }
            ShellCommandParams::Run {
                command,
                operation_id,
                timeout_seconds,
                cwd,
            } => (command, operation_id, timeout_seconds, cwd),
        };
        let operation_id = operation_id.unwrap_or_else(vibe_core::session_id::uuid_v4);
        let timeout_seconds =
            timeout_seconds.unwrap_or(super::super::manual_shell::DEFAULT_TIMEOUT_SECONDS);
        let cwd = super::super::manual_shell::resolve_workspace_cwd(
            Path::new(&session.working_directory),
            cwd.as_deref(),
        )?;
        if let Some(active) = &session.shell_operation {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                format!("Session is already running shell {}", active.id),
            ));
        }
        if let Some(turn_id) = &session.active_turn {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                format!("Session is already running turn {turn_id}"),
            ));
        }
        if let Some(operation_id) = &session.teleport_operation {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                format!("Session is already running teleport {operation_id}"),
            ));
        }
        session.shell_operation = Some(ShellOperation {
            id: operation_id.clone(),
            interrupted: Arc::default(),
        });
        Ok(DispatchBatch {
            outbound: Vec::new(),
            deferred: vec![DeferredWork::ShellCommand {
                request_id: request.id,
                session_id: session.id.clone(),
                operation_id,
                command,
                cwd: cwd.to_string_lossy().into_owned(),
                // Whole milliseconds; the schema caps a timeout at ten minutes.
                timeout_ms: (timeout_seconds * 1000.0).round() as u64,
            }],
            close_after_flush: false,
        })
    }

    /// The interrupt half of [`Self::reference_shell_command`]: the operation
    /// still running is asked to stop and answers once it has, any other is
    /// answered at once.
    fn shell_interrupt(
        request_id: RequestId,
        session: &SessionRuntime,
        operation_id: String,
    ) -> DispatchBatch {
        if let Some(operation) = &session.shell_operation
            && operation.id == operation_id
        {
            operation
                .interrupted
                .store(true, std::sync::atomic::Ordering::Release);
            return DispatchBatch {
                outbound: Vec::new(),
                deferred: vec![DeferredWork::ShellInterrupt {
                    request_id,
                    session_id: session.id.clone(),
                    operation_id,
                }],
                close_after_flush: false,
            };
        }
        success_batch(
            request_id,
            result_map([
                ("accepted", json!(true)),
                ("lastEventId", json!(session.event_watermark)),
            ]),
        )
    }

    /// Reference `prepare_prompt` (`vibe/app_server/_workspace.py`): the
    /// message goes out as typed, with the images it mentions attached and
    /// what it mentions counted.
    fn reference_prompt_prepare(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let root = self.root_id_required()?;
        let PromptPrepareParams { message } = from_params(&request.params)?;
        let message = message.as_str();
        let working_directory = self
            .server
            .lock_sessions()
            .get(&root)
            .map(|session| PathBuf::from(&session.working_directory))
            .unwrap_or_default();
        let payload =
            vibe_core::path_resources::build_path_prompt_payload(&working_directory, message);
        let refused =
            |message: String| ProtocolFault::plain(ProtocolErrorCode::InvalidParams, message);
        let images = payload
            .resources
            .iter()
            .filter(|resource| resource.kind == vibe_core::path_resources::PathResourceKind::Image)
            .collect::<Vec<_>>();
        if images.len() > vibe_core::images::MAX_IMAGES_PER_MESSAGE {
            return Err(refused(format!(
                "Too many image attachments (got {}, max {}).",
                images.len(),
                vibe_core::images::MAX_IMAGES_PER_MESSAGE
            )));
        }
        let session_dir = self
            .server
            .workspace
            .persists_runtime_sessions()
            .then(|| {
                self.server
                    .workspace
                    .session_store()
                    .session_directory(&root)
                    .ok()
            })
            .flatten();
        let images = images
            .into_iter()
            .map(|resource| {
                crate::images::snapshot_image_file(
                    &resource.path,
                    &resource.alias,
                    session_dir.as_deref(),
                )
                .map_err(|error| {
                    refused(format!(
                        "Failed to attach image {}: {error}",
                        resource.alias
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !images.is_empty() {
            let runtime = self.server.runtime_snapshot(&root);
            let config = runtime.as_ref().and_then(|runtime| runtime.get("config"));
            if config.and_then(|config| config.get("imagesSupported")) == Some(&json!(false)) {
                let model = config
                    .and_then(|config| config.pointer("/activeModel/displayName"))
                    .or_else(|| config.and_then(|config| config.pointer("/activeModel/alias")))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                return Err(refused(format!(
                    "Model `{model}` does not support images. Switch with /model or remove the attachment."
                )));
            }
        }
        Ok(success_batch(
            request.id.clone(),
            result_map([(
                "prompt",
                json!({
                    "displayText": message,
                    "promptText": message,
                    "images": images,
                    // The title is generated once a transcript exists.
                    "autoTitle": null,
                    "mentions": payload.mention_stats(),
                }),
            )]),
        ))
    }

    /// A mutation the reference answers with the runtime it changed and then
    /// publishes as `runtime/updated`: the agent switch answers the runtime
    /// itself (`RuntimeMutationResponse`), the agent catalog edits the
    /// catalog.
    fn reference_runtime_mutation(
        &mut self,
        request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let root = self.root_id_required()?;
        let id = request.id.clone();
        let switch = request.method == "session/agent/update";
        let answered = match session_management::answer(self, request) {
            Ok(answered) => answered,
            Err(refused) => return Ok(refused),
        };
        let runtime = self.server.runtime_snapshot(&root);
        let result = if switch {
            result_map([("runtime", json!(runtime)), ("status", json!("applied"))])
        } else {
            answered.result
        };
        // None of these methods publishes a runtime of its own, so the one
        // pushed here is the only one the client reads.
        let mut batch = success_batch(id, result);
        batch.outbound.extend(answered.then);
        if let Some(runtime) = runtime {
            batch.outbound.push(encode_notification(
                "runtime/updated",
                result_map([("sessionId", json!(root)), ("runtime", runtime)]),
            ));
        }
        Ok(batch)
    }

    /// Reference `_dispatch_narration`: the summary is a model call, so it is
    /// answered off the request loop, with the client's launch context.
    pub(super) fn narration_deferred(&self, request: ServerRequest) -> DispatchBatch {
        let mut params = request.params;
        if let Some(launch) = &self.launch {
            params.insert(
                CONNECTION_LAUNCH_PARAM.to_owned(),
                serde_json::to_value(launch).unwrap_or(Value::Null),
            );
        }
        DispatchBatch {
            outbound: Vec::new(),
            deferred: vec![DeferredWork::CloudRequest {
                request_id: request.id,
                method: request.method,
                params,
            }],
            close_after_flush: false,
        }
    }

    /// The parameters an in-process narration call is held to, which the
    /// reference routing applies over stdio.
    pub(super) fn narration_request(&self, request: ServerRequest) -> DispatchBatch {
        let params = Json::from_value(&Value::Object(object_of(&request.params)));
        if let Err(issues) = wire_validation::validate_method(&request.method, &params) {
            return self
                .rejected(&request.method, &issues)
                .into_batch(request.id);
        }
        self.narration_deferred(request)
    }

    /// The refusal of parameters that did not validate, which the reference
    /// also records in its log (`_dispatch_or_error`), where
    /// `diagnostics/logs/read` finds it.
    pub(in crate::server) fn rejected(
        &self,
        method: &str,
        issues: &[wire_validation::Issue],
    ) -> ProtocolFault {
        let detail = issues
            .iter()
            .map(|issue| {
                let path = issue
                    .path
                    .iter()
                    .map(|segment| match segment {
                        PathSegment::Field(name) => name.clone(),
                        PathSegment::Index(index) => index.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(".");
                format!("{path}: {}", issue.message)
            })
            .collect::<Vec<_>>()
            .join("; ");
        lock(&self.server.resources).record_log(
            LogLevel::Warning,
            &format!("Refused {method}, its parameters did not validate: {detail}"),
        );
        ProtocolFault::from(ParamsRejection::with_issues(
            issues.iter().map(wire_validation::Issue::wire).collect(),
        ))
    }

    /// Reference `_require_session`: a session the parameters name has to be
    /// the root.
    fn require_named_root(&self, named: Option<&str>) -> Result<(), ProtocolFault> {
        let Some(named) = named else {
            return Ok(());
        };
        if self.root_id().as_deref() == Some(named) {
            Ok(())
        } else {
            Err(ProtocolFault::plain(
                ProtocolErrorCode::NotFound,
                format!("Session not found: {named}"),
            ))
        }
    }

    /// Reference `session/start` on the legacy host: the `agentConfig` a
    /// client sends is the start this port already knows, spelled under one
    /// key, and a connection holds one root.
    fn reference_session_start(
        &mut self,
        request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        if self.root_key().is_some() {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "A session is already attached",
            ));
        }
        let start = from_params::<StartParams>(&request.params)?;
        let mut params = reopen_start_params(&start.agent_config);
        if let Some(limit) = start.history_limit {
            params.insert("historyLimit".to_owned(), json!(limit));
        }
        if self.server.harness_selection().use_unified {
            return self.unified_session_start(ServerRequest { params, ..request });
        }
        self.open_reopened(ServerRequest { params, ..request }, false)
    }
}

/// The refusal a method the reference declares but does not serve here gets.
pub(super) fn method_not_found(method: &str) -> ProtocolFault {
    ProtocolFault::plain(
        ProtocolErrorCode::MethodNotFound,
        format!("Method not found: {method}"),
    )
}

/// Reference `TurnInterruptParams` and `TurnSteerParams`, for the turn the
/// client expects to be running.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedTurn {
    expected_turn_id: String,
}

/// Reference `SessionShellCommandParams`, read as the action it asks for: an
/// interrupt names its operation, a run its command (`validate_action`).
#[derive(Deserialize)]
#[serde(try_from = "ShellCommandFields")]
enum ShellCommandParams {
    Interrupt {
        operation_id: String,
    },
    Run {
        command: String,
        operation_id: Option<String>,
        timeout_seconds: Option<f64>,
        cwd: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellCommandFields {
    #[serde(default)]
    action: ShellAction,
    command: Option<String>,
    cwd: Option<String>,
    timeout_seconds: Option<f64>,
    operation_id: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ShellAction {
    #[default]
    Run,
    Interrupt,
}

impl TryFrom<ShellCommandFields> for ShellCommandParams {
    type Error = &'static str;

    fn try_from(fields: ShellCommandFields) -> Result<Self, Self::Error> {
        match fields.action {
            ShellAction::Interrupt => Ok(Self::Interrupt {
                operation_id: fields
                    .operation_id
                    .ok_or("an interrupt names its operation")?,
            }),
            ShellAction::Run => Ok(Self::Run {
                command: fields.command.ok_or("a run names its command")?,
                operation_id: fields.operation_id,
                timeout_seconds: fields.timeout_seconds,
                cwd: fields.cwd,
            }),
        }
    }
}

/// Reference `WorkspacePromptPrepareParams`, for the message it prepares.
#[derive(Deserialize)]
struct PromptPrepareParams {
    message: String,
}

/// Reference `SessionStartParams` on the legacy host, as far as it carries
/// over to the start this port knows.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartParams {
    #[serde(default)]
    agent_config: serde_json::Map<String, Value>,
    history_limit: Option<u64>,
}
