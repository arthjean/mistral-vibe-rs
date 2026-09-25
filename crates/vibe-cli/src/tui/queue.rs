use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::attachments::{PromptDraft, merge_submissions};
use super::chat_input::ChatInputState;
use super::interaction::QueuedIntent;
use super::prompt::{
    PromptContext, enqueue_prompt, prepare_prompt_for_runtime, report_prompt_telemetry,
    start_prompt_with_client_id,
};
use super::state::{QueueSelection, TuiState};
use super::turn::CancellationPhase;
use super::{CliError, unix_millis};
use vibe_app_server::client::{ClientError, ProtocolErrorCode};

enum BatchOutcome {
    Complete,
    Pause,
}

/// Reference `TurnController._promote_next` for the one item the terminal
/// keeps: once the session is free, every queued prompt starts as one turn.
pub(super) async fn start_next_queued_prompt(
    mut context: PromptContext<'_>,
) -> Result<(), CliError> {
    if context.active.is_some()
        || context
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.shell.is_some())
        || context.controls.pending_callback().is_some()
    {
        return Ok(());
    }
    let Some(mut batch) = context.state.prompt_queue.take_next_batch() else {
        return Ok(());
    };

    let result = process_batch(&mut context, &mut batch).await;

    match result {
        Ok(BatchOutcome::Complete) => batch.clear(),
        Ok(BatchOutcome::Pause) => context.state.prompt_queue.restore_batch_and_pause(batch),
        Err(error) => {
            context.state.prompt_queue.restore_batch_and_pause(batch);
            return Err(error);
        }
    }
    Ok(())
}

async fn process_batch(
    context: &mut PromptContext<'_>,
    batch: &mut [QueuedIntent],
) -> Result<BatchOutcome, CliError> {
    if context
        .runtime
        .as_ref()
        .is_some_and(|runtime| !runtime.supports_images())
        && batch.iter().any(|item| {
            item.prepared
                .as_ref()
                .is_some_and(|prepared| !prepared.provider_images.as_slice().is_empty())
        })
    {
        context.state.push_diagnostic(
            "The active model does not support queued images; switch models and resume the queue",
        );
        return Ok(BatchOutcome::Pause);
    }
    for item in batch.iter_mut() {
        if item.prepared.is_none() {
            let Some(prepared) = prepare_prompt_for_runtime(
                context.working_directory,
                &item.draft,
                context.runtime,
                context.state,
            )
            .await?
            else {
                return Ok(BatchOutcome::Pause);
            };
            item.prepared = Some(prepared);
        }
    }
    let submissions = batch
        .iter()
        .filter_map(|item| item.prepared.clone())
        .collect::<Vec<_>>();
    let merged = match merge_submissions(submissions) {
        Ok(merged) => merged,
        Err(error) => {
            context.state.push_diagnostic(error.to_string());
            return Ok(BatchOutcome::Pause);
        }
    };
    // Reference `_report_prompt` runs once per queued prompt as its turn
    // starts; the first one reports with the turn it opens.
    if let Some(runtime) = context.runtime.as_ref() {
        for item in batch.iter().skip(1) {
            if let Some(prepared) = &item.prepared {
                report_prompt_telemetry(
                    runtime,
                    &prepared.turn.prompt,
                    &prepared.mention_stats,
                    Some(item.id.clone()),
                );
            }
        }
    }
    let draft = PromptDraft::merged(batch.iter().map(|item| &item.draft));
    if start_prompt_with_client_id(context.reborrow(), &draft, Some(&batch[0].id), Some(merged))
        .await?
    {
        return Ok(BatchOutcome::Complete);
    }
    Ok(BatchOutcome::Pause)
}

