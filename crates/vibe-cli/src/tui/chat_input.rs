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

use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

use super::input::{
    CompletionCandidate, CompletionEngine, CompletionKey, CompletionKeyOutcome, CompletionKind,
    InputError, PromptEditor, active_token, completion_description, composer_content_width,
    visual_cursor_cell, visual_text_lines,
};

/// Composer input mode, mirroring the reference prefix characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum InputMode {
    #[default]
    #[serde(rename = ">")]
    Prompt,
    #[serde(rename = "!")]
    Shell,
    #[serde(rename = "/")]
    Command,
    #[serde(rename = "&")]
    Teleport,
}

impl InputMode {
    #[must_use]
    pub const fn symbol(self) -> char {
        match self {
            Self::Prompt => '>',
            Self::Shell => '!',
            Self::Command => '/',
            Self::Teleport => '&',
        }
    }

    #[must_use]
    pub const fn prefix_len(self) -> usize {
        match self {
            Self::Prompt => 0,
            Self::Shell | Self::Command | Self::Teleport => 1,
        }
    }
}

/// Normalised key identity shared by the oracle traces and the terminal adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyName {
    Char,
    Enter,
    Backspace,
    Delete,
    Tab,
    Backtab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    #[serde(rename = "pageup")]
    PageUp,
    #[serde(rename = "pagedown")]
    PageDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modifier {
    Shift,
    Ctrl,
    Alt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Safety {
    Neutral,
    Safe,
    Destructive,
    Yolo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Information,
    Warning,
    Error,
}

/// Every input the composer can observe, including responses to its effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum InputEvent {
    Key {
        key: KeyName,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        char: Option<char>,
        #[serde(default)]
        mods: Vec<Modifier>,
    },
    Paste {
        text: String,
    },
    Resize {
        width: u16,
        height: u16,
    },
    Mouse {
        x: u16,
        y: u16,
        #[serde(default)]
        extend_selection: bool,
    },
    /// Raw candidates produced for a completion request.
    ///
    /// Ranking, the mention cap and the empty-result rule are applied by
    /// [`CompletionEngine`], so an adapter only has to perform the lookup.
    CompletionResults {
        generation: u64,
        candidates: Vec<CompletionCandidate>,
    },
    /// A completion request that could not be served.
    CompletionFailed {
        generation: u64,
        reason: String,
    },
    /// Result of the external editor effect; `None` when it was cancelled.
    ExternalEditor {
        #[serde(default)]
        text: Option<String>,
    },
    /// Normalised replacement for the most recent paste.
    PasteNormalized {
        text: String,
    },
    Transcript {
        text: String,
    },
    Switching {
        active: bool,
    },
    Feedback {
        active: bool,
    },
    SafetyChanged {
        value: Safety,
    },
}

