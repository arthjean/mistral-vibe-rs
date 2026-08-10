//! Deterministic chat-input transition boundary.
//!
//! The composer is expressed as `ChatInputState + InputEvent -> InputEffect`
//! so an event sequence can be replayed without a terminal or an operating
//! system service. The transition performs no filesystem, clipboard, editor,
//! microphone, transcription or timer work: every such request leaves the
//! boundary as an effect and returns as an event.
//!
//! Editing lives in [`PromptEditor`] and completion in [`CompletionEngine`];
//! this module owns only the event vocabulary, the ordering between the two,
//! and the observation schema shared with the reference traces recorded in
//! `tests/parity`.

use std::ops::Range;
use std::path::Path;

use unicode_segmentation::UnicodeSegmentation;

use super::commands::{CommandContext, CommandId};
use super::completion::{
    CompletionCandidate, CompletionEngine, CompletionKey, CompletionKeyOutcome, CompletionKind,
    CompletionRequest, CompletionResolution, active_token,
};
use super::composer_layout::ComposerLayout;
use super::input::{InputError, PromptEditor, composer_content_width};
use super::voice::{VoiceCommand, VoiceState, VoiceUpdate, VoiceUpdateOutcome};

pub use super::voice::VoicePhase;

#[path = "chat_input/observation.rs"]
mod observation;
#[path = "chat_input/protocol.rs"]
mod protocol;

pub use observation::{
    CompletionItemObservation, CompletionObservation, HistoryObservation, RenderObservation,
    StateObservation,
};
pub use protocol::{
    EditorSnapshot, InputEffect, InputEvent, InputMode, KeyName, Modifier, Safety, Severity,
};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Everything the composer owns between two events.
#[derive(Debug)]
pub struct ChatInputState {
    editor: PromptEditor,
    completion: CompletionEngine,
    mode: InputMode,
    feedback_active: bool,
    switching: bool,
    safety: Safety,
    agent_name: String,
    voice: VoiceState,
    secret_input: bool,
    history_navigating: bool,
    content_width: usize,
    viewport_height: u16,
    last_paste: Option<Range<usize>>,
}

impl Default for ChatInputState {
    fn default() -> Self {
        Self {
            editor: PromptEditor::default(),
            completion: CompletionEngine::default(),
            mode: InputMode::Prompt,
            feedback_active: false,
            switching: false,
            safety: Safety::Neutral,
            agent_name: String::new(),
            voice: VoiceState::default(),
            secret_input: false,
            history_navigating: false,
            content_width: 71,
            viewport_height: 24,
            last_paste: None,
        }
    }
}

