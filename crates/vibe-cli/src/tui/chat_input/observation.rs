use serde::{Deserialize, Serialize};

use super::{ChatInputState, InputMode, Safety, candidate_kind, char_offset};

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
    pub border_classes: Vec<String>,
    pub border_title: String,
    pub cursor_cell: [usize; 2],
    pub popup_rows: Vec<String>,
    pub popup_visible: bool,
    pub prompt: Option<char>,
    pub visual_lines: Vec<String>,
    pub wrap_width: usize,
}

impl ChatInputState {
    /// Projects state onto the schema shared with the reference traces.
    #[must_use]
    pub fn observe(&self) -> StateObservation {
        let completion = self.completion.view().map_or_else(
            || CompletionObservation {
                open: false,
                kind: None,
                selected: 0,
                items: Vec::new(),
            },
            |view| CompletionObservation {
                open: true,
                kind: Some(candidate_kind(view.candidates)),
                selected: view.selected,
                items: view
                    .candidates
                    .iter()
                    .map(|candidate| CompletionItemObservation {
                        label: candidate.label.clone(),
                        description: candidate.description.clone(),
                    })
                    .collect(),
            },
        );
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

    /// Projects the normalized composer render contract recorded by the oracle.
    #[must_use]
    pub fn observe_render(&self) -> RenderObservation {
        let layout = self.composer_layout();
        let mut border_classes = match self.safety {
            Safety::Neutral => Vec::new(),
            Safety::Safe => vec!["border-safe".to_owned()],
            Safety::Destructive => vec!["border-warning".to_owned()],
            Safety::Yolo => vec!["border-error".to_owned()],
        };
        if self.voice.phase().is_active() {
            border_classes = vec!["border-recording".to_owned()];
        }
        let presentation_hidden = self.switching || self.voice.phase().is_active();
        RenderObservation {
            border_classes,
            border_title: if self.agent_name.is_empty() {
                String::new()
            } else {
                format!(" {} ", self.agent_name)
            },
            cursor_cell: [layout.cursor_row(), layout.cursor_column()],
            popup_rows: self.completion.view().map_or_else(Vec::new, |view| {
                view.candidates
                    .iter()
                    .map(|candidate| candidate.label.clone())
                    .collect()
            }),
            popup_visible: self.completion.view().is_some(),
            prompt: (!presentation_hidden).then(|| self.mode.symbol()),
            visual_lines: layout.visual().text_lines(self.editor.text()),
            wrap_width: if presentation_hidden {
                0
            } else {
                layout.width()
            },
        }
    }
}
