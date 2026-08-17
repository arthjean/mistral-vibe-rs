//! Routing one ACP request to the agent, and answering it in the right order.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use vibe_acp::{
    AcpAgent, AcpError, AcpForkSession, AcpInitializeRequest, AcpListSessions, AcpLoadSession,
    AcpLoadedSession, AcpNewSession, AcpSessionUpdate,
};
use vibe_app_server::client::TurnDriver;

use crate::stdio::client::{WriterMessage, send_value, send_value_wait};
use crate::stdio::wire::{
    WireRequest, acp_error_response, required_string, scalar_string, success_response, valid_id,
};

/// One dispatched request: what answers it, and what the client is told once
/// it has been answered.
///
/// Notifications that belong after the response cannot simply be queued during
/// dispatch: the writer is FIFO, so anything queued before the handler returns
/// reaches the editor first. Carrying them out of dispatch is what keeps the
/// ordering a property of the type rather than a convention.
struct Dispatched {
    result: Value,
    after: Vec<Value>,
}

impl Dispatched {
    const fn just(result: Value) -> Self {
        Self {
            result,
            after: Vec::new(),
        }
    }
}

pub(crate) async fn handle_request<D>(
    agent: Arc<AcpAgent<D>>,
    request: WireRequest,
    writer: mpsc::Sender<WriterMessage>,
) where
    D: TurnDriver + 'static,
{
    let id = request.id.clone();
    let (response, after) = match dispatch_request(agent, request, &writer).await {
        Ok(dispatched) => (success_response(id, dispatched.result), dispatched.after),
        Err(error) => (acp_error_response(id, error), Vec::new()),
    };
    let _ = send_value_wait(&writer, response).await;
    for notification in after {
        let _ = send_value_wait(&writer, notification).await;
    }
}

async fn dispatch_request<D>(
    agent: Arc<AcpAgent<D>>,
    request: WireRequest,
    writer: &mpsc::Sender<WriterMessage>,
) -> Result<Dispatched, AcpError>
where
    D: TurnDriver + 'static,
{
    if request.jsonrpc != "2.0" || !valid_id(&request.id) {
        return Err(AcpError::InvalidParams(
            "requests require jsonrpc 2.0 and a string or integer ID".to_owned(),
        ));
    }
    match request.method.as_str() {
        "initialize" => serde_json::from_value::<AcpInitializeRequest>(request.params)
            .map_err(AcpError::Json)
            .and_then(|params| agent.initialize_with(params))
            .and_then(|value| serde_json::to_value(value).map_err(AcpError::Json))
            .map(Dispatched::just),
        "authenticate" => {
            let method_id = required_string(&request.params, "methodId")?.to_owned();
            agent
                .authenticate(&method_id, &request.params)
                .await
                .map(Dispatched::just)
        }
        // Extension methods reach the wire under a leading underscore, which
        // the reference's router strips before dispatching them.
        "_auth/status" => agent.auth_status().map(Dispatched::just),
        "_auth/signOut" => agent.auth_sign_out().map(Dispatched::just),
        "session/new" => {
            let params =
                serde_json::from_value::<AcpNewSession>(request.params).map_err(AcpError::Json)?;
            let session = agent.new_session(params)?;
            let commands = commands_notification(&session.session_id, agent.advertised_commands());
            Ok(Dispatched {
                result: serde_json::to_value(&session)?,
                after: vec![commands],
            })
        }
        "session/load" | "session/resume" => {
            let params =
                serde_json::from_value::<AcpLoadSession>(request.params).map_err(AcpError::Json)?;
            let session = agent.load_session(params)?;
            // ACP replays a loaded transcript before the response, so these
            // updates are queued during dispatch rather than carried out of it.
            agent
                .replay_history(&session.session_id, |update| send_update(writer, &update))
                .await?;
            let commands = commands_notification(&session.session_id, agent.advertised_commands());
            Ok(Dispatched {
                result: serde_json::to_value(AcpLoadedSession {
                    settings: session.settings,
                })?,
                after: vec![commands],
            })
        }
        "session/list" => {
            let params = serde_json::from_value::<AcpListSessions>(request.params)
                .map_err(AcpError::Json)?;
            serde_json::to_value(
                agent.list_sessions(params.cwd.as_deref(), params.cursor.as_deref())?,
            )
            .map_err(AcpError::Json)
            .map(Dispatched::just)
        }
        "session/fork" => {
            let params =
                serde_json::from_value::<AcpForkSession>(request.params).map_err(AcpError::Json)?;
            let session = agent.fork_session(params)?;
            let commands = commands_notification(&session.session_id, agent.advertised_commands());
            Ok(Dispatched {
                result: serde_json::to_value(session)?,
                after: vec![commands],
            })
        }
        "session/close" => {
            let session_id = required_string(&request.params, "sessionId")?;
            agent.close_session(session_id).await?;
            Ok(Dispatched::just(json!({})))
        }
        "session/set_mode" => {
            agent
                .set_mode(
                    required_string(&request.params, "sessionId")?,
                    required_string(&request.params, "modeId")?,
                )
                .await?;
            Ok(Dispatched::just(json!({})))
        }
        "session/set_config_option" => {
            let session_id = required_string(&request.params, "sessionId")?;
            let value = scalar_string(
                request
                    .params
                    .get("value")
                    .ok_or_else(|| AcpError::InvalidParams("value is required".to_owned()))?,
            )?;
            let config_options = agent
                .set_config_option(
                    session_id,
                    required_string(&request.params, "configId")?,
                    &value,
                )
                .await?;
            Ok(Dispatched::just(json!({"configOptions": config_options})))
        }
        "session/prompt" => {
            let session_id = required_string(&request.params, "sessionId")?.to_owned();
            let prompt = request
                .params
                .get("prompt")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| {
                    AcpError::InvalidParams("session/prompt requires prompt blocks".to_owned())
                })?;
            let response = agent
                .prompt_content(&session_id, prompt, |update| send_update(writer, update))
                .await?;
            serde_json::to_value(response)
                .map_err(AcpError::Json)
                .map(Dispatched::just)
        }
        method => Err(AcpError::UnsupportedClientFlow(format!(
            "unknown ACP method `{method}`"
        ))),
    }
}

/// The command catalog a session is told about once it exists. Reference
/// `_send_initial_commands` publishes this from a task the session spawns,
/// which is why it always lands after the response that announced the session.
fn commands_notification(session_id: &str, commands: Vec<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "available_commands_update",
                "availableCommands": commands,
            },
        },
    })
}

fn send_update(
    writer: &mpsc::Sender<WriterMessage>,
    update: &AcpSessionUpdate,
) -> Result<(), AcpError> {
    send_value(
        writer,
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": update,
        }),
    )
    .map_err(AcpError::ClientTool)
}
