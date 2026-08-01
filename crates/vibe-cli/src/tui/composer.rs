use std::collections::VecDeque;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::attachments::normalize_pasted_text;
use super::chat_input::{ChatInputState, InputEffect, InputEvent, KeyName, Modifier};
use super::completion::CompletionResolution;
use super::state::TuiState;

pub(super) fn apply_event(
    input: &mut ChatInputState,
    event: InputEvent,
    working_directory: &Path,
    state: &mut TuiState,
) -> Vec<InputEffect> {
    let effects = input.apply(event);
    apply_effects(input, effects, working_directory, state)
}

pub(super) fn apply_effects(
    input: &mut ChatInputState,
    effects: Vec<InputEffect>,
    working_directory: &Path,
    state: &mut TuiState,
) -> Vec<InputEffect> {
    let mut pending = VecDeque::from(effects);
    let mut application_effects = Vec::new();
    while let Some(effect) = pending.pop_front() {
        match effect {
            InputEffect::RequestCompletion { request } => {
                match input.dispatch_completion_request(request.clone(), working_directory) {
                    Ok(Some(resolution)) => {
                        pending.extend(input.apply(InputEvent::CompletionResolved { resolution }));
                    }
                    Ok(None) => {}
                    Err(error) => pending.extend(input.apply(InputEvent::CompletionResolved {
                        resolution: CompletionResolution::failed(request, &error),
                    })),
                }
            }
            InputEffect::NormalizePastedPath { text } => {
                let text = normalize_pasted_text(&text);
                pending.extend(input.apply(InputEvent::PasteNormalized { text }));
            }
            InputEffect::Notify { message, .. } | InputEffect::Rejected { reason: message } => {
                state.push_diagnostic(message);
            }
            effect => application_effects.push(effect),
        }
    }
    application_effects
}

pub(super) fn normalized_key_event(key: KeyEvent) -> Option<InputEvent> {
    let (key_name, character) = match key.code {
        KeyCode::Esc => (KeyName::Escape, None),
        KeyCode::Enter => (KeyName::Enter, None),
        KeyCode::Backspace => (KeyName::Backspace, None),
        KeyCode::Delete => (KeyName::Delete, None),
        KeyCode::Left => (KeyName::Left, None),
        KeyCode::Right => (KeyName::Right, None),
        KeyCode::Up => (KeyName::Up, None),
        KeyCode::Down => (KeyName::Down, None),
        KeyCode::Home => (KeyName::Home, None),
        KeyCode::End => (KeyName::End, None),
        KeyCode::PageUp => (KeyName::PageUp, None),
        KeyCode::PageDown => (KeyName::PageDown, None),
        KeyCode::Tab => (KeyName::Tab, None),
        KeyCode::BackTab => (KeyName::Backtab, None),
        KeyCode::Char(character) => (KeyName::Char, Some(character)),
        _ => return None,
    };
    let mut mods = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        mods.push(Modifier::Ctrl);
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        mods.push(Modifier::Shift);
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        mods.push(Modifier::Alt);
    }
    if key.modifiers.contains(KeyModifiers::SUPER) {
        mods.push(Modifier::Meta);
    }
    Some(InputEvent::Key {
        key: key_name,
        char: character,
        mods,
    })
}
