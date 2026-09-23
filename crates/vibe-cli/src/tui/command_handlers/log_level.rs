//! `/log-level`: the picker's draft state and what applying it does.
//!
//! Reference `LogLevelPickerApp` (`vibe/cli/textual_ui/widgets/log_level_picker.py`)
//! and `on_log_level_picker_app_applied` (`vibe/cli/textual_ui/app.py`). The
//! level a record needs is the first of: this session's override, the
//! environment, `log_level` in `config.toml`, and `WARNING`.

use vibe_core::observability::{LogLevel, LogLevelChain};

use super::Effect;

/// The two things a picker row can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum Badge {
    Session,
    Config,
}

/// What the picker hands back when it closes. Reference
/// `LogLevelPickerApp.Applied`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) struct Applied {
    pub session: Option<LogLevel>,
    pub config: Option<LogLevel>,
    /// The operator removed a level `config.toml` carried.
    pub config_cleared: bool,
}

/// The picker's draft. Reference `LogLevelPickerApp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui) struct Picker {
    chain: LogLevelChain,
    session: Option<LogLevel>,
    config: Option<LogLevel>,
    highlighted: LogLevel,
    focused: Badge,
}

impl Picker {
    #[must_use]
    pub(in crate::tui) fn new(chain: LogLevelChain) -> Self {
        Self {
            session: chain.session,
            config: chain.config,
            highlighted: chain.session.unwrap_or(chain.effective),
            focused: Badge::Session,
            chain,
        }
    }

    pub(in crate::tui) const fn highlighted(&self) -> LogLevel {
        self.highlighted
    }

    pub(in crate::tui) const fn focused(&self) -> Badge {
        self.focused
    }

    pub(in crate::tui) const fn session(&self) -> Option<LogLevel> {
        self.session
    }

    pub(in crate::tui) const fn config(&self) -> Option<LogLevel> {
        self.config
    }

    pub(in crate::tui) fn highlight(&mut self, level: LogLevel) {
        self.highlighted = level;
    }

    pub(in crate::tui) fn focus(&mut self, badge: Badge) {
        self.focused = badge;
    }

    /// Reference `_toggle_badge`: the focused badge moves onto the highlighted
    /// level, or comes off it when it was already there.
    pub(in crate::tui) fn toggle(&mut self) {
        let level = Some(self.highlighted);
        let slot = match self.focused {
            Badge::Session => &mut self.session,
            Badge::Config => &mut self.config,
        };
        *slot = if *slot == level { None } else { level };
    }

    /// Reference `_effective_level`, recomputed against the draft.
    #[must_use]
    pub(in crate::tui) fn effective(&self) -> LogLevel {
        self.session
            .or(self.chain.env)
            .or(self.config)
            .unwrap_or(LogLevel::DEFAULT)
    }

    /// Reference `_subtitle_text`.
    #[must_use]
    pub(in crate::tui) fn subtitle(&self) -> String {
        let source = if let Some(session) = self.session {
            format!("session override: {}", session.as_str())
        } else if let Some(env) = self.chain.env {
            format!("env LOG_LEVEL: {}", env.as_str())
        } else if let Some(config) = self.config {
            format!("config.toml: {}", config.as_str())
        } else {
            "default".to_owned()
        };
        format!("Effective: {}  ({source})", self.effective().as_str())
    }

    /// Reference `action_apply`.
    #[must_use]
    pub(in crate::tui) fn applied(&self) -> Applied {
        Applied {
            session: self.session,
            config: self.config,
            config_cleared: self.chain.config.is_some() && self.config.is_none(),
        }
    }
}

/// Where applying the picker reads the chain and writes the two levels.
pub(in crate::tui) trait LogLevelBackend {
    fn chain(&self) -> LogLevelChain;
    fn set_session_override(&mut self, level: Option<LogLevel>);
    /// Writes `log_level` to `config.toml`, or removes it when `level` is none.
    fn persist(&mut self, level: Option<LogLevel>) -> Result<(), String>;
    fn emit(&mut self, effect: Effect);
}

/// Reference `on_log_level_picker_app_applied`.
///
/// The session override applies at once. The configured level is written to
/// the file and takes effect when the configuration is next read, so the
/// effective level reported here is computed before it does.
pub(in crate::tui) fn apply(applied: Applied, backend: &mut impl LogLevelBackend) {
    let mut parts = Vec::new();
    let previous = backend.chain();
    backend.set_session_override(applied.session);
    match applied.session {
        Some(level) => parts.push(format!("session override → {}", level.as_str())),
        None if previous.session.is_some() => parts.push("session override cleared".to_owned()),
        None => {}
    }
    let written = if let Some(level) = applied.config {
        Some(
            backend
                .persist(Some(level))
                .map(|()| format!("config.toml → {}", level.as_str())),
        )
    } else if applied.config_cleared {
        Some(
            backend
                .persist(None)
                .map(|()| "config.toml cleared".to_owned()),
        )
    } else {
        None
    };
    match written {
        Some(Err(error)) => {
            backend.emit(Effect::ClosePanel);
            backend.emit(Effect::Error(format!(
                "Failed to persist log-level config: {error}"
            )));
            return;
        }
        Some(Ok(feedback)) => parts.push(feedback),
        None => {}
    }
    let effective = backend.chain().effective;
    let feedback = if parts.is_empty() {
        format!("Log level unchanged  (effective: {})", effective.as_str())
    } else {
        format!("{}  (effective: {})", parts.join("  "), effective.as_str())
    };
    backend.emit(Effect::ClosePanel);
    backend.emit(Effect::Message(feedback));
}
