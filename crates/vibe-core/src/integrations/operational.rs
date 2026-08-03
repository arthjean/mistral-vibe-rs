use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::shared::{IntegrationError, bounded_text, push_bounded, redact};

const MAX_DIAGNOSTICS: usize = 1_024;
const MAX_LOG_RECORDS: usize = 4_096;
const MAX_FEEDBACK_RECORDS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    pub authenticated: bool,
    pub account_id: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogRecord {
    pub timestamp: u64,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationalStats {
    pub turns: u64,
    pub tool_calls: u64,
    pub failures: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeView {
    pub version: String,
    pub ready: bool,
    pub active_sessions: usize,
}

#[derive(Default)]
struct OperationalState {
    account: Option<AccountView>,
    diagnostics: Vec<Diagnostic>,
    logs: Vec<LogRecord>,
    feedback: Vec<String>,
    stats: OperationalStats,
    active_sessions: usize,
    ready: bool,
}

pub struct OperationalResources {
    version: String,
    state: Mutex<OperationalState>,
}

impl OperationalResources {
    #[must_use]
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            state: Mutex::new(OperationalState::default()),
        }
    }

    pub fn set_account(&self, account: AccountView) -> Result<(), IntegrationError> {
        self.state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?
            .account = Some(account);
        Ok(())
    }

    pub fn account(&self) -> Result<Option<AccountView>, IntegrationError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?
            .account
            .clone())
    }

    pub fn record_diagnostic(
        &self,
        code: impl Into<String>,
        message: &str,
        source: impl Into<String>,
    ) -> Result<(), IntegrationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?;
        push_bounded(
            &mut state.diagnostics,
            MAX_DIAGNOSTICS,
            Diagnostic {
                code: bounded_text(&code.into(), 128),
                message: redact(message),
                source: bounded_text(&source.into(), 128),
            },
        );
        Ok(())
    }

    pub fn diagnostics(&self) -> Result<Vec<Diagnostic>, IntegrationError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?
            .diagnostics
            .clone())
    }

    pub fn record_log(
        &self,
        timestamp: u64,
        level: impl Into<String>,
        message: &str,
    ) -> Result<(), IntegrationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?;
        push_bounded(
            &mut state.logs,
            MAX_LOG_RECORDS,
            LogRecord {
                timestamp,
                level: bounded_text(&level.into(), 32),
                message: redact(message),
            },
        );
        Ok(())
    }

    pub fn logs(&self, offset: usize, limit: usize) -> Result<Vec<LogRecord>, IntegrationError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?
            .logs
            .iter()
            .skip(offset)
            .take(limit.min(1_000))
            .cloned()
            .collect())
    }

    pub fn record_feedback(&self, message: &str) -> Result<usize, IntegrationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?;
        push_bounded(&mut state.feedback, MAX_FEEDBACK_RECORDS, redact(message));
        Ok(state.feedback.len())
    }

    pub fn should_show_feedback(&self) -> Result<bool, IntegrationError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?
            .feedback
            .is_empty())
    }

    pub fn summarize(&self, text: &str) -> Result<String, IntegrationError> {
        if text.trim().is_empty() {
            return Err(IntegrationError::UnsupportedNarration(
                "cannot summarize empty text".to_owned(),
            ));
        }
        Ok(text.chars().take(280).collect())
    }

    pub fn set_runtime(&self, ready: bool, active_sessions: usize) -> Result<(), IntegrationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?;
        state.ready = ready;
        state.active_sessions = active_sessions;
        Ok(())
    }

    pub fn runtime(&self) -> Result<RuntimeView, IntegrationError> {
        let state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?;
        Ok(RuntimeView {
            version: self.version.clone(),
            ready: state.ready,
            active_sessions: state.active_sessions,
        })
    }

    pub fn stats(&self) -> Result<OperationalStats, IntegrationError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?
            .stats
            .clone())
    }

    pub fn record_tool_outcome(&self, failed: bool) -> Result<(), IntegrationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| IntegrationError::LockPoisoned("operational resources"))?;
        state.stats.tool_calls = state.stats.tool_calls.saturating_add(1);
        if failed {
            state.stats.failures = state.stats.failures.saturating_add(1);
        }
        Ok(())
    }
}
