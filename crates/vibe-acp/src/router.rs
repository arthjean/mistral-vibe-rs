//! Which handler a JSON-RPC method reaches, and how its parameters are read.
//!
//! The reference serves ACP through the `acp` package's router: a method with a
//! leading underscore is an extension and reaches `ext_method` with the
//! underscore stripped, any other request must name a route, and a route
//! validates its parameters before the handler sees them. Members of a
//! request's `_meta` object are handed to the handler as extra arguments,
//! which is how a fork's `messageId` and a prompt's display content travel.

use std::cell::RefCell;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use tokio::sync::oneshot;
use vibe_app_server::client::TurnDriver;

use crate::agent::AcpAgent;
use crate::client_tools::AcpClientPort;
use crate::protocol::{
    AcpError, AcpForkSession, AcpInitializeRequest, AcpListSessions, AcpLoadSession, AcpNewSession,
    AcpSessionUpdate,
};
use crate::validation::validate_request;

/// The standard requests the reference routes.
const REQUEST_METHODS: [&str; 11] = [
    "initialize",
    "authenticate",
    "session/new",
    "session/load",
    "session/list",
    "session/close",
    "session/set_mode",
    "session/prompt",
    "session/set_config_option",
    "session/fork",
    "session/resume",
];

tokio::task_local! {
    static FOLLOWUPS: RefCell<Followups>;
}

/// What a request leaves to happen once its answer is written: notifications
/// a reference handler scheduled with `asyncio.ensure_future`, and tasks that
/// wait for the answer before they speak.
#[derive(Default)]
pub struct Followups {
    notifications: Vec<(String, Value)>,
    waiters: Vec<oneshot::Sender<()>>,
}

impl Followups {
    /// Sends the notifications, then releases the waiting tasks, so both land
    /// behind the answer already queued on `port`.
    pub fn deliver(self, port: &dyn AcpClientPort) {
        for (method, params) in self.notifications {
            port.notify(&method, params);
        }
        for waiter in self.waiters {
            let _ = waiter.send(());
        }
    }
}

/// Resolves once the request being answered has been answered, or `None`
/// outside of one.
pub(crate) fn response_sent() -> Option<oneshot::Receiver<()>> {
    FOLLOWUPS
        .try_with(|followups| {
            let (sender, receiver) = oneshot::channel();
            followups.borrow_mut().waiters.push(sender);
            receiver
        })
        .ok()
}

impl<D> AcpAgent<D>
where
    D: TurnDriver + 'static,
{
    /// Sends `method` once the request being answered has been answered, or
    /// right away outside of one.
    pub(crate) fn notify_after_response(&self, method: &str, params: Value) {
        let queued = FOLLOWUPS
            .try_with(|followups| {
                followups
                    .borrow_mut()
                    .notifications
                    .push((method.to_owned(), params.clone()));
            })
            .is_ok();
        if !queued && let Some(client) = &self.client {
            client.notify(method, params);
        }
    }
}

