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

use serde::{Deserialize, Serialize};

use super::input::{
    CompletionCandidate, CompletionEngine, CompletionKey, CompletionKeyOutcome, CompletionKind,
    InputError, PromptEditor, active_token, completion_description,
};

/// Composer input mode, mirroring the reference prefix characters.
///
/// Only [`InputMode::Prompt`] is modelled today; US-004 introduces the prefix
/// modes, so the corpus reports the others as divergences rather than having
/// this boundary pretend to support them.
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
#[serde(tag = "type", rename_all = "camelCase")]
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

/// Prompt-history position.
///
/// Only what the composer actually tracks is reported. The reference also
/// records a recalled-entry marker and post-recall cursor movement; US-007
/// adds that state, and until then the runner declares those fields
/// unmodelled rather than having this boundary answer for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryObservation {
    pub navigating: bool,
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

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Everything the composer owns between two events.
#[derive(Debug, Default)]
pub struct ChatInputState {
    editor: PromptEditor,
    completion: CompletionEngine,
    feedback_active: bool,
    switching: bool,
    secret_input: bool,
    last_paste: Option<Range<usize>>,
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
        }
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
                if let Some(text) = text {
                    self.editor.set_text(text);
                    self.reset_completion(&mut effects);
                    effects.push(InputEffect::HistoryReset);
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
            // Accepted so a trace stays replayable, but not modelled yet:
            // viewport geometry is US-018 and safety chrome is US-017. Neither
            // reaches an observation, so storing them would be theatre.
            InputEvent::Resize { .. } | InputEvent::SafetyChanged { .. } => {}
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
            self.reset_completion(effects);
            match character {
                Some('c') if !shift => self.editor.set_text(""),
                Some('d') => self.editor.delete_forward(),
                Some('g') => effects.push(InputEffect::OpenExternalEditor {
                    text: self.editor.text().to_owned(),
                }),
                Some('a') => self.editor.move_home(false),
                Some('e') => self.editor.move_end(false),
                Some('j') => self.editor.insert("\n"),
                _ => {}
            }
            return;
        }

        match key {
            KeyName::Escape => self.editor.set_text(""),
            KeyName::Enter if shift => self.editor.insert("\n"),
            KeyName::Enter => {
                self.submit(effects);
                return;
            }
            KeyName::Backspace => self.editor.delete_backward(),
            KeyName::Delete => self.editor.delete_forward(),
            KeyName::Left => self.editor.move_left(shift),
            KeyName::Right => self.editor.move_right(shift),
            KeyName::Home => self.editor.move_home(shift),
            KeyName::End => self.editor.move_end(shift),
            KeyName::Up => {
                self.editor.history_previous();
                effects.push(InputEffect::HistoryPrevious);
            }
            KeyName::Down => {
                self.editor.history_next();
                effects.push(InputEffect::HistoryNext);
            }
            KeyName::PageUp | KeyName::PageDown | KeyName::Backtab | KeyName::Tab => {}
            KeyName::Char => {
                let Some(character) = character else {
                    effects.push(InputEffect::Rejected {
                        reason: "character key without a character".to_owned(),
                    });
                    return;
                };
                self.editor.insert(&character.to_string());
            }
        }
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
        effects.push(InputEffect::SubmitRequested {
            text: self.editor.text().to_owned(),
        });
        if self.switching {
            return;
        }
        let submitted = self.editor.submit();
        self.reset_completion(effects);
        // An all-whitespace prompt requests a submission but emits no turn.
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
        self.reset_completion(effects);
        if self.secret_input {
            return;
        }
        let Some((range, query)) = active_token(&self.editor) else {
            return;
        };
        effects.push(InputEffect::RequestCompletion {
            generation: self.completion.generation(),
            query,
            range: [range.start, range.end],
        });
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
            // US-004 introduces the prefix modes; until then the composer is
            // always in the default mode and the corpus records the gap.
            mode: InputMode::Prompt,
            cursor: char_offset(self.editor.text(), self.editor.cursor()),
            selection: self.editor.selection().map(|range| {
                [
                    char_offset(self.editor.text(), range.start),
                    char_offset(self.editor.text(), range.end),
                ]
            }),
            completion,
            history: HistoryObservation {
                navigating: self.editor.history_navigating(),
            },
            feedback_active: self.feedback_active,
            switching: self.switching,
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

