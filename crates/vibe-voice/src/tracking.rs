//! What one transcription reports about itself.
//!
//! The reference keeps a small tracking record beside its voice manager and
//! emits four events off it: the session identifier the endpoint answers with,
//! how much transcript accumulated, how long the transcription ran and how long
//! the recording that fed it lasted
//! (`vibe/cli/voice_manager/telemetry.py:8-30` and
//! `vibe/cli/voice_manager/voice_manager.py:202-251`). This module holds the
//! same record and builds the same four events from it.
//!
//! The payloads themselves live in `vibe_core::telemetry::records`, where the
//! differential replay can measure them, and each one is handed to the same
//! client every other event of this process goes to.

use std::time::{Duration, Instant};

use vibe_core::telemetry::TelemetryRecord;

/// Reference `MIN_SIGNAL_RECORDING_DURATION_MS`: below this, a recording is too
/// short to have produced a block, so silence means it stopped early rather
/// than that the microphone is muted.
pub(crate) const MIN_SIGNAL_RECORDING_DURATION: Duration = Duration::from_millis(500);

/// Reference `TranscriptionTrackingState`: the recording identity, when the
/// transcription started, how much text it produced and how long the recording
/// that fed it lasted.
#[derive(Debug)]
pub(crate) struct TranscriptionTracking {
    recording_id: String,
    start: Instant,
    accumulated_transcript_length: usize,
    last_recording_duration: Option<Duration>,
}

impl Default for TranscriptionTracking {
    fn default() -> Self {
        Self {
            recording_id: String::new(),
            start: Instant::now(),
            accumulated_transcript_length: 0,
            last_recording_duration: None,
        }
    }
}

impl TranscriptionTracking {
    /// Reference `reset`, called where a recording starts.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Reference `set_recording_id`, filled from the `session.created` frame.
    pub(crate) fn set_recording_id(&mut self, recording_id: String) {
        self.recording_id = recording_id;
    }

    /// Reference `record_text`: the accumulated length counts characters, which
    /// is what `len(text)` counts on the reference's `str`.
    pub(crate) fn record_text(&mut self, text: &str) {
        self.accumulated_transcript_length = self
            .accumulated_transcript_length
            .saturating_add(text.chars().count());
    }

    /// Reference `set_recording_duration`, taken where the recorder stops.
    pub(crate) fn set_recording_duration(&mut self, duration: Duration) {
        self.last_recording_duration = Some(duration);
    }

    /// Reference `accumulated_transcript_length`.
    pub(crate) fn transcript_length(&self) -> usize {
        self.accumulated_transcript_length
    }

    /// Reference `_recorded_long_enough_for_signal`: a recording that never
    /// stopped cleanly, or stopped before it could deliver a block, says
    /// nothing about the microphone.
    pub(crate) fn recorded_long_enough_for_signal(&self) -> bool {
        self.last_recording_duration
            .is_some_and(|duration| duration >= MIN_SIGNAL_RECORDING_DURATION)
    }

    /// How long ago the recording started, which is what the reference's
    /// monotonic `elapsed_ms` measures.
    pub(crate) fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    pub(crate) fn start_event(&self) -> TelemetryRecord {
        TelemetryRecord::TranscriptionStarted {
            recording_id: self.recording_id.clone(),
        }
    }

    pub(crate) fn cancel_event(&self) -> TelemetryRecord {
        TelemetryRecord::TranscriptionCancelled {
            recording_id: self.recording_id.clone(),
            recording_duration: self.elapsed(),
        }
    }

    /// Reference `_on_audio_transcription_done`: a recording whose duration was
    /// never taken reports the transcription's own elapsed time rather than
    /// nothing, so both durations are always present on this event.
    pub(crate) fn done_event(&self) -> TelemetryRecord {
        let transcription_duration = self.elapsed();
        TelemetryRecord::TranscriptionDone {
            recording_id: self.recording_id.clone(),
            transcript_length: self.accumulated_transcript_length as u64,
            transcription_duration,
            recording_duration: self
                .last_recording_duration
                .unwrap_or(transcription_duration),
        }
    }

    /// Reference `_on_audio_transcription_error`: the recording duration is
    /// reported as it stands, which is null when the recording never stopped
    /// cleanly, rather than being filled in from the transcription's.
    pub(crate) fn error_event(&self, message: &str) -> TelemetryRecord {
        TelemetryRecord::TranscriptionFailed {
            recording_id: self.recording_id.clone(),
            message: message.to_owned(),
            transcription_duration: self.elapsed(),
            recording_duration: self.last_recording_duration,
        }
    }
}
