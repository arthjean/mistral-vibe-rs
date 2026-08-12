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
use super::clipboard::{SystemClipboard, SystemClipboardPort};
use super::clipboard_images::ClipboardImageManager;
use super::composer::{
    apply_effects as apply_composer_effects, apply_event as apply_composer_event,
    normalized_key_event as normalized_input_event,
};
use super::controls::ControlState;
use super::history::PromptHistory;
use super::input::{ExternalEditorPort, PromptEditor, SystemExternalEditor};
use super::path_normalization::PathNormalizationManager;
use super::prompt::PromptContext;
use super::remote_project_workflow::{handle_project_action, handle_teleport_push_response};
use super::setup::ResolvedTheme;
use super::shell::interrupt_shell;
use super::state::{EntryStatus, TuiState};
use super::submission::restore_draft;
use super::terminal::{CrosstermOps, TerminalGuard};
use super::workflow::{
    CommandAction, OverlayEffect, OverlayKeyResult, SystemUrlOpener, cycle_agent, dispatch_command,
    execute_mcp_effect, handle_overlay_key, handle_runtime_command, show_rewind,
};
use super::{
    ActiveTurn, Arguments, CliError, InteractiveRuntime, callback, copy_transcript_selection,
    emit_attention, exit, feedback, interaction, is_exit_command, page_older_debug_logs,
    page_older_history, push_local_notice, render, request_active_turn_interrupt,
    settle_transcript_pointer, stop_narration, submission, suspend_session, teleport_available,
    unix_millis,
};

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
            if should_defer_submission(key, context.path_normalization.has_pending()) {
                *context.deferred_enter = Some(key);
                return Ok(false);
            }
            return handle_key(key, context).await;
        }
        // Reference `on_app_focus` and `on_app_blur`.
        Event::FocusGained => {
            if let Some(effect) = context.state.notifier.on_focus() {
                emit_attention(context.state, &effect);
            }
        }
        Event::FocusLost => context.state.notifier.on_blur(),
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
                });
            } else if let Some(cell) = context.state.transcript_view.cell_at(column, row) {
                if extend_selection {
                    context.state.transcript_view.extend_selection(cell);
                } else {
                    context.state.transcript_view.begin_selection(cell);
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            settle_transcript_pointer(context.state, column, row).await;
        }
        _ => {}
    }
}

pub(super) const fn accepts_key_event(kind: KeyEventKind) -> bool {
    matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
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
        .set_teleport_available(teleport_available(context.runtime.as_ref()));
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
    if key.code == KeyCode::BackTab
        || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
    {
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
                        handle_project_action(
                            action,
                            context.working_directory,
                            runtime,
                            context.state,
                        );
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
    match key.code {
        // Reference `action_copy_selection`: the transcript selection wins when
        // one exists, otherwise the composer keeps the shortcut.
        KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            copy_selection(context);
        }
        KeyCode::Char('c') => return interrupt_or_quit(key, context).await.map(Some),
        KeyCode::Char('d') => {
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
        // Reference `action_suspend_with_message`.
        KeyCode::Char('z') => {
            suspend_session(context.terminal_guard, context.terminal, context.state)?;
        }
        KeyCode::Char('o') => {
            context.state.tools_collapsed = !context.state.tools_collapsed;
            let message = if context.state.tools_collapsed {
                "Tool output collapsed"
            } else {
                "Tool output expanded"
            };
            context.state.push_diagnostic(message);
        }
        KeyCode::Char('y' | 'Y') => copy_selection(context),
        KeyCode::Char('g') => open_external_editor(key, context)?,
        _ => return Ok(None),
    }
    Ok(Some(false))
}

fn copy_selection(context: &mut KeyContext<'_>) {
    let copied = copy_transcript_selection(context.state)
        .or_else(|| copy_prompt_selection(context.input.editor(), context.state));
    if let Some(copied) = copied {
        super::report_copied_text(context.runtime.as_ref(), &copied);
    }
}

/// Reference `action_interrupt_or_quit`: an active turn, then a shell, then a
/// draft, then narration, then queued intent, and only an idle session quits.
async fn interrupt_or_quit(key: KeyEvent, context: &mut KeyContext<'_>) -> Result<bool, CliError> {
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
    let edited = ExternalEditorPort::edit(&mut external, &text);
    context
        .terminal_guard
        .resume()
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    context
        .terminal
        .clear()
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    match edited {
        Ok(edited) => {
            context.compose(InputEvent::ExternalEditor { text: Some(edited) });
        }
        Err(error) => context.state.push_diagnostic(error),
    }
    Ok(())
}

/// The unmodified keys: submission, scrolling, selection, and plain editing.
async fn navigate(key: KeyEvent, context: &mut KeyContext<'_>) -> Result<bool, CliError> {
    match key.code {
        KeyCode::Esc => escape(key, context).await,
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            context.compose_key(key);
        }
        KeyCode::Enter => return submit(key, context).await,
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

/// `Enter`: resume a paused queue or route one submitted line.
async fn submit(key: KeyEvent, context: &mut KeyContext<'_>) -> Result<bool, CliError> {
    if resume_paused_queue(context.input.editor(), context.state) {
        return Ok(false);
    }
    let Some(submitted) = take_submission(key, context) else {
        return Ok(false);
    };
    let runtime_busy = context.active.is_some()
        || context.shell_running()
        || context.state.prompt_queue.is_paused();
    if !runtime_busy && is_exit_command(&submitted) {
        return Ok(true);
    }
    let command_action = dispatch_command(
        &submitted,
        context.arguments,
        context.working_directory,
        context.runtime,
        context.state,
        context.input,
        runtime_busy,
    )
    .await;
    context.refresh_composer();
    match command_action {
        CommandAction::Exit => return Ok(true),
        CommandAction::ClipboardImageRequested => context.clipboard_images.schedule(true),
        CommandAction::RejectedBusy => restore_draft(
            context.input,
            submitted,
            context.working_directory,
            context.state,
        ),
        CommandAction::Handled => {}
        CommandAction::Runtime(command) => {
            let turn_active = context.active.is_some();
            handle_runtime_command(
                &command,
                context.working_directory,
                context.runtime,
                context.state,
                context.controls,
                context.input,
                context.theme,
                turn_active,
            )
            .await;
        }
        // A command that resolves to a model turn takes the same path a typed
        // line takes, so it queues while busy and prepares images the same way.
        CommandAction::Prompt(prompt) => {
            let input = &mut *context.input;
            submission::execute(
                prompt,
                runtime_busy,
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
        }
        CommandAction::Unhandled => {
            let input = &mut *context.input;
            submission::execute(
                submitted,
                runtime_busy,
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

pub(super) fn copy_prompt_selection(editor: &PromptEditor, state: &mut TuiState) -> Option<String> {
    let selection = editor.selected_text()?;
    match SystemClipboard.copy_text(&selection) {
        Ok(()) => push_local_notice(
            state,
            "Selection copied to clipboard",
            EntryStatus::Completed,
        ),
        Err(_) => state.push_diagnostic("Failed to copy: clipboard not available"),
    }
    Some(selection)
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
