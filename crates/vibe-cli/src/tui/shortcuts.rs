use super::clipboard::{SystemClipboard, SystemClipboardPort};
use super::input::PromptEditor;
use super::push_local_notice;
use super::state::{EntryStatus, TuiState};

pub(super) fn copy_prompt_selection(editor: &PromptEditor, state: &mut TuiState) -> bool {
    let Some(selection) = editor.selected_text() else {
        return false;
    };
    match SystemClipboard.copy_text(&selection) {
        Ok(()) => {
            push_local_notice(
                state,
                "Selection copied to clipboard",
                EntryStatus::Completed,
            );
            true
        }
        Err(_) => {
            state.push_diagnostic("Failed to copy: clipboard not available");
            true
        }
    }
}

pub(super) fn resume_paused_queue(editor: &PromptEditor, state: &mut TuiState) -> bool {
    if !state.prompt_queue.is_paused() || !editor.text().trim().is_empty() {
        return false;
    }
    state.prompt_queue.resume();
    state.push_diagnostic(format!(
        "Queued prompts resumed ({} pending)",
        state.prompt_queue.len()
    ));
    true
}