/// Reference `_steer_queued_now` over `QueueController._steer_pending_legacy`:
/// an empty Enter while a turn runs sends every queued prompt into it as one
/// steering message instead of waiting for the turn to end. Nothing happens
/// unless a turn is running and something is queued; a paused queue answers
/// the same Enter by resuming instead.
///
/// The queued prompts leave the queue only once the turn accepts them. A turn
/// that already ended refuses them quietly and they promote as the next turn,
/// as the reference treats `STALE_TURN`; any other refusal is reported.
/// A queue holding an image stays queued, since this port's steering carries
/// text alone.
pub(super) async fn steer_queued_prompts(context: PromptContext<'_>) -> Result<bool, CliError> {
    let queue = &context.state.prompt_queue;
    if queue.is_empty() || queue.is_paused() {
        return Ok(false);
    }
    let Some(turn_id) = context
        .active
        .as_ref()
        .filter(|active| active.cancellation == CancellationPhase::Active)
        .map(|active| active.turn_id.clone())
    else {
        return Ok(false);
    };
    if !queue.transient_images().is_empty() {
        return Ok(false);
    }
    let mut batch = context.state.prompt_queue.take_all();
    for item in &mut batch {
        if item.prepared.is_none() {
            item.prepared = prepare_prompt_for_runtime(
                context.working_directory,
                &item.draft,
                context.runtime,
                context.state,
            )
            .await?;
        }
    }
    let submissions = batch
        .iter()
        .filter_map(|item| item.prepared.clone())
        .collect::<Vec<_>>();
    let carries_images = submissions
        .iter()
        .any(|prepared| !prepared.provider_images.as_slice().is_empty());
    if submissions.len() != batch.len() || carries_images {
        context.state.prompt_queue.restore_batch(batch);
        return Ok(false);
    }
    let merged = match merge_submissions(submissions) {
        Ok(merged) => merged,
        Err(error) => {
            context.state.prompt_queue.restore_batch(batch);
            context.state.push_diagnostic(error.to_string());
            return Ok(false);
        }
    };
    let Some(runtime) = context.runtime.as_mut() else {
        context.state.prompt_queue.restore_batch(batch);
        return Ok(false);
    };
    let session_id = runtime.session_id.clone();
    match runtime.service.steer(
        &session_id,
        &turn_id,
        &merged.turn.input,
        Some(&batch[0].id),
    ) {
        Ok(()) => {
            for item in &batch {
                if let Some(prepared) = &item.prepared {
                    report_prompt_telemetry(
                        runtime,
                        &prepared.turn.prompt,
                        &prepared.mention_stats,
                        Some(item.id.clone()),
                    );
                }
            }
            Ok(true)
        }
        Err(error) => {
            context.state.prompt_queue.restore_batch(batch);
            if !matches!(
                error,
                ClientError::Protocol(ProtocolErrorCode::StaleTurn, _)
            ) {
                context
                    .state
                    .push_diagnostic(format!("Steering failed: {error}"));
            }
            Ok(false)
        }
    }
}

/// Reference `ChatInputBody._try_enter_queue_selection`.
pub(super) const SELECTION_HINT: &str =
    "Up/Down: select  \u{b7}  Enter: edit  \u{b7}  Backspace/Delete: remove  \u{b7}  Esc: exit";
/// Reference `on_chat_text_area_queue_selection_enter`.
pub(super) const EDIT_HINT: &str = "Enter to save \u{b7} Esc to discard";
/// This port's wording for the notice the reference raises when the prompt
/// being edited started before the edit was saved.
pub(super) const CONSUMED_HINT: &str = "That prompt already started: press Enter to queue the edit as a new prompt, or Esc to drop it.";

/// Reference `ChatInputBody._try_enter_queue_selection`: Up at the top of the
/// composer selects the newest queued prompt and locks the draft.
pub(super) fn enter_queue_selection(input: &ChatInputState, state: &mut TuiState) -> bool {
    let Some(selected) = state
        .prompt_queue
        .newest_first()
        .first()
        .map(|(id, _)| (*id).to_owned())
    else {
        return false;
    };
    state.prompt_queue.reveal(&selected);
    state.queue_selection = Some(QueueSelection {
        selected,
        position: 0,
        original: input.editor().text().to_owned(),
        editing: false,
        consumed: false,
    });
    state.show_inline_notice(SELECTION_HINT, Some(3_000), unix_millis());
    true
}

