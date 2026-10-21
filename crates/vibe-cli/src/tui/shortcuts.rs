//! Terminal event routing: every key, mouse, paste, resize, and focus event the
//! session reacts to.
//!
//! The handlers share one [`KeyContext`] instead of threading sixteen borrows
//! through each other, so a new dependency is a field rather than another
//! parameter on four signatures.

use std::path::Path;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use super::chat_input::{ChatInputState, InputEffect, InputEvent, VoicePhase};
use super::clipboard_images::ClipboardImageManager;
use super::composer::{
    apply_effects as apply_composer_effects, apply_event as apply_composer_event,
    normalized_key_event as normalized_input_event,
};
use super::controls::ControlState;
use super::history::PromptHistory;
use super::input::{ExternalEditorPort, PromptEditor, SystemExternalEditor};
use super::path_normalization::PathNormalizationManager;
use super::prompt::{PromptContext, start_injected_prompt, start_prompt};
use super::queue::{
    enter_queue_selection, leave_queue_edit, queue_selection_key, steer_queued_prompts,
    submit_queue_edit,
};
use super::remote_project_workflow::{handle_project_action, handle_teleport_push_response};
use super::setup::ResolvedTheme;
use super::shell::interrupt_shell;
use super::state::TuiState;
use super::submission::{Occupancy, Route, classify, restore_draft, route};
use super::terminal::{CrosstermOps, TerminalGuard};
use super::workflow::{
    FollowUp, LiveBackend, OverlayEffect, OverlayKeyResult, SystemUrlOpener, apply_value_edit,
    cancel_value_edit, cycle_agent, execute_mcp_effect, handle_overlay_key, run_command,
    show_rewind,
};
use super::{
    ActiveTurn, Arguments, CliError, InteractiveRuntime, callback, command_context, exit, feedback,
    help, interaction, page_older_debug_logs, page_older_history, render,
    request_active_turn_interrupt, settle_transcript_pointer, stop_narration, submission,
    suspend_session, unix_millis,
};

/// The global chords the help document advertises.
///
/// Every one of them is routed by [`chord_of`] rather than by an inline key
/// test, so `crates/vibe-cli/src/tui/help.rs` can declare the key events each of
/// its eight lines names and a test can hold the two together: a help line
/// naming a key this function does not resolve fails the suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Chord {
    Submit,
    Newline,
    Escape,
    Quit,
    ExternalEditor,
    ToggleTools,
    CycleAgent,
}

/// Which global chord a key names, if any.
///
/// [`handle_key`], [`control_chord`] and [`navigate`] route by this, and the
/// answer comes out of [`help::SHORTCUTS`], the table the help document is
/// printed from. Binding and documentation are therefore the same declaration:
/// a line advertising a key resolves that key, and a key no line declares is
/// left to the composer.
pub(super) fn chord_of(key: KeyEvent) -> Option<Chord> {
    help::chord_for(key)
}

/// Everything a terminal event may read or mutate.
pub(super) struct KeyContext<'a> {
    pub arguments: &'a Arguments,
    pub working_directory: &'a Path,
    pub runtime: &'a mut Option<InteractiveRuntime>,
    pub active: &'a mut Option<ActiveTurn>,
    pub state: &'a mut TuiState,
    pub controls: &'a mut ControlState,
    pub prompt_history: &'a mut PromptHistory,
    pub input: &'a mut ChatInputState,
    pub theme: &'a mut ResolvedTheme,
    pub terminal_guard: &'a mut TerminalGuard<CrosstermOps<std::io::Stdout>>,
    pub terminal: &'a mut Terminal<CrosstermBackend<std::io::Stdout>>,
    pub clipboard_images: &'a mut ClipboardImageManager,
    pub path_normalization: &'a mut PathNormalizationManager,
    /// A submission held back until path normalization settles.
    pub deferred_enter: &'a mut Option<KeyEvent>,
}

