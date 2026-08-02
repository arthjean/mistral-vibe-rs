use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoicePhase {
    Disabled,
    #[default]
    Idle,
    Starting,
    Recording,
    Transcribing,
}

impl VoicePhase {
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Recording | Self::Transcribing)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoiceCommand {
    Start,
    Stop,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VoiceUpdate {
    Transcript {
        text: String,
        generation: Option<u64>,
    },
    Delta {
        text: String,
        generation: u64,
    },
    Done {
        generation: u64,
    },
    Peak {
        generation: u64,
        level: u8,
    },
    IndicatorTick,
    StartResolved {
        generation: u64,
        error: Option<String>,
    },
    StopResolved {
        generation: u64,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VoiceUpdateOutcome {
    None,
    Insert(String),
    Notify(String),
    Rejected(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VoiceKeyOutcome {
    pub consumed: bool,
    pub command: Option<VoiceCommand>,
}

#[derive(Debug, Default)]
pub(crate) struct VoiceState {
    phase: VoicePhase,
    generation: u64,
    level: u8,
    frame: u8,
}

impl VoiceState {
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        let next = if enabled {
            VoicePhase::Idle
        } else {
            VoicePhase::Disabled
        };
        if self.phase == next {
            return;
        }
        self.generation = self.generation.saturating_add(1);
        self.reset(next);
    }

    #[must_use]
    pub(crate) const fn phase(&self) -> VoicePhase {
        self.phase
    }

    #[must_use]
    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub(crate) const fn indicator(&self) -> u8 {
        if matches!(self.phase, VoicePhase::Recording) {
            self.level
        } else {
            self.frame
        }
    }

    pub(crate) fn handle_key(
        &mut self,
        start_requested: bool,
        cancel_requested: bool,
    ) -> VoiceKeyOutcome {
        if self.phase.is_active() {
            let command = if cancel_requested {
                self.generation = self.generation.saturating_add(1);
                self.reset(VoicePhase::Idle);
                Some(VoiceCommand::Cancel)
            } else if self.phase == VoicePhase::Recording {
                self.phase = VoicePhase::Transcribing;
                self.frame = 0;
                Some(VoiceCommand::Stop)
            } else {
                None
            };
            return VoiceKeyOutcome {
                consumed: true,
                command,
            };
        }
        if self.phase == VoicePhase::Idle && start_requested {
            self.generation = self.generation.saturating_add(1);
            self.reset(VoicePhase::Starting);
            return VoiceKeyOutcome {
                consumed: true,
                command: Some(VoiceCommand::Start),
            };
        }
        VoiceKeyOutcome {
            consumed: false,
            command: None,
        }
    }

    pub(crate) fn apply(&mut self, update: VoiceUpdate) -> VoiceUpdateOutcome {
        match update {
            VoiceUpdate::Transcript { text, generation } => {
                if generation.is_some_and(|generation| generation != self.generation)
                    || !self.phase.is_active()
                {
                    return VoiceUpdateOutcome::Rejected("stale voice transcript");
                }
                self.reset(VoicePhase::Idle);
                if text.is_empty() {
                    VoiceUpdateOutcome::None
                } else {
                    VoiceUpdateOutcome::Insert(text)
                }
            }
            VoiceUpdate::Delta { text, generation } => {
                if generation != self.generation || !self.phase.is_active() || text.is_empty() {
                    VoiceUpdateOutcome::None
                } else {
                    VoiceUpdateOutcome::Insert(text)
                }
            }
            VoiceUpdate::Done { generation } => {
                if generation == self.generation && self.phase.is_active() {
                    self.reset(VoicePhase::Idle);
                }
                VoiceUpdateOutcome::None
            }
            VoiceUpdate::Peak { generation, level } => {
                if generation == self.generation && self.phase == VoicePhase::Recording {
                    self.level = level.min(7);
                }
                VoiceUpdateOutcome::None
            }
            VoiceUpdate::IndicatorTick => {
                if self.phase == VoicePhase::Transcribing {
                    self.frame = (self.frame + 1) % 8;
                }
                VoiceUpdateOutcome::None
            }
            VoiceUpdate::StartResolved { generation, error } => {
                if generation != self.generation || self.phase != VoicePhase::Starting {
                    return VoiceUpdateOutcome::None;
                }
                if let Some(error) = error {
                    self.reset(VoicePhase::Idle);
                    VoiceUpdateOutcome::Notify(error)
                } else {
                    self.phase = VoicePhase::Recording;
                    VoiceUpdateOutcome::None
                }
            }
            VoiceUpdate::StopResolved { generation, error } => {
                if generation != self.generation || !self.phase.is_active() {
                    return VoiceUpdateOutcome::None;
                }
                if let Some(error) = error {
                    self.reset(VoicePhase::Idle);
                    VoiceUpdateOutcome::Notify(error)
                } else {
                    self.phase = VoicePhase::Transcribing;
                    self.frame = 0;
                    VoiceUpdateOutcome::None
                }
            }
        }
    }

    fn reset(&mut self, phase: VoicePhase) {
        self.phase = phase;
        self.level = 0;
        self.frame = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_updates_cannot_escape_generation_invalidation() {
        let mut state = VoiceState::default();
        state.set_enabled(true);
        state.handle_key(true, false);
        let stale = state.generation();
        state.handle_key(false, true);
        state.handle_key(true, false);

        assert_eq!(
            state.apply(VoiceUpdate::Delta {
                text: "stale".to_owned(),
                generation: stale,
            }),
            VoiceUpdateOutcome::None
        );
        assert_eq!(state.phase(), VoicePhase::Starting);
    }
}