/// Converts a grapheme index into the character index the reference reports.
fn char_offset(text: &str, graphemes: usize) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
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
mod tests {
    use super::*;

    fn key(name: KeyName) -> InputEvent {
        InputEvent::Key {
            key: name,
            char: None,
            mods: Vec::new(),
        }
    }

    fn character(value: char) -> InputEvent {
        InputEvent::Key {
            key: KeyName::Char,
            char: Some(value),
            mods: Vec::new(),
        }
    }

    fn type_text(state: &mut ChatInputState, value: &str) -> Vec<InputEffect> {
        value
            .chars()
            .flat_map(|character_value| state.apply(character(character_value)))
            .collect()
    }

    /// Types `value`, then answers the completion request it produced.
    fn complete(
        state: &mut ChatInputState,
        value: &str,
        candidates: Vec<CompletionCandidate>,
    ) -> Vec<InputEffect> {
        let generation = type_text(state, value)
            .into_iter()
            .rev()
            .find_map(|effect| match effect {
                InputEffect::RequestCompletion { generation, .. } => Some(generation),
                _ => None,
            })
            .expect("typing a token requests a completion");
        state.apply(InputEvent::CompletionResults {
            generation,
            candidates,
        })
    }

    fn mention(label: &str) -> CompletionCandidate {
        CompletionCandidate {
            id: format!("mention:{label}"),
            kind: CompletionKind::Mention,
            label: label.to_owned(),
            insertion: label.to_owned(),
        }
    }

    #[test]
    fn transitions_are_deterministic_and_serializable() {
        let mut left = ChatInputState::new();
        let mut right = ChatInputState::new();
        for character_value in "abc".chars() {
            assert_eq!(
                left.apply(character(character_value)),
                right.apply(character(character_value))
            );
        }
        assert_eq!(left.observe(), right.observe());
        let encoded = serde_json::to_string(&left.observe()).expect("observation serializes");
        let decoded: StateObservation =
            serde_json::from_str(&encoded).expect("observation round-trips");
        assert_eq!(decoded, left.observe());
    }