/// Reference `ChatInputBody._exit_queue_mode`: the draft the queue took comes
/// back.
fn exit_queue_selection(input: &mut ChatInputState, state: &mut TuiState) {
    let Some(selection) = state.queue_selection.take() else {
        return;
    };
    if selection.editing {
        state.inline_notice = None;
    }
    input.replace_text(selection.original);
}

/// Reference `_resync_selection`: follows the selected prompt by its id, and
/// when a promotion took it, the same place from the newest selects the next.
/// Answers false once the queue emptied, which leaves the mode.
fn resync(input: &mut ChatInputState, state: &mut TuiState) -> bool {
    let newest = state.prompt_queue.newest_first();
    let Some(selection) = state.queue_selection.as_mut() else {
        return false;
    };
    if newest.is_empty() {
        exit_queue_selection(input, state);
        return false;
    }
    if let Some(position) = newest.iter().position(|(id, _)| *id == selection.selected) {
        selection.position = position;
    } else {
        let position = selection.position.min(newest.len() - 1);
        selection.selected = newest[position].0.to_owned();
        selection.position = position;
        state.prompt_queue.reveal(&selection.selected.clone());
    }
    true
}

fn select_position(state: &mut TuiState, position: usize) {
    let Some(id) = state
        .prompt_queue
        .newest_first()
        .get(position)
        .map(|(id, _)| (*id).to_owned())
    else {
        return;
    };
    state.prompt_queue.reveal(&id);
    if let Some(selection) = state.queue_selection.as_mut() {
        selection.selected = id;
        selection.position = position;
    }
}

/// Reference `ChatTextArea._on_key` while a queued prompt is selected: Up and
/// Down walk the queue, Enter edits, Backspace and Delete remove, Esc leaves,
/// and the locked draft ignores typing. Chords pass through.
pub(super) fn queue_selection_key(
    key: KeyEvent,
    input: &mut ChatInputState,
    state: &mut TuiState,
) -> bool {
    if state
        .queue_selection
        .as_ref()
        .is_none_or(|selection| selection.editing)
        || !key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
    {
        return false;
    }
    match key.code {
        KeyCode::Esc => exit_queue_selection(input, state),
        KeyCode::Up if resync(input, state) => {
            let position = state.queue_selection.as_ref().map_or(0, |s| s.position);
            if position + 1 < state.prompt_queue.len() {
                select_position(state, position + 1);
            }
        }
        KeyCode::Down if resync(input, state) => {
            match state.queue_selection.as_ref().map_or(0, |s| s.position) {
                0 => exit_queue_selection(input, state),
                position => select_position(state, position - 1),
            }
        }
        KeyCode::Enter if resync(input, state) => {
            let Some(selection) = state.queue_selection.as_mut() else {
                return true;
            };
            selection.editing = true;
            let text = state
                .prompt_queue
                .newest_first()
                .get(selection.position)
                .map(|(_, text)| (*text).to_owned())
                .unwrap_or_default();
            input.replace_text(text);
            state.show_inline_notice(EDIT_HINT, None, unix_millis());
        }
        KeyCode::Backspace | KeyCode::Delete if resync(input, state) => {
            let Some(selection) = state.queue_selection.as_ref() else {
                return true;
            };
            let (id, position) = (selection.selected.clone(), selection.position);
            state.prompt_queue.remove(&id);
            if state.prompt_queue.is_empty() {
                exit_queue_selection(input, state);
            } else {
                select_position(state, position.saturating_sub(1));
            }
        }
        _ => {}
    }
    true
}