impl ChatInputState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_secret_input(&mut self, secret: bool) {
        self.secret_input = secret;
        if secret {
            self.completion.cancel();
            self.mode = InputMode::Prompt;
        }
    }

    pub fn set_voice_enabled(&mut self, enabled: bool) {
        self.voice.set_enabled(enabled);
    }

    #[must_use]
    pub const fn voice_phase(&self) -> VoicePhase {
        self.voice.phase()
    }

    #[must_use]
    pub const fn voice_generation(&self) -> u64 {
        self.voice.generation()
    }

    #[must_use]
    pub fn voice_indicator(&self) -> u8 {
        self.voice.indicator()
    }

    pub fn set_safety(&mut self, safety: Safety) {
        self.safety = safety;
    }

    #[must_use]
    pub const fn safety(&self) -> Safety {
        self.safety
    }

    pub fn set_agent_name(&mut self, name: &str) {
        if self.agent_name != name {
            self.agent_name.clear();
            self.agent_name.push_str(name);
        }
    }

    #[must_use]
    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }

    #[must_use]
    pub const fn switching(&self) -> bool {
        self.switching
    }

    #[must_use]
    pub const fn feedback_active(&self) -> bool {
        self.feedback_active
    }

    pub fn set_teleport_available(&mut self, available: bool) {
        self.completion.set_vibe_code_enabled(available);
        if !self.teleport_available() && self.mode == InputMode::Teleport {
            self.mode = InputMode::Prompt;
        }
    }

    pub fn set_command_context(&mut self, context: CommandContext) {
        self.completion.set_command_context(context);
        if !self.teleport_available() && self.mode == InputMode::Teleport {
            self.mode = InputMode::Prompt;
        }
    }

    #[must_use]
    pub fn command_context(&self) -> &CommandContext {
        self.completion.command_context()
    }

    pub fn set_user_skills<'a>(&mut self, skills: impl IntoIterator<Item = (&'a str, &'a str)>) {
        self.completion.set_user_skills(skills);
    }

    #[must_use]
    pub fn editor(&self) -> &PromptEditor {
        &self.editor
    }

    #[must_use]
    pub fn completion(&self) -> &CompletionEngine {
        &self.completion
    }

    #[must_use]
    pub fn mode(&self) -> InputMode {
        self.mode
    }

    pub(crate) fn poll_completion(&mut self) -> Vec<InputEffect> {
        let resolutions = self.completion.poll();
        let mut effects = Vec::new();
        for resolution in resolutions {
            effects.extend(self.apply(InputEvent::CompletionResolved { resolution }));
        }
        effects
    }

    pub(crate) fn dispatch_completion_request(
        &mut self,
        request: CompletionRequest,
        workspace: &Path,
    ) -> Result<Option<CompletionResolution>, InputError> {
        self.completion.dispatch_request(request, workspace)
    }

    pub(crate) fn refresh_after_adapter_mutation(&mut self) -> Vec<InputEffect> {
        let mut effects = Vec::new();
        self.refresh_completion(&mut effects);
        effects
    }

    pub(crate) fn insert_image_mention(&mut self, path: &Path) -> bool {
        if self.secret_input {
            return false;
        }
        let previous = self
            .editor
            .cursor()
            .checked_sub(1)
            .and_then(|index| self.editor.text().graphemes(true).nth(index));
        let prefix = if previous.is_none_or(|grapheme| grapheme.chars().all(char::is_whitespace)) {
            ""
        } else {
            " "
        };
        let path = path.to_string_lossy();
        let token = if path.contains(' ') {
            format!("{prefix}@'{path}' ")
        } else {
            format!("{prefix}@{path} ")
        };
        self.editor.insert(&token);
        true
    }

    fn editor_snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            text: self.editor.text().to_owned(),
            cursor: self.editor.cursor(),
            selection: self
                .editor
                .selection()
                .map(|selection| [selection.start, selection.end]),
        }
    }

    fn matches_snapshot(&self, snapshot: &EditorSnapshot) -> bool {
        self.editor_snapshot() == *snapshot
    }

    fn apply_normalized_text(&mut self, snapshot: &EditorSnapshot, normalized: String) {
        if !self.matches_snapshot(snapshot)
            || snapshot.cursor != snapshot.text.graphemes(true).count()
        {
            return;
        }
        if normalized == self.editor.text() {
            return;
        }
        self.replace_text(normalized);
    }

    pub fn replace_text(&mut self, text: impl Into<String>) {
        self.editor.set_text(text);
        self.mode = mode_from_text(self.editor.text(), self.teleport_available());
    }

    pub fn replace_history(&mut self, entries: Vec<String>) {
        self.editor.replace_history(entries);
    }

    #[must_use]
    pub fn history_entries(&self) -> &[String] {
        self.editor.history_entries()
    }

    pub fn set_viewport_width(&mut self, width: u16) {
        self.content_width = composer_content_width(width);
    }

    pub fn set_viewport(&mut self, width: u16, height: u16) {
        self.content_width = composer_content_width(width);
        self.viewport_height = height.max(1);
    }

    fn effective_content_width(&self) -> usize {
        self.composer_layout().width()
    }

    fn composer_layout(&self) -> ComposerLayout {
        ComposerLayout::for_content_width(
            &self.editor,
            self.content_width,
            self.viewport_height,
            self.mode.prefix_len(),
        )
    }

    /// Applies one normalized event and returns the ordered effects it causes.
    ///
    /// The transition never blocks and never panics: an event that cannot be
    /// honored yields [`InputEffect::Rejected`] and leaves the state valid.
    pub fn apply(&mut self, event: InputEvent) -> Vec<InputEffect> {
        let mut effects = Vec::new();
        match event {
            InputEvent::Key { key, char, mods } => {
                self.apply_key(key, char, &mods, &mut effects);
            }
            InputEvent::Paste { text } => self.apply_paste(&text, &mut effects),
            InputEvent::CompletionResolved { resolution } => {
                self.completion.apply_resolution(&self.editor, resolution);
            }
            InputEvent::ExternalEditor { text } => {
                // A cancelled edit leaves the prompt exactly as it was.
                if let Some(text) = text
                    && text != self.editor.text()
                {
                    self.editor.set_text(text);
                    self.history_navigating = false;
                    effects.push(InputEffect::HistoryReset);
                    self.refresh_completion(&mut effects);
                }
            }
            InputEvent::PasteNormalized { snapshot, text } => {
                if !self.matches_snapshot(&snapshot) {
                    return effects;
                }
                let Some(range) = self.last_paste.take() else {
                    effects.push(InputEffect::Rejected {
                        reason: "no paste to normalize".to_owned(),
                    });
                    return effects;
                };
                self.editor.select(range);
                self.editor.insert(&text);
                self.refresh_completion(&mut effects);
            }
            InputEvent::TextNormalized { snapshot, text } => {
                let before = self.editor.revision();
                self.apply_normalized_text(&snapshot, text);
                if self.editor.revision() != before {
                    self.refresh_completion(&mut effects);
                }
            }
            InputEvent::Transcript { text, generation } => {
                self.apply_voice_update(VoiceUpdate::Transcript { text, generation }, &mut effects);
            }
            InputEvent::VoiceTranscriptDelta { text, generation } => {
                self.apply_voice_update(VoiceUpdate::Delta { text, generation }, &mut effects);
            }
            InputEvent::VoiceDone { generation } => {
                self.apply_voice_update(VoiceUpdate::Done { generation }, &mut effects);
            }
            InputEvent::VoicePeak { generation, level } => {
                self.apply_voice_update(VoiceUpdate::Peak { generation, level }, &mut effects);
            }
            InputEvent::VoiceIndicatorTick => {
                self.apply_voice_update(VoiceUpdate::IndicatorTick, &mut effects);
            }
            InputEvent::VoiceStartResolved { generation, error } => {
                self.apply_voice_update(
                    VoiceUpdate::StartResolved { generation, error },
                    &mut effects,
                );
            }
            InputEvent::VoiceStopResolved { generation, error } => {
                self.apply_voice_update(
                    VoiceUpdate::StopResolved { generation, error },
                    &mut effects,
                );
            }
            InputEvent::Switching { active } => {
                self.switching = active;
                if active {
                    self.completion.cancel();
                }
            }
            InputEvent::Feedback { active } => self.feedback_active = active,
            InputEvent::Mouse {
                x,
                y,
                extend_selection,
            } => {
                let _ = self.editor.move_to_visual_cell(
                    usize::from(y),
                    usize::from(x),
                    self.effective_content_width(),
                    self.mode_prefix(),
                    extend_selection,
                );
            }
            InputEvent::Resize { width, height } => self.set_viewport(width, height),
            InputEvent::SafetyChanged { value } => self.safety = value,
        }
        effects
    }

    fn apply_key(
        &mut self,
        key: KeyName,
        character: Option<char>,
        mods: &[Modifier],
        effects: &mut Vec<InputEffect>,
    ) {
        let ctrl = mods.contains(&Modifier::Ctrl);
        let shift = mods.contains(&Modifier::Shift);
        let alt = mods.contains(&Modifier::Alt);
        let meta = mods.contains(&Modifier::Meta);

        if self.apply_voice_key(key, character, ctrl, effects) {
            return;
        }

        if self.feedback_active && key == KeyName::Char && !ctrl && !alt && !meta {
            match character {
                Some('1'..='3') => {
                    effects.push(InputEffect::FeedbackRating {
                        rating: match character {
                            Some('1') => 1,
                            Some('2') => 2,
                            Some('3') => 3,
                            _ => 0,
                        },
                    });
                    return;
                }
                Some('0') => {
                    effects.push(InputEffect::FeedbackSnooze);
                    return;
                }
                Some(_) => effects.push(InputEffect::FeedbackDismissed),
                None => {}
            }
        } else if self.feedback_active && key == KeyName::Escape {
            effects.push(InputEffect::FeedbackDismissed);
            return;
        }

        if key == KeyName::Char
            && character == Some('v')
            && (ctrl || meta)
            && self.command_context().is_available(CommandId::PasteImage)
        {
            effects.push(InputEffect::ClipboardImageRequested {
                notify_when_empty: true,
            });
            return;
        }

        if !self.secret_input
            && let Some(completion_key) = popup_key(key, mods)
        {
            let outcome = self.completion.handle_key(completion_key, &mut self.editor);
            match outcome {
                Ok(CompletionKeyOutcome::Consumed) => return,
                Ok(CompletionKeyOutcome::Submit) => {
                    self.submit(effects);
                    return;
                }
                Ok(CompletionKeyOutcome::Refresh) => {
                    self.refresh_completion(effects);
                    return;
                }
                Ok(CompletionKeyOutcome::Ignored) => {}
                Err(error) => {
                    effects.push(InputEffect::Rejected {
                        reason: error.to_string(),
                    });
                    return;
                }
            }
        }

        if key == KeyName::Backtab || (key == KeyName::Tab && shift) {
            self.reset_completion(effects);
            return;
        }

        if ctrl {
            let before = self.editor.revision();
            let mode_prefix = self.mode_prefix();
            self.reset_completion(effects);
            match character {
                Some('c') if !shift => {
                    self.editor.set_text("");
                    if self.mode != InputMode::Prompt {
                        self.mode = InputMode::Prompt;
                        effects.push(InputEffect::ModeChanged {
                            mode: InputMode::Prompt,
                        });
                    }
                }
                Some('d') => self.editor.delete_forward(),
                Some('g') => effects.push(InputEffect::OpenExternalEditor {
                    text: self.editor.text().to_owned(),
                }),
                Some('a') => self.editor.move_home_bounded(false, mode_prefix),
                Some('e') => self.editor.move_end(false),
                Some('j') => self.editor.insert("\n"),
                _ => {}
            }
            self.finish_user_edit(before, effects);
            return;
        }

        if key == KeyName::Backspace
            && self.mode != InputMode::Prompt
            && self.editor.text().graphemes(true).count() == 1
            && self.editor.cursor() == 1
        {
            self.editor.set_text("");
            self.completion.cancel();
            self.mode = InputMode::Prompt;
            effects.push(InputEffect::ModeChanged {
                mode: InputMode::Prompt,
            });
            return;
        }

        let before = self.editor.revision();
        let mode_prefix = self.mode_prefix();
        match key {
            KeyName::Escape => self.editor.set_text(""),
            KeyName::Enter if shift => self.editor.insert("\n"),
            KeyName::Enter => {
                self.submit(effects);
                return;
            }
            KeyName::Backspace => self.editor.delete_backward_bounded(mode_prefix),
            KeyName::Delete => self.editor.delete_forward(),
            KeyName::Left if alt => self.editor.move_word_left_bounded(mode_prefix),
            KeyName::Right if alt => self.editor.move_word_right(),
            KeyName::Left => self.editor.move_left_bounded(shift, mode_prefix),
            KeyName::Right => self.editor.move_right(shift),
            KeyName::Home => {
                self.editor
                    .move_visual_home(shift, self.effective_content_width(), mode_prefix)
            }
            KeyName::End => {
                self.editor
                    .move_visual_end(shift, self.effective_content_width(), mode_prefix)
            }
            KeyName::Up => {
                self.history_up(effects);
                return;
            }
            KeyName::Down => {
                self.history_down(effects);
                return;
            }
            KeyName::PageUp | KeyName::PageDown | KeyName::Backtab | KeyName::Tab => {}
            KeyName::Char => {
                let Some(character) = character else {
                    effects.push(InputEffect::Rejected {
                        reason: "character key without a character".to_owned(),
                    });
                    return;
                };
                if self.editor.text().is_empty()
                    && self.mode == InputMode::Prompt
                    && let Some(mode) = self.prefix_mode(character)
                {
                    self.editor.insert(&character.to_string());
                    self.mode = mode;
                    effects.push(InputEffect::ModeChanged { mode });
                    self.refresh_completion(effects);
                    return;
                }
                if !self.editor.insert_key_character(character) {
                    return;
                }
            }
        }
        self.finish_user_edit(before, effects);
        if self.editor.revision() != before {
            if key == KeyName::Char
                && !self.secret_input
                && !ctrl
                && !alt
                && !meta
                && self.editor.cursor_at_end()
                && self.editor.has_path_syntax()
            {
                effects.push(InputEffect::NormalizeCurrentText {
                    snapshot: self.editor_snapshot(),
                });
            }
            self.refresh_completion(effects);
        }
    }

    fn apply_voice_key(
        &mut self,
        key: KeyName,
        character: Option<char>,
        ctrl: bool,
        effects: &mut Vec<InputEffect>,
    ) -> bool {
        let outcome = self.voice.handle_key(
            key == KeyName::Char && character == Some('r') && ctrl,
            key == KeyName::Char && character == Some('c') && ctrl,
        );
        if let Some(command) = outcome.command {
            effects.push(match command {
                VoiceCommand::Start => InputEffect::RecordingStartRequested,
                VoiceCommand::Stop => InputEffect::RecordingStopRequested,
                VoiceCommand::Cancel => InputEffect::RecordingCancelRequested,
            });
        }
        outcome.consumed
    }

    fn apply_voice_update(&mut self, update: VoiceUpdate, effects: &mut Vec<InputEffect>) {
        match self.voice.apply(update) {
            VoiceUpdateOutcome::None => {}
            VoiceUpdateOutcome::Insert(text) => {
                self.editor.insert(&text);
                self.history_navigating = false;
                effects.push(InputEffect::HistoryReset);
                self.refresh_completion(effects);
            }
            VoiceUpdateOutcome::Notify(message) => effects.push(InputEffect::Notify {
                message,
                severity: Severity::Warning,
            }),
            VoiceUpdateOutcome::Rejected(reason) => effects.push(InputEffect::Rejected {
                reason: reason.to_owned(),
            }),
        }
    }

    fn apply_paste(&mut self, text: &str, effects: &mut Vec<InputEffect>) {
        if text.trim().is_empty() {
            effects.push(InputEffect::ClipboardImageRequested {
                notify_when_empty: false,
            });
            if text.is_empty() {
                return;
            }
        }
        let before = self.editor.revision();
        let start = self.editor.cursor();
        self.editor.paste(text);
        self.finish_user_edit(before, effects);
        if is_path_candidate(text) {
            self.last_paste = Some(start..self.editor.cursor());
            effects.push(InputEffect::NormalizePastedPath {
                text: text.to_owned(),
                snapshot: self.editor_snapshot(),
            });
        } else {
            self.last_paste = None;
        }
        self.refresh_completion(effects);
    }

    fn submit(&mut self, effects: &mut Vec<InputEffect>) {
        let stripped = self.editor.text().trim().to_owned();
        effects.push(InputEffect::SubmitRequested {
            text: stripped.clone(),
        });
        if self.switching {
            return;
        }
        if stripped.is_empty() {
            effects.push(InputEffect::Submit { text: stripped });
            return;
        }
        let submitted = self.editor.submit();
        self.completion.cancel();
        if self.mode != InputMode::Prompt {
            self.mode = InputMode::Prompt;
            effects.push(InputEffect::ModeChanged {
                mode: InputMode::Prompt,
            });
        }
        effects.push(InputEffect::CompletionReset);
        if let Some(entry) = submitted {
            effects.push(InputEffect::RecordHistory {
                entry: entry.clone(),
            });
            effects.push(InputEffect::Submit { text: entry });
        }
    }

    /// Invalidates any in-flight request and reports a visible popup closing.
    ///
    /// The generation always moves so a late answer is rejected; the effect is
    /// only emitted when something the user could see actually went away.
    fn reset_completion(&mut self, effects: &mut Vec<InputEffect>) {
        let was_open = self.completion.view().is_some();
        self.completion.cancel();
        if was_open {
            effects.push(InputEffect::CompletionReset);
        }
    }

    fn refresh_completion(&mut self, effects: &mut Vec<InputEffect>) {
        self.completion.cancel();
        if self.secret_input {
            return;
        }
        let Some((range, query)) = active_token(&self.editor) else {
            return;
        };
        effects.push(InputEffect::RequestCompletion {
            request: CompletionRequest::new(self.completion.generation(), range, query),
        });
    }

    fn finish_user_edit(&mut self, before: u64, effects: &mut Vec<InputEffect>) {
        if self.editor.revision() == before {
            return;
        }
        self.history_navigating = false;
        effects.push(InputEffect::HistoryReset);
    }

    fn sync_mode(&mut self, effects: &mut Vec<InputEffect>) {
        let mode = mode_from_text(self.editor.text(), self.teleport_available());
        if mode != self.mode {
            self.mode = mode;
            effects.push(InputEffect::ModeChanged { mode });
        }
    }

    fn prefix_mode(&self, character: char) -> Option<InputMode> {
        match character {
            '!' => Some(InputMode::Shell),
            '/' => Some(InputMode::Command),
            '&' if self.teleport_available() => Some(InputMode::Teleport),
            _ => None,
        }
    }

    fn teleport_available(&self) -> bool {
        self.command_context().is_available(CommandId::Teleport)
    }

    fn mode_prefix(&self) -> usize {
        self.mode.prefix_len()
    }

    fn history_up(&mut self, effects: &mut Vec<InputEffect>) {
        let mode_prefix = self.mode_prefix();
        let loaded_unmoved =
            self.editor.history_loaded() && !self.editor.cursor_moved_since_history_load();
        if !loaded_unmoved
            && self
                .editor
                .move_visual_up(self.effective_content_width(), mode_prefix)
        {
            return;
        }
        if !loaded_unmoved && self.editor.cursor() != mode_prefix {
            self.editor.move_home_bounded(false, mode_prefix);
            return;
        }
        effects.push(InputEffect::HistoryPrevious);
        if self.editor.history_previous() {
            self.history_navigating = false;
            self.sync_mode(effects);
            self.completion.cancel();
            effects.push(InputEffect::CompletionReset);
        } else {
            self.history_navigating = true;
        }
    }

    fn history_down(&mut self, effects: &mut Vec<InputEffect>) {
        let mode_prefix = self.mode_prefix();
        let loaded_unmoved =
            self.editor.history_loaded() && !self.editor.cursor_moved_since_history_load();
        if !loaded_unmoved
            && self
                .editor
                .move_visual_down(self.effective_content_width(), mode_prefix)
        {
            return;
        }
        if !self.editor.history_loaded() {
            return;
        }
        effects.push(InputEffect::HistoryNext);
        if self.editor.history_next() {
            self.history_navigating = false;
            self.sync_mode(effects);
            self.completion.cancel();
            effects.push(InputEffect::CompletionReset);
        } else {
            self.history_navigating = true;
        }
    }
}

