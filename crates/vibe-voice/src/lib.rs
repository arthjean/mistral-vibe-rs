//! Voice input, read-aloud and the narrator.
//!
//! The reference's terminal and editor adapters share one audio stack
//! (`vibe/cli/voice_manager/`, `vibe/cli/narrator_manager/`,
//! `vibe/cli/audio_recorder/`, `vibe/cli/audio_player/`,
//! `vibe/cli/transcribe/`, `vibe/cli/tts/`), the editor bridge in
//! `vibe/acp/voice.py` wiring the very managers the terminal drives. This
//! crate is that stack for both adapters of this port: the recorder and the
//! player over CPAL, the realtime transcription and speech clients, the voice
//! manager that ties a recording to its transcription, and the narrator's
//! state machine.

#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_in_result,
        clippy::unwrap_used
    )
)]

pub mod capture;
pub mod identity;
pub mod manager;
pub mod narrator;
pub mod playback;
pub mod settings;
pub mod speech;
mod tracking;
pub mod transcribe;

#[cfg(test)]
mod test_endpoint;
#[cfg(test)]
mod voice_parity_tests;

pub use manager::{RecordingStartError, StartRequest, TranscribeState, VoiceEvent, VoiceManager};
pub use narrator::{NarratorEffect, NarratorManager, NarratorState};
pub use speech::{SpeechEvent, SpeechFailure, SpeechManager};
