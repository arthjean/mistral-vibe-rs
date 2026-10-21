//! The log-level picker: its rows, its keys, and the write it makes when
//! it closes.

use serde_json::json;
use vibe_core::observability::{LogLevel, LogLevelChain, log_level_chain, set_session_log_level};

use super::super::command_handlers::Effect;
use super::super::command_handlers::log_level::{self, Badge, LogLevelBackend, Picker};
use super::super::interaction::{Overlay, OverlayItem, OverlayKind};
use super::super::state::TuiState;
use super::super::{InteractiveRuntime, push_local_document, push_local_error};
use super::live_commands::SETUP_REQUIRED;

pub(super) fn open_log_level_picker(state: &mut TuiState) {
    let picker = Picker::new(log_level_chain());
    state.overlay = Some(log_level_overlay(&picker));
    state.log_level_picker = Some(picker);
}

/// Reference `_build_row`, as a list: the effective level is marked, and each
/// row names the badges it carries, the highlighted row naming the one that
/// `Enter` toggles.
fn log_level_overlay(picker: &Picker) -> Overlay {
    let effective = picker.effective();
    let items = LogLevel::ALL
        .into_iter()
        .map(|level| {
            let marker = if level == effective { "› " } else { "  " };
            let mut badges = Vec::new();
            for (badge, name, set) in [
                (Badge::Session, "session", picker.session() == Some(level)),
                (Badge::Config, "config", picker.config() == Some(level)),
            ] {
                let focused = level == picker.highlighted() && badge == picker.focused();
                match (focused, set) {
                    (true, _) => badges.push(format!("[{name}]")),
                    (false, true) => badges.push(name.to_owned()),
                    (false, false) => {}
                }
            }
            OverlayItem::new(
                level.as_str(),
                format!("{marker}{:<10}", level.as_str()),
                badges.join("  "),
                false,
            )
        })
        .collect();
    let mut overlay = Overlay::new(OverlayKind::LogLevel, "Log Level", items);
    overlay.select_by_id(picker.highlighted().as_str());
    overlay.notice = Some(format!(
        "{}\n↑↓/jk Navigate  ←/→ Switch badge  Enter Toggle  Esc Close",
        picker.subtitle()
    ));
    overlay
}

/// What the picker's keys do, and applying it when it closes.
pub(in crate::tui) fn handle_log_level_key(
    key: crossterm::event::KeyEvent,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
) {
    use crossterm::event::KeyCode;

    let Some(mut picker) = state.log_level_picker.take() else {
        state.overlay = None;
        return;
    };
    let step = |picker: &Picker, delta: isize| {
        let index = LogLevel::ALL
            .iter()
            .position(|level| *level == picker.highlighted())
            .unwrap_or_default();
        let next = index
            .saturating_add_signed(delta)
            .min(LogLevel::ALL.len() - 1);
        LogLevel::ALL[next]
    };
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => picker.highlight(step(&picker, -1)),
        KeyCode::Down | KeyCode::Char('j') => picker.highlight(step(&picker, 1)),
        KeyCode::Left | KeyCode::Char('h') => picker.focus(Badge::Session),
        KeyCode::Right | KeyCode::Char('l') => picker.focus(Badge::Config),
        KeyCode::Enter => picker.toggle(),
        KeyCode::Esc => {
            let mut backend = LiveLogLevel { runtime, state };
            log_level::apply(picker.applied(), &mut backend);
            return;
        }
        _ => {}
    }
    state.overlay = Some(log_level_overlay(&picker));
    state.log_level_picker = Some(picker);
}

struct LiveLogLevel<'a> {
    runtime: &'a mut Option<InteractiveRuntime>,
    state: &'a mut TuiState,
}

impl LogLevelBackend for LiveLogLevel<'_> {
    fn chain(&self) -> LogLevelChain {
        log_level_chain()
    }

    fn set_session_override(&mut self, level: Option<LogLevel>) {
        set_session_log_level(level);
    }

    fn persist(&mut self, level: Option<LogLevel>) -> Result<(), String> {
        let runtime = self
            .runtime
            .as_mut()
            .ok_or_else(|| SETUP_REQUIRED.to_owned())?;
        let mutation = match level {
            Some(level) => json!({"path": ["log_level"], "value": level.as_str()}),
            None => json!({"path": ["log_level"], "remove": true}),
        };
        runtime
            .service
            .public_call(
                "config/batchWrite",
                json!({
                    "sessionId": runtime.session_id,
                    "writes": [{"target": "user", "mutations": [mutation]}],
                }),
            )
            .map_err(|error| error.to_string())?;
        // Reference `_on_config_changed`: the written level is the configured
        // tier from now on.
        vibe_core::observability::set_config_log_level(level);
        Ok(())
    }

    fn emit(&mut self, effect: Effect) {
        match effect {
            Effect::ClosePanel => {
                self.state.overlay = None;
                self.state.log_level_picker = None;
            }
            Effect::Message(text) => {
                push_local_document(self.state, text);
            }
            Effect::Error(text) => push_local_error(self.state, text),
            _ => {}
        }
    }
}