/// Reference `on_chat_text_area_queue_edit_cancelled` and
/// `_end_edit_mode_back_to_selection`: the edit is dropped and the selection
/// returns.
pub(super) fn leave_queue_edit(input: &mut ChatInputState, state: &mut TuiState) -> bool {
    let Some(selection) = state
        .queue_selection
        .as_mut()
        .filter(|selection| selection.editing)
    else {
        return false;
    };
    selection.editing = false;
    selection.consumed = false;
    let selected = selection.selected.clone();
    input.replace_text("");
    state.inline_notice = None;
    state.prompt_queue.reveal(&selected);
    true
}

/// Reference `ChatInputBody.on_chat_text_area_submitted` in edit mode and the
/// app's `on_chat_input_container_queue_edit_submitted`: the edit rewrites the
/// queued prompt, and once that prompt started it queues as a new one after a
/// confirming second Enter. Answers false when no edit is open.
pub(super) async fn submit_queue_edit(
    context: PromptContext<'_>,
    input: &mut ChatInputState,
) -> Result<bool, CliError> {
    let Some(selection) = context
        .state
        .queue_selection
        .as_ref()
        .filter(|selection| selection.editing)
        .cloned()
    else {
        return Ok(false);
    };
    let value = input.editor().text().trim().to_owned();
    if value.is_empty() {
        return Ok(true);
    }
    let draft = context
        .clipboard_images
        .draft(context.working_directory, value);
    if selection.consumed {
        context.state.inline_notice = None;
        if context.state.prompt_queue.is_empty() {
            context.state.queue_selection = None;
            input.replace_text(selection.original);
        } else {
            resync(input, context.state);
            leave_queue_edit(input, context.state);
        }
        enqueue_prompt(
            context.working_directory,
            &draft,
            context.runtime,
            context.state,
        )
        .await?;
        return Ok(true);
    }
    if context
        .state
        .prompt_queue
        .index_of(&selection.selected)
        .is_none()
    {
        if let Some(open) = context.state.queue_selection.as_mut() {
            open.consumed = true;
        }
        context
            .state
            .show_inline_notice(CONSUMED_HINT, Some(8_000), unix_millis());
        return Ok(true);
    }
    // The selection returns before the prompt is prepared, so the edit lands
    // on the prompt it was opened on, not wherever the highlight moves.
    if let Some(open) = context.state.queue_selection.as_mut() {
        open.editing = false;
    }
    input.replace_text("");
    context.state.inline_notice = None;
    let Some(prepared) = prepare_prompt_for_runtime(
        context.working_directory,
        &draft,
        context.runtime,
        context.state,
    )
    .await?
    else {
        return Ok(true);
    };
    if !context
        .state
        .prompt_queue
        .replace(&selection.selected, draft.clone(), prepared)
    {
        enqueue_prompt(
            context.working_directory,
            &draft,
            context.runtime,
            context.state,
        )
        .await?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::clipboard_images::ClipboardImageManager;
    use crate::tui::controls::ControlState;

    fn press(code: KeyCode, input: &mut ChatInputState, state: &mut TuiState) -> bool {
        queue_selection_key(KeyEvent::new(code, KeyModifiers::NONE), input, state)
    }

    fn queued(texts: &[&str]) -> (ChatInputState, TuiState) {
        let mut state = TuiState::new("session");
        for text in texts {
            state.prompt_queue.push(PromptDraft::text_only(*text));
        }
        let mut input = ChatInputState::new();
        input.replace_text("draft");
        (input, state)
    }

    fn selected(state: &TuiState) -> Option<&str> {
        let selection = state.queue_selection.as_ref()?;
        state
            .prompt_queue
            .newest_first()
            .into_iter()
            .find(|(id, _)| *id == selection.selected)
            .map(|(_, text)| text)
    }

    #[test]
    fn up_walks_the_queue_from_the_newest_and_down_past_it_restores_the_draft() {
        let (mut input, mut state) = queued(&["first", "second", "third"]);
        assert!(enter_queue_selection(&input, &mut state));
        assert_eq!(selected(&state), Some("third"));
        assert!(state.inline_notice.is_some());
        press(KeyCode::Up, &mut input, &mut state);
        press(KeyCode::Up, &mut input, &mut state);
        press(KeyCode::Up, &mut input, &mut state);
        assert_eq!(
            selected(&state),
            Some("first"),
            "the oldest bounds the walk"
        );
        // The locked draft ignores typing.
        assert!(press(KeyCode::Char('x'), &mut input, &mut state));
        assert_eq!(input.editor().text(), "draft");
        for _ in 0..3 {
            press(KeyCode::Down, &mut input, &mut state);
        }
        assert!(state.queue_selection.is_none());
        assert_eq!(input.editor().text(), "draft");
        assert!(!press(KeyCode::Up, &mut input, &mut state));
    }

    #[test]
    fn removal_selects_the_next_newer_prompt_and_an_empty_queue_leaves_the_mode() {
        let (mut input, mut state) = queued(&["first", "second"]);
        enter_queue_selection(&input, &mut state);
        press(KeyCode::Up, &mut input, &mut state);
        press(KeyCode::Backspace, &mut input, &mut state);
        assert_eq!(state.prompt_queue.len(), 1);
        assert_eq!(selected(&state), Some("second"));
        press(KeyCode::Delete, &mut input, &mut state);
        assert!(state.prompt_queue.is_empty());
        assert!(state.queue_selection.is_none());
        assert_eq!(input.editor().text(), "draft");
    }

    #[tokio::test]
    async fn an_edit_opens_in_the_composer_and_a_started_prompt_asks_before_requeueing() {
        let (mut input, mut state) = queued(&["first"]);
        enter_queue_selection(&input, &mut state);
        press(KeyCode::Enter, &mut input, &mut state);
        assert_eq!(input.editor().text(), "first");
        assert!(state.queue_selection.as_ref().is_some_and(|s| s.editing));

        assert!(leave_queue_edit(&mut input, &mut state));
        assert_eq!(input.editor().text(), "");
        press(KeyCode::Enter, &mut input, &mut state);
        input.replace_text("first, edited");

        // The prompt started while it was being edited.
        let _ = state.prompt_queue.take_all();
        let mut runtime = None;
        let mut active = None;
        let mut controls = ControlState::new("session");
        let mut images = ClipboardImageManager::default();
        let workspace = std::path::PathBuf::from(".");
        let context = PromptContext::new(
            &workspace,
            &mut runtime,
            &mut active,
            &mut state,
            &mut controls,
            &mut images,
        );
        assert!(submit_queue_edit(context, &mut input).await.expect("edit"));
        assert!(state.queue_selection.as_ref().is_some_and(|s| s.consumed));
        assert_eq!(input.editor().text(), "first, edited");
        assert_eq!(
            state
                .inline_notice
                .as_ref()
                .map(|notice| notice.text.as_str()),
            Some(CONSUMED_HINT)
        );
    }

    #[test]
    fn failed_batch_is_restored_before_the_queue_pauses() {
        let mut state = TuiState::new("session");
        state.prompt_queue.push(PromptDraft::text_only("first"));
        state.prompt_queue.push(PromptDraft::text_only("second"));
        let batch = state.prompt_queue.take_next_batch().expect("batch owned");

        state.prompt_queue.restore_batch_and_pause(batch);

        assert!(state.prompt_queue.is_paused());
        assert_eq!(state.prompt_queue.len(), 2);
        state.prompt_queue.resume();
        let restored = state
            .prompt_queue
            .take_next_batch()
            .expect("batch restored");
        assert_eq!(restored[0].draft.text(), "first");
        assert_eq!(restored[1].draft.text(), "second");
    }
}
