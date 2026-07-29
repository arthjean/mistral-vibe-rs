#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use vibe_app_server::client::{
    ClientError, HeadlessService, ProgrammaticTurn, ProgrammaticUpdate, SessionOptions, TurnDriver,
    programmatic_update_channel,
};

pub const ADAPTER_NAME: &str = "vibe-acp";
pub const ACP_PROTOCOL_VERSION: u16 = 1;

pub struct AcpAgent<D> {
    service: HeadlessService<D>,
    initialized: bool,
}

impl<D> AcpAgent<D>
where
    D: TurnDriver,
{
    pub fn new(driver: D) -> Result<Self, AcpError> {
        Ok(Self {
            service: HeadlessService::new(driver)?,
            initialized: false,
        })
    }

    pub fn initialize(&mut self) -> Result<AcpInitializeResponse, AcpError> {
        if self.initialized {
            return Err(AcpError::AlreadyInitialized);
        }
        self.initialized = true;
        Ok(AcpInitializeResponse {
            protocol_version: ACP_PROTOCOL_VERSION,
            agent_capabilities: AcpAgentCapabilities {
                load_session: false,
                prompt_capabilities: AcpPromptCapabilities {
                    audio: false,
                    embedded_context: false,
                    image: false,
                },
                session_capabilities: serde_json::json!({"close": {}}),
            },
            auth_methods: Vec::new(),
            agent_info: AcpImplementation {
                name: "@mistralai/mistral-vibe".to_owned(),
                title: "Mistral Vibe".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
        })
    }

    pub fn new_session(&mut self, request: AcpNewSession) -> Result<AcpSession, AcpError> {
        self.require_initialized()?;
        let session_id = self.service.start_session(&SessionOptions {
            working_directory: request.cwd,
            session_id: None,
            add_directories: request.additional_directories.unwrap_or_default(),
            trusted: false,
            agent: None,
            tool_filters: Vec::new(),
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            mcp_servers: request.mcp_servers,
            max_turns: None,
            max_tokens: None,
            max_price_micros: None,
            auto_approve: false,
            resume: None,
            continue_session: false,
        })?;
        Ok(AcpSession { session_id })
    }

    pub async fn prompt(
        &mut self,
        session_id: &str,
        prompt: &str,
    ) -> Result<Vec<AcpSessionUpdate>, AcpError> {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let _response = self.prompt_streaming(session_id, prompt, sender).await?;
        let mut updates = Vec::new();
        while let Ok(update) = receiver.try_recv() {
            updates.push(update);
        }
        Ok(updates)
    }

    pub async fn prompt_streaming(
        &mut self,
        session_id: &str,
        prompt: &str,
        sender: tokio::sync::mpsc::UnboundedSender<AcpSessionUpdate>,
    ) -> Result<AcpPromptResponse, AcpError> {
        self.require_initialized()?;
        let (observer, mut updates) = programmatic_update_channel(session_id);
        let prompt_future = self.service.prompt_observed(session_id, prompt, observer);
        tokio::pin!(prompt_future);
        let turn = loop {
            tokio::select! {
                result = &mut prompt_future => break result?,
                update = updates.recv() => {
                    if let Some(update) = update {
                        send_acp_updates(session_id, update, &sender)?;
                    }
                }
            }
        };
        while let Ok(update) = updates.try_recv() {
            send_acp_updates(session_id, update, &sender)?;
        }
        let mut usage_update = serde_json::json!({
            "sessionUpdate": "usage_update",
            "used": turn.context_tokens,
            "size": 200_000,
        });
        if turn.usage.price_micros > 0 {
            usage_update["cost"] = serde_json::json!({
                "amount": turn.usage.price_micros as f64 / 1_000_000.0,
                "currency": "USD",
            });
        }
        sender
            .send(AcpSessionUpdate {
                session_id: session_id.to_owned(),
                update: usage_update,
            })
            .map_err(|_| AcpError::Disconnected)?;
        Ok(AcpPromptResponse {
            stop_reason: acp_stop_reason(&turn).to_owned(),
            usage: AcpUsage {
                total_tokens: turn
                    .usage
                    .input_tokens
                    .saturating_add(turn.usage.output_tokens),
                input_tokens: turn.usage.input_tokens,
                output_tokens: turn.usage.output_tokens,
            },
        })
    }

    pub fn close_session(&mut self, session_id: &str) -> Result<(), AcpError> {
        self.require_initialized()?;
        self.service.close_session(session_id)?;
        Ok(())
    }

    pub fn disconnect(&mut self) -> Result<(), AcpError> {
        if self.initialized {
            self.service.shutdown()?;
            self.initialized = false;
        }
        Ok(())
    }

    fn require_initialized(&self) -> Result<(), AcpError> {
        if self.initialized {
            Ok(())
        } else {
            Err(AcpError::NotInitialized)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpInitializeResponse {
    pub protocol_version: u16,
    pub agent_capabilities: AcpAgentCapabilities,
    pub auth_methods: Vec<Value>,
    pub agent_info: AcpImplementation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpAgentCapabilities {
    pub load_session: bool,
    pub prompt_capabilities: AcpPromptCapabilities,
    pub session_capabilities: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpPromptCapabilities {
    pub audio: bool,
    pub embedded_context: bool,
    pub image: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpImplementation {
    pub name: String,
    pub title: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpNewSession {
    pub cwd: String,
    #[serde(default)]
    pub additional_directories: Option<Vec<String>>,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSession {
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSessionUpdate {
    pub session_id: String,
    pub update: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpUsage {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpPromptResponse {
    pub stop_reason: String,
    pub usage: AcpUsage,
}

fn send_acp_updates(
    session_id: &str,
    update: ProgrammaticUpdate,
    sender: &tokio::sync::mpsc::UnboundedSender<AcpSessionUpdate>,
) -> Result<(), AcpError> {
    let ProgrammaticUpdate::HistoryEntry { entry, .. } = update;
    let entry = serde_json::to_value(entry)?;
    let message_id = entry.get("id").cloned().unwrap_or(Value::Null);
    let update_kind = match (
        entry.get("type").and_then(Value::as_str),
        entry.get("role").and_then(Value::as_str),
    ) {
        (Some("message"), Some("user")) => "user_message_chunk",
        (Some("message"), Some("assistant")) => "agent_message_chunk",
        (Some("reasoning"), _) => "agent_thought_chunk",
        _ => return Ok(()),
    };
    let content = if update_kind == "agent_thought_chunk" {
        serde_json::json!({"type": "text", "text": entry["text"]})
    } else {
        entry
            .get("content")
            .and_then(Value::as_array)
            .and_then(|content| content.first())
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type": "text", "text": ""}))
    };
    sender
        .send(AcpSessionUpdate {
            session_id: session_id.to_owned(),
            update: serde_json::json!({
                "sessionUpdate": update_kind,
                "content": content,
                "messageId": message_id,
            }),
        })
        .map_err(|_| AcpError::Disconnected)
}

fn acp_stop_reason(turn: &ProgrammaticTurn) -> &'static str {
    match turn.stop_reason {
        vibe_app_server::client::PublicTurnStopReason::Complete => "end_turn",
        vibe_app_server::client::PublicTurnStopReason::MaxSteps => "max_turn_requests",
        vibe_app_server::client::PublicTurnStopReason::TokenLimit
        | vibe_app_server::client::PublicTurnStopReason::PriceLimit
        | vibe_app_server::client::PublicTurnStopReason::ResponseLength => "max_tokens",
        vibe_app_server::client::PublicTurnStopReason::Refusal => "refusal",
        vibe_app_server::client::PublicTurnStopReason::Cancelled => "cancelled",
        vibe_app_server::client::PublicTurnStopReason::Failed => "cancelled",
    }
}

#[derive(Debug, Error)]
pub enum AcpError {
    #[error("ACP agent is not initialized")]
    NotInitialized,
    #[error("ACP agent is already initialized")]
    AlreadyInitialized,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("ACP client disconnected")]
    Disconnected,
}

#[cfg(test)]
mod tests {
    use vibe_app_server::client::EchoTurnDriver;

    use super::*;

    #[tokio::test]
    async fn minimal_acp_exchange_stays_on_public_app_server_contracts() {
        let mut agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
        let initialized = agent.initialize().expect("ACP initializes");
        assert_eq!(initialized.protocol_version, 1);
        assert!(!initialized.agent_capabilities.load_session);
        let session = agent
            .new_session(AcpNewSession {
                cwd: "/workspace".to_owned(),
                additional_directories: Some(Vec::new()),
                mcp_servers: Vec::new(),
            })
            .expect("ACP session starts");
        let updates = agent
            .prompt(&session.session_id, "question")
            .await
            .expect("ACP prompt completes");
        assert!(updates.len() >= 3);
        assert_eq!(
            updates
                .last()
                .and_then(|update| update.update.get("sessionUpdate"))
                .and_then(Value::as_str),
            Some("usage_update")
        );
        agent
            .close_session(&session.session_id)
            .expect("ACP session closes");
        agent.disconnect().expect("ACP disconnects");
    }

    #[tokio::test]
    async fn disconnected_update_receiver_returns_a_typed_error_and_still_shuts_down() {
        let mut agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
        agent.initialize().expect("ACP initializes");
        let session = agent
            .new_session(AcpNewSession {
                cwd: "/workspace".to_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
            })
            .expect("ACP session starts");
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        drop(receiver);
        assert!(matches!(
            agent
                .prompt_streaming(&session.session_id, "question", sender)
                .await,
            Err(AcpError::Disconnected)
        ));
        agent.disconnect().expect("ACP shuts down after disconnect");
    }

    #[tokio::test]
    async fn missing_session_is_rejected_without_creating_one() {
        let mut agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
        agent.initialize().expect("ACP initializes");
        assert!(matches!(
            agent.prompt("missing-session", "question").await,
            Err(AcpError::Client(_))
        ));
        agent.disconnect().expect("ACP disconnects");
    }

    #[test]
    fn requests_before_initialize_are_rejected_without_runtime_mutation() {
        let mut agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
        assert!(matches!(
            agent.new_session(AcpNewSession {
                cwd: "/workspace".to_owned(),
                additional_directories: None,
                mcp_servers: Vec::new(),
            }),
            Err(AcpError::NotInitialized)
        ));
    }
}
