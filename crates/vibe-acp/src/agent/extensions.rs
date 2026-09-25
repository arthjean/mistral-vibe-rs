//! The `_`-prefixed extension methods an editor calls beside the protocol.
//!
//! Reference `VibeAcpAgent.ext_method` and the `_*_extension` helpers it
//! dispatches to. Each family validates its parameters the way the
//! reference's request model does, then delegates to the app server: the
//! session's own service when one is named, a throwaway one otherwise, which
//! is what the reference's passive host is for.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use vibe_app_server::client::{ClientError, HeadlessService, TurnDriver};
use vibe_core::observability::{
    LogLevel, log_level_chain, set_config_log_level, set_session_log_level,
};
use vibe_protocol::ProtocolErrorCode;

use crate::agent::AcpAgent;
use crate::projection::iso_timestamp;
use crate::protocol::AcpError;

/// The notification that tells the client narration stopped.
const NARRATION_DONE_METHOD: &str = "_voice/narrationDone";

impl<D> AcpAgent<D>
where
    D: TurnDriver + 'static,
{
    /// Reference `ext_method`. `method` is the name without its leading
    /// underscore.
    pub(crate) async fn ext_method(
        self: &Arc<Self>,
        method: &str,
        params: Map<String, Value>,
    ) -> Result<Value, AcpError> {
        match method {
            "auth/status" => self.auth.status_payload(),
            "auth/signOut" => {
                self.auth.sign_out()?;
                Ok(json!({}))
            }
            "config/schema" => self.config_schema().await,
            "session/set_title" => self.set_title(&params).await,
            "session/delete" => {
                let session_id = text(&params, "sessionId")?;
                self.delete_session(&session_id).await?;
                Ok(json!({}))
            }
            "trust/status" | "trust/decision" => self.trust_extension(method, &params).await,
            "rewind/preview" | "rewind/to" => self.rewind_extension(method, &params).await,
            "review/state" | "review/baseline" | "review/turnDiff" | "review/hunks"
            | "review/approve" | "review/revert" => self.review_extension(method, &params).await,
            _ if method.starts_with("loops/") => self.loops_extension(method, &params).await,
            _ if method.starts_with("connectors/") => {
                self.connectors_extension(method, &params).await
            }
            _ if method.starts_with("projectLinks/") => {
                self.project_links_extension(method, &params).await
            }
            _ if method.starts_with("identity/") || method.starts_with("account/") => {
                self.whoami_extension(method, &params).await
            }
            _ if method.starts_with("logLevel/") => self.log_level_extension(method, &params).await,
            _ if method.starts_with("voice/") => self.voice_extension(method, &params),
            _ => Err(AcpError::NotImplemented(method.to_owned())),
        }
    }

    /// Runs `work` against a service no session is attached to, started in the
    /// process's working directory.
    async fn host_call(&self, method: &str, params: Value) -> Result<Map<String, Value>, AcpError> {
        let cwd = std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_owned());
        let mut host: HeadlessService<D> = self.new_service(&cwd, &[])?;
        let result = host.public_call_async(method, params).await;
        let _ = host.shutdown();
        Ok(result?.result.into_iter().collect())
    }

    /// Reference `_config_schema`.
    async fn config_schema(&self) -> Result<Value, AcpError> {
        let mut result = self.host_call("config/schema", json!({})).await?;
        Ok(json!({
            "version": result.remove("configSchemaVersion").unwrap_or(Value::Null),
            "schema": result.remove("schema").unwrap_or(Value::Null),
        }))
    }

    /// Reference `session/set_title`: a live session is renamed through its
    /// own service, any other saved session through the host.
    async fn set_title(&self, params: &Map<String, Value>) -> Result<Value, AcpError> {
        let session_id = text(params, "sessionId")?;
        let title = text(params, "title")?;
        let live = self.find_live_session(&session_id)?;
        let renamed = match &live {
            Some(harness) => self
                .call_async(harness, "session/title/update", json!({"title": title}))
                .await
                .map(|dispatch| dispatch.result.into_iter().collect::<Map<_, _>>()),
            None => {
                self.host_call(
                    "session/title/update",
                    json!({"sessionId": session_id, "title": title}),
                )
                .await
            }
        };
        let renamed = renamed.map_err(|error| match protocol_code(&error) {
            Some(ProtocolErrorCode::NotFound) => AcpError::SessionNotFound(session_id.clone()),
            _ => AcpError::InvalidParams(error_message(&error)),
        })?;
        let title = renamed
            .get("title")
            .and_then(Value::as_str)
            .map_or(title.clone(), ToOwned::to_owned);
        let updated_at = match renamed.get("updatedAt") {
            Some(Value::Number(millis)) => millis.as_u64().map(iso_timestamp),
            Some(Value::String(stamp)) => Some(stamp.clone()),
            _ => None,
        }
        .unwrap_or_else(|| iso_timestamp(crate::agent::turn::now_millis()));
        if let Some(harness) = &live {
            harness.set_display_title(&title);
        }
        let target = live
            .as_ref()
            .map_or(session_id, |harness| harness.session_id.clone());
        self.session_update(
            &target,
            json!({
                "sessionUpdate": "session_info_update",
                "title": title,
                "updatedAt": updated_at,
            }),
        );
        Ok(json!({}))
    }

    /// Reference `_delete_session`: a live session is closed first and its
    /// saved transcript deleted after.
    async fn delete_session(&self, session_id: &str) -> Result<(), AcpError> {
        let Some(harness) = self.find_live_session(session_id)? else {
            return self
                .host_call("session/delete", json!({"sessionId": session_id}))
                .await
                .map(|_| ())
                .or_else(ignore_missing);
        };
        let saved = harness.canonical_id();
        self.close_session(&harness.session_id).await?;
        self.host_call("session/delete", json!({"sessionId": saved}))
            .await
            .map(|_| ())
            .or_else(ignore_missing)
    }

    /// Reference `_trust_extension`.
    async fn trust_extension(
        &self,
        method: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, AcpError> {
        let requested = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .filter(|value| truthy(value));
        let harness = match requested {
            Some(Value::String(session_id)) => Some(self.session_harness(session_id)?),
            _ => None,
        };
        let mut located = Map::new();
        if let Some(cwd) = params.get("cwd").filter(|cwd| !cwd.is_null()) {
            located.insert("cwd".to_owned(), cwd.clone());
        }
        let response = if method == "trust/status" {
            match &harness {
                // Reference `trust_status` asks about the session's own
                // directory when the client names none.
                Some(harness) => self
                    .call_unscoped(harness, "workspace/trust/status", {
                        located.entry("cwd").or_insert_with(|| json!(harness.cwd));
                        Value::Object(located)
                    })
                    .await?
                    .into_iter()
                    .collect::<Map<_, _>>(),
                None => {
                    self.host_call("workspace/trust/status", Value::Object(located))
                        .await?
                }
            }
        } else {
            let Some(harness) = &harness else {
                return Err(AcpError::InvalidParams(
                    "a trust decision must name a live sessionId".to_owned(),
                ));
            };
            located.insert(
                "decision".to_owned(),
                params.get("decision").cloned().unwrap_or(Value::Null),
            );
            self.call(harness, "workspace/trust/decision", Value::Object(located))
                .await
                .map_err(|error| AcpError::InvalidParams(error_message(&error)))?
                .into_iter()
                .collect()
        };
        Ok(json!({
            "trust_status": response.get("status").cloned().unwrap_or(Value::Null),
            "details": response.get("details").cloned().unwrap_or(Value::Null),
        }))
    }

    /// Reference `_loops_extension`.
    async fn loops_extension(
        &self,
        method: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, AcpError> {
        let session_id = text(params, "sessionId")?;
        let call_params = match method {
            "loops/create" => json!({
                "interval": text(params, "interval")?,
                "prompt": text(params, "prompt")?,
            }),
            "loops/delete" => json!({"loopId": text(params, "loopId")?}),
            _ => json!({}),
        };
        let harness = self
            .find_live_session(&session_id)?
            .ok_or_else(|| AcpError::SessionNotFound(session_id.clone()))?;
        let method = match method {
            "loops/list" | "loops/create" | "loops/delete" => method,
            _ => "loops/clear",
        };
        let result = self
            .call_async(&harness, method, call_params)
            .await
            .map_err(|error| AcpError::InvalidParams(error_message(&error)))?
            .result;
        let field = |key: &str| result.get(key).cloned().unwrap_or(Value::Null);
        Ok(match method {
            "loops/list" => json!({"loops": field("loops")}),
            "loops/clear" => json!({"count": field("count")}),
            _ => json!({"loop": field("loop")}),
        })
    }

    /// Reference `_rewind_extension`.
    async fn rewind_extension(
        &self,
        method: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, AcpError> {
        let lookup = |camel: &str, snake: &str| {
            params
                .get(camel)
                .filter(|value| truthy(value))
                .or_else(|| params.get(snake))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        };
        let (Some(session_id), Some(entry_id)) = (
            lookup("sessionId", "session_id"),
            lookup("messageId", "message_id"),
        ) else {
            return Err(AcpError::InvalidParams(
                "rewinding needs both a sessionId and a messageId".to_owned(),
            ));
        };
        let harness = self.session_harness(&session_id)?;
        if method == "rewind/preview" {
            let result = self
                .call(
                    &harness,
                    "session/rewind/read",
                    json!({"entryId": entry_id}),
                )
                .await
                .map_err(|error| AcpError::InvalidParams(error_message(&error)))?;
            return Ok(json!({"paths": result.get("paths").cloned().unwrap_or(json!([]))}));
        }
        let restore_files = params.get("restoreFiles").is_none_or(truthy);
        let result = self
            .call(
                &harness,
                "session/rewind",
                json!({"entryId": entry_id, "restoreFiles": restore_files, "inplace": true}),
            )
            .await
            .map_err(|error| AcpError::InvalidParams(error_message(&error)))?;
        let field = |key: &str| result.get(key).cloned().unwrap_or(json!([]));
        Ok(json!({
            "messageContent": result.get("message").cloned().unwrap_or(Value::Null),
            "restoreErrors": field("restoreErrors"),
            "restoredPaths": field("restoredPaths"),
        }))
    }

    /// Reference `_review_extension`: a malformed request and a refused
    /// approval or revert answer as invalid, anything else the app server
    /// refuses escapes as an internal error.
    async fn review_extension(
        &self,
        method: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, AcpError> {
        let session_id = match params.get("sessionId") {
            Some(Value::String(session_id)) => session_id.clone(),
            _ => {
                return Err(AcpError::InvalidParams(format!(
                    "the {method} request needs a sessionId"
                )));
            }
        };
        let harness = self.session_harness(&session_id)?;
        let mut forwarded = params.clone();
        forwarded.insert("sessionId".to_owned(), json!(harness.canonical_id()));
        let mutation = matches!(method, "review/approve" | "review/revert");
        let result = self
            .call(&harness, method, Value::Object(forwarded))
            .await
            .map_err(|error| match protocol_code(&error) {
                Some(ProtocolErrorCode::InvalidParams) => {
                    AcpError::InvalidParams(error_message(&error))
                }
                _ if mutation => AcpError::InvalidParams(error_message(&error)),
                _ => AcpError::Unexpected(error_message(&error)),
            })?;
        if mutation {
            return Ok(json!({}));
        }
        Ok(Value::Object(result.into_iter().collect()))
    }

    /// Reference `_connectors_extension`: mutations answer with the list the
    /// change produced.
    async fn connectors_extension(
        &self,
        method: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, AcpError> {
        let session_id = text(params, "sessionId")?;
        match method {
            "connectors/list" => {}
            "connectors/authUrl" => {
                let name = text(params, "name")?;
                let harness = self.session_harness(&session_id)?;
                let result = self
                    .call_async(&harness, "connectors/auth/read", json!({"name": name}))
                    .await
                    .map_err(|error| AcpError::InvalidParams(error_message(&error)))?
                    .result;
                return Ok(json!({"url": result.get("url").cloned().unwrap_or(Value::Null)}));
            }
            "connectors/refresh" => {
                let names = match params.get("names") {
                    Some(Value::Array(names)) if !names.is_empty() => names
                        .iter()
                        .map(|name| {
                            name.as_str()
                                .map(str::trim)
                                .filter(|name| !name.is_empty())
                                .map(ToOwned::to_owned)
                                .ok_or_else(|| {
                                    AcpError::InvalidParams(
                                        "every connector name must be non-empty text".to_owned(),
                                    )
                                })
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    _ => {
                        return Err(AcpError::InvalidParams(
                            "the refresh needs a non-empty list of names".to_owned(),
                        ));
                    }
                };
                let harness = self.session_harness(&session_id)?;
                let mut failures = Vec::new();
                for name in &names {
                    if let Err(error) = self
                        .call_async(&harness, "connectors/refresh", json!({"name": name}))
                        .await
                    {
                        failures.push(error);
                    }
                }
                if failures.len() == names.len()
                    && let Some(error) = failures.into_iter().next()
                {
                    return Err(AcpError::InvalidParams(error_message(&error)));
                }
            }
            "connectors/toggle" => {
                let name = text(params, "name")?;
                let disabled = match params.get("disabled") {
                    Some(value) => crate::validation::lax_bool(value).ok_or_else(|| {
                        AcpError::InvalidParams("`disabled` must be a boolean".to_owned())
                    })?,
                    None => {
                        return Err(AcpError::InvalidParams("`disabled` is required".to_owned()));
                    }
                };
                let tool_name = match params.get("toolName") {
                    None | Some(Value::Null) => Value::Null,
                    Some(_) => json!(text(params, "toolName")?),
                };
                let harness = self.session_harness(&session_id)?;
                self.call_async(
                    &harness,
                    "connectors/toggle",
                    json!({"name": name, "disabled": disabled, "toolName": tool_name}),
                )
                .await
                .map_err(|error| AcpError::InvalidParams(error_message(&error)))?;
            }
            _ => return Err(AcpError::NotImplemented(method.to_owned())),
        }
        let harness = self.session_harness(&session_id)?;
        let state = self
            .call_async(&harness, "mcp/read", json!({}))
            .await
            .map_err(|error| AcpError::InvalidParams(error_message(&error)))?
            .result
            .remove("mcp")
            .unwrap_or(Value::Null);
        Ok(connectors_list(&state))
    }

    /// Reference `_project_links_extension`: stateless calls keyed on the
    /// absolute root the editor holds.
    async fn project_links_extension(
        &self,
        method: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, AcpError> {
        let forwarded = match method {
            "projectLinks/list" => json!({}),
            "projectLinks/resolveRoot" | "projectLinks/picker/load" | "projectLinks/unlink" => {
                json!({"rootPath": text(params, "rootPath")?})
            }
            "projectLinks/picker/loadMore" => json!({
                "rootPath": text(params, "rootPath")?,
                "cursor": text(params, "cursor")?,
            }),
            "projectLinks/create" => json!({
                "rootPath": text(params, "rootPath")?,
                "name": text(params, "name")?,
                "defaultBranch": text(params, "defaultBranch")?,
            }),
            "projectLinks/link" => json!({
                "rootPath": text(params, "rootPath")?,
                "projectId": text(params, "projectId")?,
                "projectName": text(params, "projectName")?,
            }),
            _ => return Err(AcpError::NotImplemented(method.to_owned())),
        };
        let result =
            self.host_call(method, forwarded).await.map_err(|error| {
                match protocol_code(&error) {
                    Some(ProtocolErrorCode::Unauthorized) => {
                        AcpError::Unauthenticated(error_message(&error))
                    }
                    Some(
                        ProtocolErrorCode::InvalidParams
                        | ProtocolErrorCode::InvalidRequest
                        | ProtocolErrorCode::NotFound
                        | ProtocolErrorCode::Conflict,
                    ) => AcpError::InvalidParams(error_message(&error)),
                    _ => AcpError::Internal(error_message(&error)),
                }
            })?;
        Ok(Value::Object(result))
    }

    /// Reference `_whoami_extension`.
    async fn whoami_extension(
        &self,
        method: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, AcpError> {
        let session_id = text(params, "sessionId")?;
        let harness = self.session_harness(&session_id)?;
        match method {
            "identity/read" => {
                let identity = self.call_async(&harness, "identity/read", json!({})).await;
                Ok(match identity {
                    Ok(dispatch) => dispatch
                        .result
                        .get("identity")
                        .filter(|identity| identity.is_object())
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                    Err(error)
                        if protocol_code(&error) == Some(ProtocolErrorCode::MethodNotFound) =>
                    {
                        json!({})
                    }
                    Err(error) => return Err(AcpError::InvalidParams(error_message(&error))),
                })
            }
            "account/read" => {
                let result = self
                    .call_async(&harness, "account/read", json!({}))
                    .await
                    .map_err(|error| AcpError::InvalidParams(error_message(&error)))?
                    .result;
                Ok(result
                    .get("account")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(result.into_iter().collect())))
            }
            _ => Err(AcpError::NotImplemented(method.to_owned())),
        }
    }

    /// Reference `_log_level_extension`.
    async fn log_level_extension(
        &self,
        method: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, AcpError> {
        match method {
            "logLevel/read" => Ok(log_level_payload()),
            "logLevel/write" => {
                let session_id = text(params, "sessionId")?;
                let optional_level = |key: &str| match params.get(key) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(level)) => Ok(Some(level.trim().to_owned())),
                    Some(_) => Err(AcpError::InvalidParams(format!("`{key}` must be text"))),
                };
                let session_override = optional_level("sessionOverride")?;
                let config_level = optional_level("configLevel")?;
                if params.contains_key("sessionOverride") {
                    set_session_log_level(parse_level(session_override.as_deref())?);
                }
                if params.contains_key("configLevel") {
                    let harness = self.session_harness(&session_id)?;
                    let Some(level) = config_level else {
                        return Err(AcpError::Unexpected(
                            "a configuration write needs a value to set".to_owned(),
                        ));
                    };
                    self.call(
                        &harness,
                        "config/patch",
                        json!({
                            "ops": [{"op": "set", "path": "/log_level", "value": level}],
                            "reloadRuntime": true,
                        }),
                    )
                    .await
                    .map_err(|error| AcpError::Unexpected(error_message(&error)))?;
                    set_config_log_level(parse_level(Some(&level))?);
                }
                Ok(log_level_payload())
            }
            _ => Err(AcpError::NotImplemented(method.to_owned())),
        }
    }

    /// Reference `_voice_extension`. Dictation and narration run on the
    /// terminal client's audio stack, which this adapter does not carry, so
    /// starting either reports why it cannot, as the reference does when its
    /// managers fail to start.
    fn voice_extension(
        &self,
        method: &str,
        _params: &Map<String, Value>,
    ) -> Result<Value, AcpError> {
        let has_session = self
            .lock_state()
            .map(|state| !state.sessions.is_empty())
            .unwrap_or(false);
        let unavailable = || {
            json!({
                "ok": false,
                "error": if has_session {
                    "Voice input and narration are not available in this build"
                } else {
                    "No session is open"
                },
            })
        };
        match method {
            "voice/transcribeStart" | "voice/narrate" => Ok(unavailable()),
            "voice/transcribeStop" => Ok(json!({"ok": true, "text": ""})),
            "voice/transcribeCancel" => Ok(json!({"ok": true})),
            "voice/narrateCancel" => {
                self.notify_after_response(NARRATION_DONE_METHOD, json!({}));
                Ok(json!({"ok": true}))
            }
            _ => Err(AcpError::NotImplemented(method.to_owned())),
        }
    }
}

/// Reference `ConnectorsListResponse.from_state`: only connector sources, each
/// split into whether the user wants it and whether it is reachable.
fn connectors_list(state: &Value) -> Value {
    let connectors = state
        .get("sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|source| source.get("kind").and_then(Value::as_str) == Some("connector"))
        .map(|source| {
            let status = source
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (enabled, reachability) = match status {
                "connected" | "enabled" => (true, "connected"),
                "needs_auth" => (true, "needs_auth"),
                "needs_setup" => (true, "needs_setup"),
                "disabled" => (false, "unknown"),
                _ => (true, "unavailable"),
            };
            let tools = source
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|tool| {
                    json!({
                        "name": tool.get("name").cloned().unwrap_or(json!("")),
                        "description": tool.get("description").cloned().unwrap_or(json!("")),
                        "enabled": tool.get("enabled").cloned().unwrap_or(json!(true)),
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "name": source.get("name").cloned().unwrap_or(Value::Null),
                "enabled": enabled,
                "reachability": reachability,
                "tools": tools,
                "error": source.get("error").cloned().unwrap_or(Value::Null),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "connectors": connectors,
        "error": state.get("connectorError").cloned().unwrap_or(Value::Null),
    })
}

fn log_level_payload() -> Value {
    let chain = log_level_chain();
    let name = |level: Option<LogLevel>| level.map_or(Value::Null, |level| json!(level.as_str()));
    json!({
        "session": name(chain.session),
        "env": name(chain.env),
        "config": name(chain.config),
        "effective": chain.effective.as_str(),
    })
}

/// A level name the chain can hold. The reference stores any text and fails
/// when it applies it; this port refuses it before storing anything.
fn parse_level(level: Option<&str>) -> Result<Option<LogLevel>, AcpError> {
    level
        .map(|level| {
            LogLevel::parse(level)
                .ok_or_else(|| AcpError::Unexpected(format!("`{level}` is not a log level")))
        })
        .transpose()
}

/// A required text parameter, trimmed and non-empty, the way the reference's
/// request models (`str_strip_whitespace`, `min_length=1`) read it.
fn text(params: &Map<String, Value>, key: &str) -> Result<String, AcpError> {
    match params.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.trim().to_owned()),
        Some(Value::String(_)) => Err(AcpError::InvalidParams(format!(
            "`{key}` must not be empty"
        ))),
        Some(_) => Err(AcpError::InvalidParams(format!("`{key}` must be text"))),
        None => Err(AcpError::InvalidParams(format!("`{key}` is required"))),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

fn protocol_code(error: &AcpError) -> Option<ProtocolErrorCode> {
    match error {
        AcpError::AppServer(ClientError::Protocol(code, _)) => Some(*code),
        _ => None,
    }
}

/// What the reference reads off `AppServerResponseError.error.message`.
fn error_message(error: &AcpError) -> String {
    match error {
        AcpError::AppServer(ClientError::Protocol(_, message)) => message.clone(),
        other => other.to_string(),
    }
}

fn ignore_missing(error: AcpError) -> Result<(), AcpError> {
    match protocol_code(&error) {
        Some(ProtocolErrorCode::NotFound) => Ok(()),
        _ => Err(error),
    }
}