impl KeyContext<'_> {
    /// Feeds one event to the composer and returns what it asks of the session.
    fn compose(&mut self, event: InputEvent) -> Vec<InputEffect> {
        apply_composer_event(self.input, event, self.working_directory, self.state)
    }

    /// Feeds the key to the composer, if the key maps to a composer event at all.
    fn compose_key(&mut self, key: KeyEvent) -> Vec<InputEffect> {
        normalized_input_event(key).map_or_else(Vec::new, |event| self.compose(event))
    }

    /// Republishes composer state after something outside the composer changed it.
    fn refresh_composer(&mut self) {
        let effects = self.input.refresh_after_adapter_mutation();
        apply_composer_effects(self.input, effects, self.working_directory, self.state);
    }

    /// Drops images no queued prompt still references.
    async fn discard_unreferenced_images(&mut self) {
        let protected = self.state.prompt_queue.transient_images();
        self.clipboard_images
            .discard_unreferenced(&protected, self.state)
            .await;
    }

    fn shell_running(&self) -> bool {
        self.runtime
            .as_ref()
            .is_some_and(|runtime| runtime.shell.is_some())
    }
}

/// Routes one terminal event. `Ok(true)` ends the session.
pub(super) async fn handle_terminal_event(
    event: Event,
    context: &mut KeyContext<'_>,
) -> Result<bool, CliError> {
    match event {
        Event::Resize(width, height) => {
            context.state.resize(width, height);
            context.compose(InputEvent::Resize { width, height });
        }
        Event::Paste(text) => {
            let effects = context.compose(InputEvent::Paste { text });
            context.clipboard_images.schedule_effects(&effects);
            context
                .path_normalization
                .schedule_effects(&effects)
                .map_err(CliError::Terminal)?;
        }
        Event::Mouse(mouse) => handle_mouse(mouse.kind, mouse.column, mouse.row, context).await,
        Event::Key(key) if accepts_key_event(key.kind) => {
            let key = typed_key(key);
            if should_defer_submission(key, context.path_normalization.has_pending()) {
                *context.deferred_enter = Some(key);
                return Ok(false);
            }
            return handle_key(key, context).await;
        }
        // Reference `on_app_focus` and `on_app_blur`.
        Event::FocusGained => {
            let effect = context.state.notifier.on_focus();
            context.state.attend(effect);
        }
        Event::FocusLost => {
            let effect = context.state.notifier.on_blur();
            context.state.attend(effect);
        }
        Event::Key(_) => {}
    }
    Ok(false)
}

