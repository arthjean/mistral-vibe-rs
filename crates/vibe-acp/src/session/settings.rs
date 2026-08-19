//! The two settings an ACP session negotiates, and the canonical session
//! options they project onto.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use vibe_app_server::client::SessionOptions;

use crate::protocol::{AcpSession, AcpSessionSettings};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    #[default]
    Code,
    Plan,
}

impl Mode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "code" => Some(Self::Code),
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Plan => "plan",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Thinking {
    Off,
    Low,
    #[default]
    Medium,
    High,
    Max,
}

impl Thinking {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            _ => Self::effort(value),
        }
    }

    /// Reasoning-effort levels, which exclude `off` because the canonical
    /// session carries effort and enablement as two separate fields.
    fn effort(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    pub(crate) const fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub(crate) const fn effort_str(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            other => Some(other.as_str()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SessionSettings {
    pub(crate) mode: Mode,
    pub(crate) thinking: Thinking,
}

impl SessionSettings {
    /// Rebuilds settings from a persisted workspace payload, where `thinking`
    /// may be a boolean paired with a separate effort or a level string.
    pub(crate) fn from_workspace_result(result: &BTreeMap<String, Value>) -> Self {
        let config = result
            .get("metadata")
            .and_then(|metadata| metadata.get("config"))
            .and_then(Value::as_object);
        let mode = config
            .and_then(|config| config.get("mode"))
            .and_then(Value::as_str)
            .and_then(Mode::parse)
            .unwrap_or_default();
        let effort = config
            .and_then(|config| {
                config
                    .get("reasoningEffort")
                    .or_else(|| config.get("reasoning_effort"))
            })
            .and_then(Value::as_str)
            .and_then(Thinking::effort);
        let thinking = match config.and_then(|config| config.get("thinking")) {
            Some(Value::Bool(false)) => Thinking::Off,
            Some(Value::String(level)) => Thinking::parse(level).or(effort).unwrap_or_default(),
            _ => effort.unwrap_or_default(),
        };
        Self { mode, thinking }
    }

    pub(crate) fn from_intent(mode: Option<&str>, thinking: bool, effort: Option<&str>) -> Self {
        Self {
            mode: mode.and_then(Mode::parse).unwrap_or_default(),
            thinking: if thinking {
                effort.and_then(Thinking::effort).unwrap_or_default()
            } else {
                Thinking::Off
            },
        }
    }

    pub(crate) fn as_config(&self) -> Value {
        let mut config = json!({
            "mode": self.mode.as_str(),
            "thinking": self.thinking.enabled(),
        });
        if let Some(effort) = self.thinking.effort_str() {
            config["reasoningEffort"] = json!(effort);
        }
        config
    }

    /// What both session responses publish about these settings.
    pub(crate) fn as_wire(&self) -> AcpSessionSettings {
        AcpSessionSettings {
            modes: Some(json!({
                "currentModeId": self.mode.as_str(),
                "availableModes": [
                    {"id": "code", "name": "Code", "description": "Execute changes"},
                    {"id": "plan", "name": "Plan", "description": "Inspect and plan"},
                ],
            })),
            config_options: Some(thinking_config_options(self.thinking)),
        }
    }

    pub(crate) fn as_session(&self, session_id: String) -> AcpSession {
        AcpSession {
            session_id,
            settings: self.as_wire(),
        }
    }
}

pub(crate) fn thinking_config_options(current: Thinking) -> Vec<Value> {
    vec![json!({
        "id": "thinking",
        "name": "Thinking",
        "type": "select",
        "currentValue": current.as_str(),
        "options": [
            {"value": "off", "name": "Off"},
            {"value": "low", "name": "Low"},
            {"value": "medium", "name": "Medium"},
            {"value": "high", "name": "High"},
            {"value": "max", "name": "Max"},
        ],
    })]
}

pub(crate) fn session_options(
    cwd: &str,
    session_id: Option<String>,
    additional_directories: Vec<String>,
    mcp_servers: Vec<Value>,
    resume: Option<String>,
    settings: &SessionSettings,
) -> SessionOptions {
    SessionOptions {
        working_directory: cwd.to_owned(),
        session_id,
        add_directories: additional_directories,
        trusted: !mcp_servers.is_empty(),
        agent: None,
        tool_filters: Vec::new(),
        enabled_tools: Vec::new(),
        disabled_tools: vec!["ask_user_question".to_owned(), "exit_plan_mode".to_owned()],
        mcp_servers,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_price_micros: None,
        mode: Some(settings.mode.as_str().to_owned()),
        thinking: settings.thinking.enabled(),
        reasoning_effort: settings.thinking.effort_str().map(ToOwned::to_owned),
        auto_approve: false,
        resume,
        continue_session: false,
    }
}
