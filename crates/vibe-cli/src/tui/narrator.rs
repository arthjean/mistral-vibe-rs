//! Narrator lifecycle pinned to the reference `NarratorManager` and
//! `TurnSummaryTracker`.
//!
//! The manager is a pure state machine over turn events. Summaries and playback
//! are effects the caller executes, and every late result carries the generation
//! that produced it so a cancelled turn can never speak.

/// Reference `NarratorState`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NarratorState {
    #[default]
    Idle,
    Summarizing,
    Speaking,
}

impl NarratorState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Summarizing => "summarizing",
            Self::Speaking => "speaking",
        }
    }
}

/// Reference `TurnSummaryData`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TurnData {
    user_message: String,
    message_id: Option<String>,
    assistant_fragments: Vec<String>,
    error: Option<String>,
}

/// What the caller must execute for the narrator to progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NarratorEffect {
    /// Reference `TurnSummaryTracker._generate_summary`.
    Summarize {
        generation: u64,
        user_message: String,
        assistant_text: String,
        error: Option<String>,
        message_id: Option<String>,
    },
    /// Reference `_speak_summary`.
    Speak { generation: u64, text: String },
    /// Reference `cancel`: stop any summary task and audio playback.
    Stop,
}

/// Reference `NarratorManager`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarratorManager {
    enabled: bool,
    /// Reference `tts_client is not None`.
    speech_available: bool,
    state: NarratorState,
    generation: u64,
    data: Option<TurnData>,
    /// The generation whose summary or playback is still outstanding.
    outstanding: Option<u64>,
}

impl Default for NarratorManager {
    fn default() -> Self {
        Self::new(false, false)
    }
}

impl NarratorManager {
    #[must_use]
    pub const fn new(enabled: bool, speech_available: bool) -> Self {
        Self {
            enabled,
            speech_available,
            state: NarratorState::Idle,
            generation: 0,
            data: None,
            outstanding: None,
        }
    }