async fn handle_mouse(kind: MouseEventKind, column: u16, row: u16, context: &mut KeyContext<'_>) {
    match kind {
        MouseEventKind::ScrollUp => {
            context.state.scroll_up(3);
            page_older_history(context.runtime.as_mut(), context.state);
        }
        MouseEventKind::ScrollDown => {
            context.state.scroll_down(3);
        }
        // Reference `SubagentList`: a click selects the row under it.
        MouseEventKind::Down(MouseButton::Left)
            if context.state.overlay.is_none()
                && context.state.subagent_rows.is_some_and(|(area, _)| {
                    area.contains(ratatui::layout::Position::new(column, row))
                }) =>
        {
            if let Some((area, first)) = context.state.subagent_rows {
                let target = first + usize::from(row - area.y);
                context.state.subagents.focus_first();
                context.state.subagents.select_row(target);
            }
        }
        // Reference `TodoStatusRow.on_click`, which opens the todo overlay.
        MouseEventKind::Down(MouseButton::Left)
            if context.state.overlay.is_none()
                && context.state.todo_row.is_some_and(|area| {
                    area.contains(ratatui::layout::Position::new(column, row))
                }) =>
        {
            context.state.overlay = Some(super::workflow::todos_overlay(context.state));
        }
        MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) => {
            let screen = Rect::new(0, 0, context.state.viewport.0, context.state.viewport.1);
            let editor_cell = render::editor_mouse_cell(
                context.input.editor(),
                screen,
                false,
                context.input.mode(),
                column,
                row,
            );
            let extend_selection = matches!(kind, MouseEventKind::Drag(_));
            if let Some((cell_row, cell_column)) = editor_cell {
                context.compose(InputEvent::Mouse {
                    x: u16::try_from(cell_column).unwrap_or(u16::MAX),
                    y: u16::try_from(cell_row).unwrap_or(u16::MAX),
                    extend_selection,
                    at_ms: unix_millis(),
                });
            } else if let Some(cell) = context.state.transcript_view.cell_at(column, row) {
                if extend_selection {
                    context.state.transcript_view.extend_selection(cell);
                } else {
                    context
                        .state
                        .transcript_view
                        .press(cell, (column, row), unix_millis());
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            settle_transcript_pointer(context.runtime.as_mut(), context.state, column, row).await;
            // Reference `on_mouse_up`: every release copies what is selected.
            if context.state.autocopy_to_clipboard {
                copy_selection(context);
            }
        }
        _ => {}
    }
}

pub(super) const fn accepts_key_event(kind: KeyEventKind) -> bool {
    matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

/// The key as the operator typed it.
///
/// Windows reports AltGr as `Ctrl+Alt` beside the character the layout
/// produced, so `AltGr+0` on French AZERTY arrives as `Ctrl+Alt+@`. Left as a
/// chord it is swallowed with every other unclaimed `Ctrl` chord; read through
/// [`altgr_text`] it is the `@` that was typed. Other platforms deliver AltGr
/// characters without modifiers and keep their keys as they come.
pub(super) fn typed_key(key: KeyEvent) -> KeyEvent {
    if cfg!(windows) {
        altgr_text(key).unwrap_or(key)
    } else {
        key
    }
}

/// A `Ctrl+Alt` key that carries a printable character, as text.
///
/// An ASCII letter is the exception: `Ctrl+Alt+A` reports its own unshifted
/// key, `a`, and no AZERTY, QWERTZ or US AltGr position produces one, so it
/// stays a chord.
pub(super) fn altgr_text(key: KeyEvent) -> Option<KeyEvent> {
    let KeyCode::Char(character) = key.code else {
        return None;
    };
    let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
    (key.modifiers.contains(altgr) && !character.is_control() && !character.is_ascii_alphabetic())
        .then(|| KeyEvent {
            modifiers: key.modifiers.difference(altgr),
            ..key
        })
}

pub(super) fn should_defer_submission(key: KeyEvent, normalization_pending: bool) -> bool {
    normalization_pending && key.code == KeyCode::Enter && key.modifiers.is_empty()
}

/// Routes one key press. `Ok(true)` ends the session.
///
/// Ordering is deliberate: modal consumers (callback, overlay, voice, feedback)
/// claim the key before any global chord, and the composer is the last resort.
pub(super) async fn handle_key(
    key: KeyEvent,
    context: &mut KeyContext<'_>,
) -> Result<bool, CliError> {
    context
        .input
        .set_command_context(command_context(context.runtime.as_ref()));
    // Reference priority bindings `ctrl+y` and `ctrl+shift+c`: a copy reaches
    // past every prompt and picker.
    if is_copy_chord(key) {
        copy_selection(context);
        return Ok(false);
    }
    let queue_selectable = context.state.queue_selection.is_none()
        && !context.state.prompt_queue.is_empty()
        && (context.active.is_some() || context.shell_running());
    context.input.set_queue_selectable(queue_selectable);
    if callback::handle_key(
        key,
        context.runtime,
        context.active,
        context.state,
        context.controls,
        context.terminal_guard,
        context.terminal,
    )? {
        return Ok(false);
    }
    if handle_modal_key(key, context).await {
        return Ok(false);
    }
    // Reference `SubagentList` holds the keyboard while it has the focus.
    if context.state.subagents.handle_key(key) != super::subagents::ListKey::Ignored {
        return Ok(false);
    }
    context.state.last_keystroke_ms = unix_millis();
    if queue_selection_key(key, context.input, context.state) {
        return Ok(false);
    }
    if key.code != KeyCode::Esc {
        context.state.rewind_confirmation.cancel();
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
        && key.code == KeyCode::Char('v')
    {
        let effects = context.compose_key(key);
        context.clipboard_images.schedule_effects(&effects);
        return Ok(false);
    }
    if chord_of(key) == Some(Chord::CycleAgent) {
        context.compose_key(key);
        cycle_agent(context.runtime, context.state, context.input);
        return Ok(false);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        // An unclaimed `Ctrl` chord is swallowed rather than typed.
        return match control_chord(key, context).await? {
            Some(exit) => Ok(exit),
            None => {
                if matches!(key.code, KeyCode::Char('a' | 'e' | 'j')) {
                    context.compose_key(key);
                }
                Ok(false)
            }
        };
    }
    navigate(key, context).await
}

/// Overlays, voice capture, and the feedback prompt each swallow the key whole.
/// Returns whether one of them claimed it.
async fn handle_modal_key(key: KeyEvent, context: &mut KeyContext<'_>) -> bool {
    match handle_overlay_key(
        key,
        context.runtime,
        context.state,
        context.controls,
        context.input,
        context.theme,
    )
    .await
    {
        OverlayKeyResult::Unhandled => {}
        OverlayKeyResult::Handled => {
            context.refresh_composer();
            return true;
        }
        OverlayKeyResult::Effect(effect) => {
            if let Some(runtime) = context.runtime.as_mut() {
                match effect {
                    OverlayEffect::Mcp(effect) => {
                        execute_mcp_effect(effect, runtime, context.state, &SystemUrlOpener);
                    }
                    OverlayEffect::RemoteProject(action) => {
                        handle_project_action(action, runtime, context.state);
                    }
                    OverlayEffect::TeleportPush(action) => {
                        handle_teleport_push_response(action, runtime, context.state);
                    }
                }
                context.refresh_composer();
            }
            return true;
        }
    }
    let voice_key = context.input.voice_phase().is_active()
        || (context.input.voice_phase() == VoicePhase::Idle
            && key.code == KeyCode::Char('r')
            && key.modifiers.contains(KeyModifiers::CONTROL));
    if voice_key {
        let effects = context.compose_key(key);
        let generation = context.input.voice_generation();
        if let Some(runtime) = context.runtime.as_mut() {
            runtime.voice.apply_effects(&effects, generation);
        }
        return true;
    }
    if context.input.feedback_active() && key.code == KeyCode::Esc {
        let effects = context.compose_key(key);
        feedback::handle_effects(&effects, context.runtime, context.input, context.state).await;
        return true;
    }
    false
}

/// The `Ctrl`-modified chords. `Ok(None)` leaves the key to the composer.
async fn control_chord(
    key: KeyEvent,
    context: &mut KeyContext<'_>,
) -> Result<Option<bool>, CliError> {
    // Reference `action_copy_selection`: the transcript selection wins when one
    // exists, otherwise the composer keeps the shortcut.
    if matches!(key.code, KeyCode::Char('c' | 'C')) && key.modifiers.contains(KeyModifiers::SHIFT) {
        copy_selection(context);
        return Ok(Some(false));
    }
    match chord_of(key) {
        Some(Chord::Quit) if key.code == KeyCode::Char('c') => {
            return interrupt_or_quit(key, context).await.map(Some);
        }
        Some(Chord::Quit) => {
            if !context.input.editor().text().is_empty() {
                context.compose_key(key);
                return Ok(Some(false));
            }
            return Ok(Some(exit::resolve_quit(
                "Ctrl+D",
                context.state.ask_confirmation_on_exit,
                &mut context.state.quit_confirmation,
                unix_millis(),
            )));
        }
        // Reference `action_toggle_tool`: every section and group takes the new
        // state, whatever it was folded to one at a time, and nothing is said.
        Some(Chord::ToggleTools) => {
            let collapsed = !context.state.tools_collapsed;
            context.state.tools_collapsed = collapsed;
            for (key, folded) in &mut context.state.folds {
                if super::render::follows_tool_toggle(key) {
                    *folded = collapsed;
                }
            }
        }
        Some(Chord::ExternalEditor) => open_external_editor(key, context)?,
        // `Ctrl+J` is the newline chord, and the composer is what inserts it.
        Some(Chord::Newline) => return Ok(None),
        Some(Chord::Submit | Chord::Escape | Chord::CycleAgent) | None => match key.code {
            // Reference `action_suspend_with_message`.
            KeyCode::Char('z') => {
                suspend_session(context.terminal_guard, context.terminal, context.state)?;
            }
            _ => return Ok(None),
        },
    }
    Ok(Some(false))
}

fn is_copy_chord(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && match key.code {
            KeyCode::Char('y' | 'Y') => true,
            KeyCode::Char('c' | 'C') => key.modifiers.contains(KeyModifiers::SHIFT),
            _ => false,
        }
}

fn copy_selection(context: &mut KeyContext<'_>) {
    super::copy_selection(
        context.runtime.as_ref(),
        context.state,
        context.input.editor(),
    );
}

/// Reference `action_interrupt_or_quit`: an active turn, then a shell, then a
/// draft, then narration, then queued intent, and only an idle session quits.
async fn interrupt_or_quit(key: KeyEvent, context: &mut KeyContext<'_>) -> Result<bool, CliError> {
    // Reference `action_interrupt_or_quit`: a child's transcript is read only,
    // so Ctrl+C says so and asks to be pressed again to quit.
    if context.state.subagents.is_viewing() {
        let quit = exit::resolve_quit(
            "Ctrl+C",
            context.state.ask_confirmation_on_exit,
            &mut context.state.quit_confirmation,
            unix_millis(),
        );
        if !quit {
            context.state.subagents.refuse_input();
        }
        return Ok(quit);
    }
    if request_active_turn_interrupt(
        context.runtime,
        context.active,
        context.controls,
        context.state,
    ) {
        return Ok(false);
    }
    if context.shell_running() {
        if let Some(runtime) = context.runtime.as_mut() {
            interrupt_shell(runtime, context.state).await;
        }
        context.state.prompt_queue.pause();
        return Ok(false);
    }
    if !context.input.editor().text().is_empty() {
        context.compose_key(key);
        context.discard_unreferenced_images().await;
        context.state.quit_confirmation.cancel();
        return Ok(false);
    }
    if stop_narration(context.runtime, context.state) {
        return Ok(false);
    }
    if let Some(cancelled) = context.state.prompt_queue.cancel_last() {
        let first_line = cancelled
            .text()
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned();
        context
            .state
            .push_diagnostic(format!("Removed queued prompt: {first_line}"));
        context.discard_unreferenced_images().await;
        return Ok(false);
    }
    Ok(exit::resolve_quit(
        "Ctrl+C",
        context.state.ask_confirmation_on_exit,
        &mut context.state.quit_confirmation,
        unix_millis(),
    ))
}

fn open_external_editor(key: KeyEvent, context: &mut KeyContext<'_>) -> Result<(), CliError> {
    let effects = context.compose_key(key);
    let Some(text) = effects.into_iter().find_map(|effect| match effect {
        InputEffect::OpenExternalEditor { text } => Some(text),
        _ => None,
    }) else {
        return Ok(());
    };
    context
        .terminal_guard
        .restore()
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    let mut external = SystemExternalEditor::from_environment();
    // Reference `ExternalEditor.edit`: an editor that fails or changes nothing
    // leaves the prompt as it was, without a word. One that cannot start, as
    // the `nano` fallback on a stock Windows, is named here instead.
    let edited = match ExternalEditorPort::edit(&mut external, &text) {
        Ok(edited) => edited.filter(|edited| *edited != text),
        Err(error) => {
            context.state.push_diagnostic(error);
            None
        }
    };
    context
        .terminal_guard
        .resume()
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    context
        .terminal
        .clear()
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    context.compose(InputEvent::ExternalEditor { text: edited });
    Ok(())
}

/// The unmodified keys: submission, scrolling, selection, and plain editing.
async fn navigate(key: KeyEvent, context: &mut KeyContext<'_>) -> Result<bool, CliError> {
    match chord_of(key) {
        Some(Chord::Escape) => {
            escape(key, context).await;
            return Ok(false);
        }
        Some(Chord::Newline) => {
            context.compose_key(key);
            return Ok(false);
        }
        Some(Chord::Submit) => return submit(key, context).await,
        _ => {}
    }
    match key.code {
        // Transcript selection stays reachable without a pointer, so copying
        // history never requires mouse reporting.
        KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => {
            context.state.transcript_view.move_selection(1);
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => {
            context.state.transcript_view.move_selection(-1);
        }
        KeyCode::PageUp if key.modifiers.contains(KeyModifiers::ALT) => {
            context.state.prompt_queue.scroll(-5);
        }
        KeyCode::PageDown if key.modifiers.contains(KeyModifiers::ALT) => {
            context.state.prompt_queue.scroll(5);
        }
        KeyCode::PageUp => {
            if context
                .state
                .overlay
                .as_ref()
                .is_some_and(|overlay| overlay.kind == interaction::OverlayKind::Debug)
            {
                page_older_debug_logs(context.runtime.as_mut(), context.state);
                return Ok(false);
            }
            context.state.scroll_up(10);
            page_older_history(context.runtime.as_mut(), context.state);
        }
        KeyCode::PageDown => {
            context.state.scroll_down(10);
        }
        KeyCode::Tab => {
            context.compose_key(key);
        }
        KeyCode::Backspace
        | KeyCode::Delete
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::Up
        | KeyCode::Down
        | KeyCode::Char(_) => {
            let effects = context.compose_key(key);
            if effects
                .iter()
                .any(|effect| matches!(effect, InputEffect::QueueSelectionRequested))
            {
                enter_queue_selection(context.input, context.state);
            }
            // Reference `on_chat_text_area_navigate_below`.
            if effects
                .iter()
                .any(|effect| matches!(effect, InputEffect::NavigateBelow))
            {
                context.state.subagents.focus_first();
            }
            feedback::handle_effects(&effects, context.runtime, context.input, context.state).await;
            context
                .path_normalization
                .schedule_effects(&effects)
                .map_err(CliError::Terminal)?;
        }
        _ => {}
    }
    Ok(false)
}

async fn escape(key: KeyEvent, context: &mut KeyContext<'_>) {
    // Reference `_try_interrupt`: Esc first leaves a subagent's transcript.
    if context.state.subagents.is_viewing() {
        context.state.subagents.show_main(true);
        return;
    }
    // Reference `check_action`: queue mode turns the interrupt off, and Esc
    // drops the edit instead.
    if leave_queue_edit(context.input, context.state) {
        return;
    }
    if let Some(edit) = context.state.value_edit.take() {
        cancel_value_edit(edit, context.runtime, context.input, context.state);
        return;
    }
    if stop_narration(context.runtime, context.state) {
        return;
    }
    let interrupted = request_active_turn_interrupt(
        context.runtime,
        context.active,
        context.controls,
        context.state,
    );
    if !interrupted && context.shell_running() {
        if let Some(runtime) = context.runtime.as_mut() {
            interrupt_shell(runtime, context.state).await;
        }
        context.state.prompt_queue.pause();
    } else if context.active.is_none() && !context.state.prompt_queue.is_empty() {
        context.state.prompt_queue.pause();
        context
            .state
            .push_diagnostic("Queued prompts paused; press Enter on an empty prompt to resume");
    } else if context.active.is_none() && !context.input.editor().text().is_empty() {
        context.compose_key(key);
        context.discard_unreferenced_images().await;
        context.state.rewind_confirmation.cancel();
    } else if context.active.is_none()
        && context
            .state
            .rewind_confirmation
            .request("Esc", unix_millis())
        && let Some(runtime) = context.runtime.as_mut()
    {
        show_rewind(runtime, context.state);
    }
}

/// `Enter`: save a panel field, resume a paused queue, or route one submitted
/// line.
async fn submit(key: KeyEvent, context: &mut KeyContext<'_>) -> Result<bool, CliError> {
    if context.state.value_edit.is_some() {
        let value = take_submission(key, context).unwrap_or_default();
        if let Some(edit) = context.state.value_edit.take() {
            apply_value_edit(edit, &value, context.runtime, context.state);
        }
        context.refresh_composer();
        return Ok(false);
    }
    // Reference `ChatTextArea._on_key`: an open completion takes Enter before
    // the queue edit does.
    let input = &mut *context.input;
    if input.completion().view().is_none()
        && submit_queue_edit(
            PromptContext::new(
                context.working_directory,
                context.runtime,
                context.active,
                context.state,
                context.controls,
                context.clipboard_images,
            ),
            input,
        )
        .await?
    {
        context.refresh_composer();
        return Ok(false);
    }
    if resume_paused_queue(context.input.editor(), context.state) {
        return Ok(false);
    }
    // Reference `_dispatch_submitted_value`: an empty Enter steers what is
    // queued into the running turn, except in queue mode (`_steer_queued_now`).
    if context.input.editor().text().trim().is_empty()
        && !context.state.prompt_queue.is_empty()
        && context.state.queue_selection.is_none()
    {
        steer_queued_prompts(PromptContext::new(
            context.working_directory,
            context.runtime,
            context.active,
            context.state,
            context.controls,
            context.clipboard_images,
        ))
        .await?;
        return Ok(false);
    }
    let Some(submitted) = take_submission(key, context) else {
        return Ok(false);
    };
    // Reference `_dispatch_submitted_value`: the first line sent stills the
    // banner's cat.
    context.state.banner_cat.freeze();
    // Reference `_dispatch_submitted_value` strips the line once, and every
    // route below reads the stripped value.
    let value = submitted.trim().to_owned();
    let command_context = context.input.command_context().clone();
    let occupancy = Occupancy {
        turn: context.active.is_some(),
        shell: context.shell_running(),
        paused: context.state.prompt_queue.is_paused(),
    };
    let kind = classify(&value, &command_context, context.runtime.as_ref());
    // Commands run to completion before the next key is read, so the side
    // channel is always free when a line reaches it.
    let route = route(kind, occupancy, true);
    if route != Route::Command {
        context.refresh_composer();
        let input = &mut *context.input;
        submission::execute(
            value,
            route,
            PromptContext::new(
                context.working_directory,
                context.runtime,
                context.active,
                context.state,
                context.controls,
                context.clipboard_images,
            ),
            input,
        )
        .await?;
        return Ok(false);
    }
    let follow_ups = {
        let mut backend = LiveBackend::new(
            context.arguments,
            context.working_directory,
            context.runtime,
            context.state,
            context.controls,
            context.input,
            context.theme,
            occupancy.turn,
        );
        run_command(&value, &command_context, &mut backend)
            .await
            .unwrap_or_default()
    };
    context.refresh_composer();
    for follow_up in follow_ups {
        match follow_up {
            FollowUp::Exit => return Ok(true),
            FollowUp::ClipboardImage => context.clipboard_images.schedule(true),
            FollowUp::Submit { text, injected } => {
                let mut prompt_context = PromptContext::new(
                    context.working_directory,
                    context.runtime,
                    context.active,
                    context.state,
                    context.controls,
                    context.clipboard_images,
                );
                let draft = prompt_context
                    .clipboard_images
                    .draft(context.working_directory, text);
                let started = if injected {
                    start_injected_prompt(prompt_context.reborrow(), &draft).await?
                } else {
                    start_prompt(prompt_context.reborrow(), &draft).await?
                };
                if !started && !injected {
                    restore_draft(
                        context.input,
                        draft.into_text(),
                        context.working_directory,
                        context.state,
                    );
                }
            }
        }
    }
    Ok(false)
}

/// Takes the submitted line out of the composer.
fn take_submission(key: KeyEvent, context: &mut KeyContext<'_>) -> Option<String> {
    let effects = context.compose_key(key);
    for entry in effects.iter().filter_map(|effect| match effect {
        InputEffect::RecordHistory { entry } => Some(entry),
        _ => None,
    }) {
        context.prompt_history.persist(entry.clone());
    }
    let submitted = effects.into_iter().find_map(|effect| match effect {
        InputEffect::Submit { text } if !text.is_empty() => Some(text),
        _ => None,
    });
    debug_assert!(submitted.is_none() || context.input.editor().text().is_empty());
    submitted
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_adapter_accepts_press_and_repeat_but_not_release() {
        assert!(accepts_key_event(KeyEventKind::Press));
        assert!(accepts_key_event(KeyEventKind::Repeat));
        assert!(!accepts_key_event(KeyEventKind::Release));
    }

    const ALTGR: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::ALT);

    #[test]
    fn altgr_characters_of_azerty_and_qwertz_are_text() {
        // French AZERTY AltGr with `0 3 4 5 6 8 = ) 2 E`, then German QWERTZ
        // AltGr with `Q 7 0`, as Windows reports them.
        for character in "@#{[|\\}]~€@{}".chars() {
            let key = KeyEvent::new(KeyCode::Char(character), ALTGR);
            assert_eq!(
                altgr_text(key),
                Some(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
                "AltGr {character:?} was not read as text",
            );
        }
    }

    #[test]
    fn a_ctrl_alt_letter_stays_a_chord() {
        for character in ['a', 'A', 'z'] {
            let key = KeyEvent::new(KeyCode::Char(character), ALTGR);
            assert_eq!(altgr_text(key), None);
            assert_eq!(typed_key(key), key);
        }
        let control = KeyEvent::new(KeyCode::Char('@'), KeyModifiers::CONTROL);
        assert_eq!(altgr_text(control), None);
    }

    #[test]
    fn a_synthetic_altgr_at_sign_is_typed_and_opens_mention_completion() {
        let key = KeyEvent::new(KeyCode::Char('@'), ALTGR);
        let typed = typed_key(key);
        if !cfg!(windows) {
            assert_eq!(typed, key, "only Windows reports AltGr as Ctrl+Alt");
            return;
        }
        let mut input = ChatInputState::new();
        // The composer's own effects, before the session dispatches the
        // completion request they carry.
        let effects = input.apply(normalized_input_event(typed).expect("a composer key"));
        assert_eq!(input.editor().text(), "@");
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                InputEffect::RequestCompletion { request } if request.query == "@"
            )),
            "the mention completion did not open: {effects:?}",
        );
    }

    #[test]
    fn only_plain_submission_waits_for_path_normalization() {
        assert!(should_defer_submission(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            true,
        ));
        assert!(!should_defer_submission(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            true,
        ));
        assert!(!should_defer_submission(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            false,
        ));
    }
}