/// Work the composer delegates, and decisions it exposes to the application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum InputEffect {
    SubmitRequested {
        text: String,
    },
    Submit {
        text: String,
    },
    ModeChanged {
        mode: InputMode,
    },
    HistoryPrevious,
    HistoryNext,
    HistoryReset,
    CompletionReset,
    RequestCompletion {
        generation: u64,
        query: String,
        range: [usize; 2],
    },
    NormalizePastedPath {
        text: String,
    },
    OpenExternalEditor {
        text: String,
    },
    ClipboardImageRequested {
        notify_when_empty: bool,
    },
    RecordHistory {
        entry: String,
    },
    FeedbackRating {
        rating: u8,
    },
    FeedbackSnooze,
    FeedbackDismissed,
    RecordingStartRequested,
    RecordingStopRequested,
    RecordingCancelRequested,
    Notify {
        message: String,
        severity: Severity,
    },
    /// An event the boundary refused; recorded instead of panicking.
    Rejected {
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Observations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionItemObservation {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionObservation {
    pub open: bool,
    pub kind: Option<String>,
    pub selected: usize,
    pub items: Vec<CompletionItemObservation>,
}

/// Prompt-history position exposed to differential fixtures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryObservation {
    pub navigating: bool,
    pub loaded_entry: bool,
    pub cursor_moved_since_load: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateObservation {
    pub text: String,
    pub mode: InputMode,
    pub cursor: usize,
    pub selection: Option<[usize; 2]>,
    pub completion: CompletionObservation,
    pub history: HistoryObservation,
    pub feedback_active: bool,
    pub switching: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderObservation {
    pub cursor_cell: [usize; 2],
    pub prompt: char,
    pub visual_lines: Vec<String>,
    pub wrap_width: usize,
}

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
    secret_input: bool,
    history_navigating: bool,
    teleport_available: bool,
    content_width: usize,
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
            secret_input: false,
            history_navigating: false,
            teleport_available: false,
            content_width: 71,
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

    pub fn set_teleport_available(&mut self, available: bool) {
        self.teleport_available = available;
        if !available && self.mode == InputMode::Teleport {
            self.mode = InputMode::Prompt;
        }
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

    pub(crate) fn poll_completion(&mut self) -> Result<bool, InputError> {
        self.completion.poll()
    }

    pub(crate) fn dispatch_completion_request(
        &mut self,
        generation: u64,
        query: String,
        range: [usize; 2],
        workspace: &Path,
    ) -> Result<(), InputError> {
        self.completion
            .dispatch_request(generation, range[0]..range[1], query, workspace)
    }

    pub(crate) fn refresh_after_adapter_mutation(&mut self) -> Vec<InputEffect> {
        let mut effects = Vec::new();
        self.refresh_completion(&mut effects);
        effects
    }

    pub(crate) fn insert_adapter_text(&mut self, text: &str) {
        self.editor.insert(text);
    }

    pub(crate) fn take_unrecorded(&mut self) -> Option<String> {
        self.completion.cancel();
        self.mode = InputMode::Prompt;
        self.editor.take_unrecorded()
    }

    pub fn replace_text(&mut self, text: impl Into<String>) {
        self.editor.set_text(text);
        self.mode = mode_from_text(self.editor.text(), self.teleport_available);
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

    pub(crate) fn set_content_width(&mut self, width: usize) {
        self.content_width = width.max(1);
    }

    /// Applies one normalised event and returns the ordered effects it causes.
    ///
    /// The transition never blocks and never panics: an event that cannot be
    /// honoured yields [`InputEffect::Rejected`] and leaves the state valid.
    pub fn apply(&mut self, event: InputEvent) -> Vec<InputEffect> {
        let mut effects = Vec::new();
        match event {
            InputEvent::Key { key, char, mods } => {
                self.apply_key(key, char, &mods, &mut effects);
            }
            InputEvent::Paste { text } => self.apply_paste(&text, &mut effects),
            InputEvent::CompletionResults {
                generation,
                candidates,
            } => self.apply_completion_results(generation, candidates, &mut effects),
            InputEvent::CompletionFailed { generation, reason } => {
                if generation == self.completion.generation() {
                    self.reset_completion(&mut effects);
                    effects.push(InputEffect::Notify {
                        message: format!("Path completion is unavailable: {reason}"),
                        severity: Severity::Warning,
                    });
                } else {
                    effects.push(InputEffect::Rejected {
                        reason: "stale completion generation".to_owned(),
                    });
                }
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
            InputEvent::PasteNormalized { text } => {
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
            InputEvent::Transcript { text } => {
                self.editor.insert(&text);
                self.refresh_completion(&mut effects);
            }
            InputEvent::Switching { active } => self.switching = active,
            InputEvent::Feedback { active } => self.feedback_active = active,
            InputEvent::Mouse {
                x,
                y,
                extend_selection,
            } => {
                let _ = self.editor.move_to_visual_cell(
                    usize::from(y),
                    usize::from(x),
                    self.content_width,
                    self.mode_prefix(),
                    extend_selection,
                );
            }
            // Accepted so a trace stays replayable, but not modelled yet:
            // viewport geometry is US-018 and safety chrome is US-017. Neither
            // reaches an observation, so storing them would be theatre.
            InputEvent::Resize { width, .. } => self.set_viewport_width(width),
            InputEvent::SafetyChanged { .. } => {}
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

        if !self.secret_input
            && let Some(completion_key) = popup_key(key, mods)
        {
            let was_open = self.completion.view().is_some();
            let outcome = self.completion.handle_key(completion_key, &mut self.editor);
            if was_open && self.completion.view().is_none() {
                effects.push(InputEffect::CompletionReset);
            }
            match outcome {
                Ok(CompletionKeyOutcome::Consumed) => return,
                Ok(CompletionKeyOutcome::Submit) => {
                    self.submit(effects);
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
            let before = self.editor.text().to_owned();
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
            self.finish_user_edit(&before, effects);
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

        let before = self.editor.text().to_owned();
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
            KeyName::Home => self.editor.move_home_bounded(shift, mode_prefix),
            KeyName::End => self.editor.move_end(shift),
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
        self.finish_user_edit(&before, effects);
        self.refresh_completion(effects);
    }

    fn apply_paste(&mut self, text: &str, effects: &mut Vec<InputEffect>) {
        if text.trim().is_empty() {
            effects.push(InputEffect::ClipboardImageRequested {
                notify_when_empty: false,
            });
            return;
        }
        let start = self.editor.cursor();
        if let Err(error) = self.editor.paste(text) {
            effects.push(InputEffect::Notify {
                message: error.to_string(),
                severity: Severity::Warning,
            });
            debug_assert!(matches!(error, InputError::PasteTooLarge { .. }));
            return;
        }
        self.last_paste = Some(start..self.editor.cursor());
        if is_path_candidate(text) {
            effects.push(InputEffect::NormalizePastedPath {
                text: text.to_owned(),
            });
        }
        self.refresh_completion(effects);
    }

    fn apply_completion_results(
        &mut self,
        generation: u64,
        candidates: Vec<CompletionCandidate>,
        effects: &mut Vec<InputEffect>,
    ) {
        if generation != self.completion.generation() {
            effects.push(InputEffect::Rejected {
                reason: "stale completion generation".to_owned(),
            });
            return;
        }
        // The generation matches, so the prompt has not changed since the
        // request and the token under the cursor is still the one queried.
        let Some((token_range, query)) = active_token(&self.editor) else {
            self.reset_completion(effects);
            return;
        };
        self.completion
            .install(generation, token_range, &query, candidates);
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
        let was_open = self.completion.view().is_some();
        self.completion.cancel();
        if self.secret_input {
            if was_open {
                effects.push(InputEffect::CompletionReset);
            }
            return;
        }
        let Some((range, query)) = active_token(&self.editor) else {
            if was_open {
                effects.push(InputEffect::CompletionReset);
            }
            return;
        };
        effects.push(InputEffect::RequestCompletion {
            generation: self.completion.generation(),
            query,
            range: [range.start, range.end],
        });
    }

    fn finish_user_edit(&mut self, before: &str, effects: &mut Vec<InputEffect>) {
        if self.editor.text() == before {
            return;
        }
        self.history_navigating = false;
        effects.push(InputEffect::HistoryReset);
    }

    fn sync_mode(&mut self, effects: &mut Vec<InputEffect>) {
        let mode = mode_from_text(self.editor.text(), self.teleport_available);
        if mode != self.mode {
            self.mode = mode;
            effects.push(InputEffect::ModeChanged { mode });
        }
    }

    fn prefix_mode(&self, character: char) -> Option<InputMode> {
        match character {
            '!' => Some(InputMode::Shell),
            '/' => Some(InputMode::Command),
            '&' if self.teleport_available => Some(InputMode::Teleport),
            _ => None,
        }
    }

    fn mode_prefix(&self) -> usize {
        self.mode.prefix_len()
    }

    fn history_up(&mut self, effects: &mut Vec<InputEffect>) {
        let mode_prefix = self.mode_prefix();
        let loaded_unmoved =
            self.editor.history_loaded() && !self.editor.cursor_moved_since_history_load();
        if !loaded_unmoved && self.editor.move_visual_up(self.content_width, mode_prefix) {
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
                .move_visual_down(self.content_width, mode_prefix)
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

    /// Projects the state onto the schema shared with the reference traces.
    #[must_use]
    pub fn observe(&self) -> StateObservation {
        let completion = match self.completion.view() {
            Some(view) => CompletionObservation {
                open: true,
                kind: Some(candidate_kind(view.candidates)),
                selected: view.selected,
                items: view
                    .candidates
                    .iter()
                    .map(|candidate| CompletionItemObservation {
                        label: candidate.label.clone(),
                        description: completion_description(&candidate.label).to_owned(),
                    })
                    .collect(),
            },
            None => CompletionObservation {
                open: false,
                kind: None,
                selected: 0,
                items: Vec::new(),
            },
        };
        StateObservation {
            text: self.editor.text().to_owned(),
            mode: self.mode,
            cursor: char_offset(self.editor.text(), self.editor.cursor()),
            selection: self.editor.selection().map(|range| {
                [
                    char_offset(self.editor.text(), range.start),
                    char_offset(self.editor.text(), range.end),
                ]
            }),
            completion,
            history: HistoryObservation {
                navigating: self.history_navigating,
                loaded_entry: self.editor.history_loaded(),
                cursor_moved_since_load: self.editor.cursor_moved_since_history_load(),
            },
            feedback_active: self.feedback_active,
            switching: self.switching,
        }
    }

    /// Projects the same composer geometry used by production rendering.
    #[must_use]
    pub fn observe_render(&self) -> RenderObservation {
        let prefix = self.mode.prefix_len();
        let (row, column) = visual_cursor_cell(
            self.editor.text(),
            self.editor.cursor(),
            self.content_width,
            prefix,
        );
        RenderObservation {
            cursor_cell: [row, column],
            prompt: self.mode.symbol(),
            visual_lines: visual_text_lines(self.editor.text(), self.content_width, prefix),
            wrap_width: self.content_width,
        }
    }
}

/// Maps a normalised key onto the popup vocabulary, honouring modifier rules.
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
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains(['\n', '\r']) || trimmed.starts_with('@') {
        return false;
    }
    let unquoted = trimmed
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
        .or_else(|| {
            trimmed
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
        })
        .unwrap_or(trimmed);
    unquoted.starts_with('/') || unquoted.starts_with('~')
}

#[cfg(test)]
#[path = "chat_input/tests.rs"]
mod tests;