    #[must_use]
    pub const fn state(&self) -> NarratorState {
        self.state
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Reference `sync`: a configuration change cancels first, then rebuilds the
    /// summary and speech ports.
    pub fn sync(&mut self, enabled: bool, speech_available: bool) -> Option<NarratorEffect> {
        let effect = self.cancel();
        self.enabled = enabled;
        self.speech_available = speech_available;
        effect
    }

    /// Reference `on_turn_start`: a disabled narrator tracks nothing.
    pub fn on_turn_start(&mut self, user_message: &str) {
        if !self.enabled {
            self.data = None;
            return;
        }
        self.generation = self.generation.saturating_add(1);
        self.data = Some(TurnData {
            user_message: user_message.to_owned(),
            ..TurnData::default()
        });
    }

    /// Reference `on_user_message`.
    pub fn on_user_message(&mut self, message_id: &str) {
        if let Some(data) = self.data.as_mut() {
            data.message_id = Some(message_id.to_owned());
        }
    }

    /// Reference `on_assistant_text`: empty fragments are dropped.
    pub fn on_assistant_text(&mut self, content: &str) {
        if content.is_empty() {
            return;
        }
        if let Some(data) = self.data.as_mut() {
            data.assistant_fragments.push(content.to_owned());
        }
    }

    /// Reference `on_turn_error`.
    pub fn on_turn_error(&mut self, message: &str) {
        if let Some(data) = self.data.as_mut() {
            data.error = Some(message.to_owned());
        }
    }

    /// Reference `on_turn_cancel`: a cancelled turn is never summarized.
    pub fn on_turn_cancel(&mut self) {
        self.data = None;
    }

    /// Reference `on_turn_end`.
    pub fn on_turn_end(&mut self) -> Option<NarratorEffect> {
        let data = self.data.take()?;
        if !self.enabled || !self.speech_available {
            return None;
        }
        self.state = NarratorState::Summarizing;
        self.outstanding = Some(self.generation);
        Some(NarratorEffect::Summarize {
            generation: self.generation,
            user_message: data.user_message,
            assistant_text: data.assistant_fragments.concat(),
            error: data.error,
            message_id: data.message_id,
        })
    }

    /// Reference `_on_turn_summary`: a stale or empty summary returns to idle.
    pub fn apply_summary(
        &mut self,
        generation: u64,
        summary: Option<String>,
    ) -> Option<NarratorEffect> {
        if self.outstanding != Some(generation) || generation != self.generation {
            return None;
        }
        let text = summary.filter(|summary| !summary.trim().is_empty());
        match text {
            Some(text) if self.speech_available => Some(NarratorEffect::Speak { generation, text }),
            _ => {
                self.settle(generation);
                None
            }
        }
    }

    /// Reference `_speak_summary` once audio starts.
    pub fn playback_started(&mut self, generation: u64) {
        if self.outstanding == Some(generation) {
            self.state = NarratorState::Speaking;
        }
    }

    /// Reference `_on_playback_finished` and the TTS failure path.
    pub fn settle(&mut self, generation: u64) {
        if self.outstanding == Some(generation) {
            self.outstanding = None;
            self.state = NarratorState::Idle;
        }
    }

    /// Reference `cancel`.
    pub fn cancel(&mut self) -> Option<NarratorEffect> {
        self.data = None;
        if self.state == NarratorState::Idle && self.outstanding.is_none() {
            return None;
        }
        self.outstanding = None;
        self.state = NarratorState::Idle;
        Some(NarratorEffect::Stop)
    }

    /// Reference `close`: shutdown cancels and then disables further work.
    pub fn shutdown(&mut self) -> Option<NarratorEffect> {
        let effect = self.cancel();
        self.enabled = false;
        effect
    }

    /// Reference `NarratorStatus._tick`, without its animation frames.
    #[must_use]
    pub fn status_line(&self) -> Option<String> {
        match self.state {
            NarratorState::Idle => None,
            NarratorState::Summarizing | NarratorState::Speaking => {
                Some(format!("{} Esc/Ctrl+C to stop", self.state.label()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_manager() -> NarratorManager {
        NarratorManager::new(true, true)
    }

    #[test]
    fn a_disabled_narrator_never_summarizes() {
        let mut narrator = NarratorManager::new(false, true);
        narrator.on_turn_start("write the parser");
        narrator.on_assistant_text("done");
        assert_eq!(narrator.on_turn_end(), None);
        assert_eq!(narrator.state(), NarratorState::Idle);
        assert_eq!(narrator.status_line(), None);
    }

    #[test]
    fn missing_speech_keeps_the_turn_successful_and_silent() {
        let mut narrator = NarratorManager::new(true, false);
        narrator.on_turn_start("write the parser");
        narrator.on_assistant_text("done");
        assert_eq!(narrator.on_turn_end(), None);
        assert_eq!(narrator.state(), NarratorState::Idle);
    }

    #[test]
    fn a_completed_turn_summarizes_then_speaks() {
        let mut narrator = enabled_manager();
        narrator.on_turn_start("write the parser");
        narrator.on_user_message("message-1");
        narrator.on_assistant_text("first ");
        narrator.on_assistant_text("");
        narrator.on_assistant_text("second");
        assert_eq!(
            narrator.on_turn_end(),
            Some(NarratorEffect::Summarize {
                generation: 1,
                user_message: "write the parser".to_owned(),
                assistant_text: "first second".to_owned(),
                error: None,
                message_id: Some("message-1".to_owned()),
            })
        );
        assert_eq!(narrator.state(), NarratorState::Summarizing);
        assert_eq!(
            narrator.status_line().as_deref(),
            Some("summarizing Esc/Ctrl+C to stop")
        );
        assert_eq!(
            narrator.apply_summary(1, Some("wrote the parser".to_owned())),
            Some(NarratorEffect::Speak {
                generation: 1,
                text: "wrote the parser".to_owned(),
            })
        );
        // The reference only reports speaking once audio starts.
        assert_eq!(narrator.state(), NarratorState::Summarizing);
        narrator.playback_started(1);
        assert_eq!(narrator.state(), NarratorState::Speaking);
        assert_eq!(
            narrator.status_line().as_deref(),
            Some("speaking Esc/Ctrl+C to stop")
        );
        narrator.settle(1);
        assert_eq!(narrator.state(), NarratorState::Idle);
    }

    #[test]
    fn a_failed_turn_summarizes_its_error() {
        let mut narrator = enabled_manager();
        narrator.on_turn_start("deploy");
        narrator.on_turn_error("Rate limits exceeded.");
        let effect = narrator
            .on_turn_end()
            .expect("failed turns still summarize");
        assert!(matches!(
            effect,
            NarratorEffect::Summarize { error: Some(error), .. } if error == "Rate limits exceeded."
        ));
    }

    #[test]
    fn a_cancelled_turn_is_never_summarized() {
        let mut narrator = enabled_manager();
        narrator.on_turn_start("deploy");
        narrator.on_turn_cancel();
        assert_eq!(narrator.on_turn_end(), None);
        assert_eq!(narrator.state(), NarratorState::Idle);
    }

    #[test]
    fn late_results_are_rejected_by_identity() {
        let mut narrator = enabled_manager();
        narrator.on_turn_start("first");
        narrator.on_turn_end().expect("first summary");
        assert_eq!(narrator.cancel(), Some(NarratorEffect::Stop));
        narrator.on_turn_start("second");
        narrator.on_turn_end().expect("second summary");

        assert_eq!(
            narrator.apply_summary(1, Some("stale".to_owned())),
            None,
            "a summary from a cancelled generation never speaks"
        );
        assert_eq!(narrator.state(), NarratorState::Summarizing);
        narrator.settle(1);
        assert_eq!(
            narrator.state(),
            NarratorState::Summarizing,
            "a stale settle never clears live work"
        );
        assert!(
            narrator
                .apply_summary(2, Some("fresh".to_owned()))
                .is_some()
        );
    }

    #[test]
    fn empty_summaries_and_repeated_cancellation_settle_once() {
        let mut narrator = enabled_manager();
        narrator.on_turn_start("first");
        narrator.on_turn_end().expect("summary");
        assert_eq!(narrator.apply_summary(1, Some("   ".to_owned())), None);
        assert_eq!(narrator.state(), NarratorState::Idle);
        assert_eq!(narrator.cancel(), None, "an idle narrator emits no stop");

        narrator.on_turn_start("second");
        narrator.on_turn_end().expect("summary");
        assert_eq!(narrator.shutdown(), Some(NarratorEffect::Stop));
        assert!(!narrator.enabled());
        narrator.on_turn_start("after shutdown");
        assert_eq!(narrator.on_turn_end(), None);
    }

    #[test]
    fn sync_cancels_live_work_before_applying_the_new_preference() {
        let mut narrator = enabled_manager();
        narrator.on_turn_start("first");
        narrator.on_turn_end().expect("summary");
        assert_eq!(narrator.sync(false, true), Some(NarratorEffect::Stop));
        assert_eq!(narrator.state(), NarratorState::Idle);
        assert!(!narrator.enabled());
    }
}
