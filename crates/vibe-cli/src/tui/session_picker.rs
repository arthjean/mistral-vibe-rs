use crossterm::event::{KeyCode, KeyEvent};

use super::interaction::Overlay;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionDeletePhase {
    Confirmation,
    Feedback(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDeleteState {
    pub session_id: String,
    pub phase: SessionDeletePhase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPickerEffect {
    None,
    Close,
    Resume(String),
    Delete(String),
}

impl SessionDeleteState {
    /// The delete key on `session_id`, which both session pickers bind: the
    /// first press asks for a confirmation, or says why the session cannot go,
    /// and a second press on the same session confirms. Answers whether the
    /// session is to be deleted now.
    pub fn press(delete: &mut Option<Self>, session_id: &str, current: bool) -> bool {
        let phase = if current {
            SessionDeletePhase::Feedback("Can't delete current session".to_owned())
        } else if delete.as_ref().is_some_and(|delete| {
            delete.session_id == session_id && delete.phase == SessionDeletePhase::Confirmation
        }) {
            return true;
        } else {
            SessionDeletePhase::Confirmation
        };
        *delete = Some(Self {
            session_id: session_id.to_owned(),
            phase,
        });
        false
    }

    /// Whether the delete pending on `session_id` holds back resuming it.
    #[must_use]
    pub fn holds(delete: Option<&Self>, session_id: &str) -> bool {
        delete.is_some_and(|delete| delete.session_id == session_id)
    }

    #[must_use]
    pub fn failure(session_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            phase: SessionDeletePhase::Feedback(format!("Delete failed: {}", error.into())),
        }
    }

    #[must_use]
    pub fn message(&self) -> &str {
        match &self.phase {
            SessionDeletePhase::Confirmation => "Press d again to delete",
            SessionDeletePhase::Feedback(message) => message,
        }
    }
}

pub fn reduce_key(
    overlay: &mut Overlay,
    delete: &mut Option<SessionDeleteState>,
    current_session_id: &str,
    key: KeyEvent,
) -> SessionPickerEffect {
    match key.code {
        KeyCode::Esc => {
            if delete.take().is_some() {
                SessionPickerEffect::None
            } else {
                SessionPickerEffect::Close
            }
        }
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            *delete = None;
            overlay.move_selection(-1);
            SessionPickerEffect::None
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            *delete = None;
            overlay.move_selection(1);
            SessionPickerEffect::None
        }
        KeyCode::Backspace if key.modifiers.is_empty() => {
            *delete = None;
            overlay.pop_query();
            SessionPickerEffect::None
        }
        KeyCode::Delete | KeyCode::Char('d') if key.modifiers.is_empty() => {
            let Some(session_id) = overlay.selected_item().map(|item| item.id.clone()) else {
                return SessionPickerEffect::None;
            };
            let current = session_id == current_session_id;
            if SessionDeleteState::press(delete, &session_id, current) {
                SessionPickerEffect::Delete(session_id)
            } else {
                SessionPickerEffect::None
            }
        }
        KeyCode::Enter | KeyCode::Char(' ') if key.modifiers.is_empty() => {
            let Some(session_id) = overlay.selected_item().map(|item| item.id.clone()) else {
                return SessionPickerEffect::None;
            };
            if SessionDeleteState::holds(delete.as_ref(), &session_id) {
                SessionPickerEffect::None
            } else {
                SessionPickerEffect::Resume(session_id)
            }
        }
        KeyCode::Char(character) if key.modifiers.is_empty() => {
            *delete = None;
            overlay.push_query(character);
            SessionPickerEffect::None
        }
        _ => SessionPickerEffect::None,
    }
}