/// Maps a normalized key onto the popup vocabulary, honoring modifier rules.
fn popup_key(key: KeyName, mods: &[Modifier]) -> Option<CompletionKey> {
    match key {
        KeyName::Escape => Some(CompletionKey::Escape),
        KeyName::Up if mods.is_empty() => Some(CompletionKey::Up),
        KeyName::Down if mods.is_empty() => Some(CompletionKey::Down),
        KeyName::Tab if mods.is_empty() => Some(CompletionKey::Tab),
        KeyName::Enter if mods.is_empty() => Some(CompletionKey::Enter),
        _ => None,
    }
}

fn candidate_kind(candidates: &[CompletionCandidate]) -> String {
    let kind = candidates
        .first()
        .map_or(CompletionKind::Path, |candidate| candidate.kind);
    match kind {
        CompletionKind::SlashCommand | CompletionKind::Skill => "slash".to_owned(),
        _ => "path".to_owned(),
    }
}

fn mode_from_text(text: &str, teleport_available: bool) -> InputMode {
    match text.chars().next() {
        Some('!') => InputMode::Shell,
        Some('/') => InputMode::Command,
        Some('&') if teleport_available => InputMode::Teleport,
        _ => InputMode::Prompt,
    }
}

/// Converts a grapheme index into the character index the reference reports.
fn char_offset(text: &str, graphemes: usize) -> usize {
    text.graphemes(true)
        .take(graphemes)
        .map(|grapheme| grapheme.chars().count())
        .sum()
}

/// Cheap syntactic test used to decide whether a paste may name a file.
///
/// The decision is deliberately free of filesystem access: resolving the path
/// is the adapter's job, requested through [`InputEffect::NormalizePastedPath`].
fn is_path_candidate(text: &str) -> bool {
    text.contains(['/', '~', '\'', '"'])
}

#[cfg(test)]
#[path = "chat_input/tests.rs"]
mod tests;
