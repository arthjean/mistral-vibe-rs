use std::collections::VecDeque;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::tui::chat_input;
    use crate::tui::path_normalization::PathNormalizationManager;

    #[tokio::test]
    async fn unbracketed_drag_and_drop_characters_become_an_image_mention() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let image = workspace.path().join("dropped image.png");
        fs::write(&image, b"image").expect("image fixture");
        let dropped = image.to_string_lossy().replace(' ', "\\ ");
        let mut input = ChatInputState::new();
        let mut state = TuiState::new("session");
        let mut normalization = PathNormalizationManager::new().expect("normalization worker");

        for character in dropped.chars() {
            let effects = apply_event(
                &mut input,
                InputEvent::Key {
                    key: KeyName::Char,
                    char: Some(character),
                    mods: Vec::new(),
                },
                workspace.path(),
                &mut state,
            );
            normalization
                .schedule_effects(&effects)
                .expect("normalization request");
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while normalization.has_pending() {
                let event = normalization
                    .next_event()
                    .await
                    .expect("normalization event");
                let effects = apply_event(&mut input, event, workspace.path(), &mut state);
                normalization
                    .schedule_effects(&effects)
                    .expect("follow-up effects");
            }
        })
        .await
        .expect("normalization completes");

        assert_eq!(
            input.editor().text(),
            format!("@'{}'", image.to_string_lossy())
        );
    }

    #[test]
    fn production_adapter_routes_keys_completion_and_mouse_through_chat_input_state() {
        let temporary = tempfile::tempdir().expect("workspace");
        let mut state = TuiState::new("session");
        let mut input = ChatInputState::new();

        for character in "select me".chars() {
            let event =
                normalized_key_event(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                    .expect("character is normalized");
            apply_event(&mut input, event, temporary.path(), &mut state);
        }
        apply_event(
            &mut input,
            InputEvent::Mouse {
                x: 2,
                y: 0,
                extend_selection: false,
            },
            temporary.path(),
            &mut state,
        );
        apply_event(
            &mut input,
            InputEvent::Mouse {
                x: 7,
                y: 0,
                extend_selection: true,
            },
            temporary.path(),
            &mut state,
        );
        assert_eq!(input.observe().selection, Some([2, 7]));

        let mut external = ChatInputState::new();
        apply_event(
            &mut external,
            InputEvent::ExternalEditor {
                text: Some("/c".to_owned()),
            },
            temporary.path(),
            &mut state,
        );
        assert_eq!(external.mode(), chat_input::InputMode::Prompt);
        assert!(external.completion().view().is_some());
    }
}