impl<D> AcpAgent<D>
where
    D: TurnDriver + 'static,
{
    /// Answers one request, with the notifications that must reach the
    /// client only after the answer does: a reference handler schedules
    /// those with `asyncio.ensure_future`, which runs once it has returned.
    pub async fn handle_request_with_followups(
        self: &Arc<Self>,
        method: &Value,
        params: Value,
    ) -> (Result<Value, AcpError>, Followups) {
        FOLLOWUPS
            .scope(RefCell::new(Followups::default()), async {
                let result = self.handle_request(method, params).await;
                (result, FOLLOWUPS.with(RefCell::take))
            })
            .await
    }

    /// Answers one request: the result on success, or the error the client
    /// is told.
    pub async fn handle_request(
        self: &Arc<Self>,
        method: &Value,
        params: Value,
    ) -> Result<Value, AcpError> {
        let Some(name) = method.as_str() else {
            return Err(AcpError::MethodNotFound(method.to_string()));
        };
        if let Some(extension) = name.strip_prefix('_') {
            let payload = match params {
                Value::Object(payload) => payload,
                _ => serde_json::Map::new(),
            };
            return self.ext_method(extension, payload).await;
        }
        if !REQUEST_METHODS.contains(&name) {
            return Err(AcpError::MethodNotFound(name.to_owned()));
        }
        validate_request(name, &params)?;
        let meta = params
            .get("_meta")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        match name {
            "initialize" => {
                let request = AcpInitializeRequest::from_params(&params);
                Ok(serde_json::to_value(self.initialize_with(request)?)?)
            }
            "authenticate" => {
                let method_id = string(&params, "methodId");
                self.authenticate(&method_id, &Value::Object(meta)).await
            }
            "session/new" => {
                let request = serde_json::from_value::<AcpNewSession>(params)?;
                self.new_session(request).await
            }
            "session/load" => {
                let request = serde_json::from_value::<AcpLoadSession>(params)?;
                self.load_session(request).await
            }
            "session/resume" => {
                let request = serde_json::from_value::<AcpLoadSession>(params)?;
                self.resume_session(request).await
            }
            "session/fork" => {
                let mut request = serde_json::from_value::<AcpForkSession>(params)?;
                request.message_id = meta
                    .get("messageId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                if meta
                    .get("messageId")
                    .is_some_and(|value| !value.is_string() && !value.is_null())
                {
                    return Err(AcpError::InvalidParams(
                        "the fork messageId must be a string".to_owned(),
                    ));
                }
                self.fork_session(request).await
            }
            "session/list" => {
                let request = serde_json::from_value::<AcpListSessions>(params)?;
                self.list_sessions(request.cwd.as_deref()).await
            }
            "session/close" => {
                self.close_session(&string(&params, "sessionId")).await?;
                Ok(json!({}))
            }
            "session/set_mode" => {
                self.set_mode(&string(&params, "sessionId"), &string(&params, "modeId"))
                    .await?;
                Ok(json!({}))
            }
            "session/set_config_option" => {
                let value = params.get("value").cloned().unwrap_or(Value::Null);
                self.set_config_option(
                    &string(&params, "sessionId"),
                    &string(&params, "configId"),
                    &value,
                )
                .await
            }
            "session/prompt" => {
                let session_id = string(&params, "sessionId");
                let prompt = params
                    .get("prompt")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                self.prompt_content(&session_id, prompt, &meta).await
            }
            _ => Err(AcpError::MethodNotFound(name.to_owned())),
        }
    }

    /// Serves one notification. A notification is answered with nothing, so
    /// whatever its handler raises is dropped, as the reference's connection
    /// drops it.
    pub async fn handle_notification(self: &Arc<Self>, method: &Value, params: Value) {
        let Some(name) = method.as_str() else {
            return;
        };
        match name {
            "session/cancel" => {
                if validate_request(name, &params).is_ok() {
                    let _ = self.cancel(&string(&params, "sessionId")).await;
                }
            }
            "_telemetry/send" => {
                let payload = if params.is_object() {
                    params
                } else {
                    json!({})
                };
                let _ = self.telemetry_notification(&payload).await;
            }
            _ => {}
        }
    }

    /// Sends one `session/update` notification to the client.
    pub(crate) fn send_update(&self, update: &AcpSessionUpdate) {
        if let Some(client) = self.client.as_ref()
            && let Ok(params) = serde_json::to_value(update)
        {
            client.notify("session/update", params);
        }
    }

    /// Sends `update` about `session_id`.
    pub(crate) fn session_update(&self, session_id: &str, update: Value) {
        self.send_update(&AcpSessionUpdate {
            session_id: session_id.to_owned(),
            update,
        });
    }
}

/// A string member the validator already checked, or empty when it is absent.
fn string(params: &Value, key: &str) -> String {
    params
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The keyword arguments a handler receives: the validated fields, with the
/// members of `_meta` laid over them.
#[allow(dead_code)]
pub(crate) fn call_arguments(params: &Value) -> Map<String, Value> {
    let mut arguments = params.as_object().cloned().unwrap_or_default();
    if let Some(Value::Object(meta)) = arguments.remove("_meta") {
        arguments.extend(meta);
    }
    arguments
}