    #[test]
    fn filesystem_work_is_requested_as_an_effect() {
        let mut state = ChatInputState::new();
        let effects = type_text(&mut state, "@src");
        assert!(effects.iter().any(|effect| matches!(
            effect,
            InputEffect::RequestCompletion { query, .. } if query == "@src"
        )));
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, InputEffect::Submit { .. }))
        );
    }

    #[test]
    fn external_editor_and_clipboard_leave_the_boundary_as_effects() {
        let mut state = ChatInputState::new();
        type_text(&mut state, "draft");
        let effects = state.apply(InputEvent::Key {
            key: KeyName::Char,
            char: Some('g'),
            mods: vec![Modifier::Ctrl],
        });
        assert!(effects.iter().any(|effect| matches!(
            effect,
            InputEffect::OpenExternalEditor { text } if text == "draft"
        )));
        let effects = state.apply(InputEvent::Paste {
            text: String::new(),
        });
        assert_eq!(
            effects,
            vec![InputEffect::ClipboardImageRequested {
                notify_when_empty: false
            }]
        );
    }

    #[test]
    fn a_cancelled_external_edit_keeps_the_prompt() {
        let mut state = ChatInputState::new();
        type_text(&mut state, "draft");
        let effects = state.apply(InputEvent::ExternalEditor { text: None });
        assert_eq!(effects, Vec::new());
        assert_eq!(state.observe().text, "draft");
    }

    #[test]
    fn stale_completion_results_are_rejected_without_panicking() {
        let mut state = ChatInputState::new();
        type_text(&mut state, "@s");
        let effects = state.apply(InputEvent::CompletionResults {
            generation: 0,
            candidates: vec![mention("@src/")],
        });
        assert_eq!(
            effects,
            vec![InputEffect::Rejected {
                reason: "stale completion generation".to_owned()
            }]
        );
        assert!(!state.observe().completion.open);
    }

    #[test]
    fn invalid_events_keep_state_valid() {
        let mut state = ChatInputState::new();
        let effects = state.apply(InputEvent::Key {
            key: KeyName::Char,
            char: None,
            mods: Vec::new(),
        });
        assert_eq!(
            effects,
            vec![InputEffect::Rejected {
                reason: "character key without a character".to_owned()
            }]
        );
        let effects = state.apply(InputEvent::PasteNormalized {
            text: "@image.png".to_owned(),
        });
        assert_eq!(
            effects,
            vec![InputEffect::Rejected {
                reason: "no paste to normalize".to_owned()
            }]
        );
        assert_eq!(state.observe().text, "");
        assert_eq!(state.observe().cursor, 0);
    }

    #[test]
    fn completion_selection_and_acceptance_stay_in_bounds() {
        let mut state = ChatInputState::new();
        complete(
            &mut state,
            "@sr",
            vec![mention("@src/"), mention("@srv.rs")],
        );
        let observation = state.observe();
        assert!(observation.completion.open);
        assert_eq!(observation.completion.items.len(), 2);
        assert_eq!(observation.completion.selected, 0);

        state.apply(key(KeyName::Up));
        assert_eq!(state.observe().completion.selected, 1);
        state.apply(key(KeyName::Down));
        assert_eq!(state.observe().completion.selected, 0);

        let accepted = state.observe().completion.items[0].label.clone();
        state.apply(key(KeyName::Tab));
        assert_eq!(state.observe().text, accepted);
        assert!(!state.observe().completion.open);
    }

    #[test]
    fn navigation_inside_the_popup_does_not_report_a_reset() {
        let mut state = ChatInputState::new();
        complete(
            &mut state,
            "@sr",
            vec![mention("@src/"), mention("@srv.rs")],
        );
        assert_eq!(state.apply(key(KeyName::Down)), Vec::new());
        assert!(state.observe().completion.open);
        assert_eq!(
            state.apply(key(KeyName::Escape)),
            vec![InputEffect::CompletionReset]
        );
    }

    #[test]
    fn a_closed_popup_never_reports_a_reset() {
        let mut state = ChatInputState::new();
        let effects = type_text(&mut state, "plain");
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, InputEffect::CompletionReset)),
            "{effects:?}"
        );
    }

    #[test]
    fn submission_reports_the_payload_and_history_entry() {
        let mut state = ChatInputState::new();
        type_text(&mut state, " spaced ");
        let effects = state.apply(key(KeyName::Enter));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            InputEffect::SubmitRequested { text } if text == " spaced "
        )));
        // GAP-02: the reference strips the submitted text. US-004 closes this,
        // and the assertion below changes with it.
        assert!(effects.iter().any(|effect| matches!(
            effect,
            InputEffect::RecordHistory { entry } if entry == " spaced "
        )));
        assert_eq!(state.observe().text, "");
    }

    #[test]
    fn an_empty_submission_emits_no_turn() {
        let mut state = ChatInputState::new();
        type_text(&mut state, "   ");
        let effects = state.apply(key(KeyName::Enter));
        assert_eq!(
            effects,
            vec![InputEffect::SubmitRequested {
                text: "   ".to_owned()
            }]
        );
    }

    #[test]
    fn switching_blocks_submission_and_keeps_the_prompt() {
        let mut state = ChatInputState::new();
        type_text(&mut state, "queued");
        state.apply(InputEvent::Switching { active: true });
        let effects = state.apply(key(KeyName::Enter));
        assert_eq!(
            effects,
            vec![InputEffect::SubmitRequested {
                text: "queued".to_owned()
            }]
        );
        assert_eq!(state.observe().text, "queued");
    }

    #[test]
    fn history_navigation_is_observed_from_the_editor() {
        let mut state = ChatInputState::new();
        type_text(&mut state, "first");
        state.apply(key(KeyName::Enter));
        assert!(!state.observe().history.navigating);
        state.apply(key(KeyName::Up));
        assert!(state.observe().history.navigating);
        assert_eq!(state.observe().text, "first");
        state.apply(key(KeyName::Down));
        assert!(!state.observe().history.navigating);
    }

    #[test]
    fn secret_input_closes_and_suppresses_completion() {
        let mut state = ChatInputState::new();
        complete(&mut state, "@sr", vec![mention("@src/")]);
        assert!(state.observe().completion.open);
        state.set_secret_input(true);
        assert!(!state.observe().completion.open);
        let effects = type_text(&mut state, "c");
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, InputEffect::RequestCompletion { .. })),
            "{effects:?}"
        );
    }
}
