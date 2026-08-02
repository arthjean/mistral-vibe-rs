//! Serializable input protocol shared by deterministic traces and the TUI adapter.

use serde::{Deserialize, Serialize};

use crate::tui::completion::{CompletionRequest, CompletionResolution};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorSnapshot {
    pub text: String,
    pub cursor: usize,
    pub selection: Option<[usize; 2]>,
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
    Meta,
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
    CompletionResolved {
        resolution: CompletionResolution,
    },
    ExternalEditor {
        #[serde(default)]
        text: Option<String>,
    },
    PasteNormalized {
        snapshot: EditorSnapshot,
        text: String,
    },
    TextNormalized {
        snapshot: EditorSnapshot,
        text: String,
    },
    Transcript {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        generation: Option<u64>,
    },
    VoiceTranscriptDelta {
        text: String,
        generation: u64,
    },
    VoiceDone {
        generation: u64,
    },
    VoicePeak {
        generation: u64,
        level: u8,
    },
    VoiceIndicatorTick,
    VoiceStartResolved {
        generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    VoiceStopResolved {
        generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
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
        request: CompletionRequest,
    },
    NormalizePastedPath {
        text: String,
        snapshot: EditorSnapshot,
    },
    NormalizeCurrentText {
        snapshot: EditorSnapshot,
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
