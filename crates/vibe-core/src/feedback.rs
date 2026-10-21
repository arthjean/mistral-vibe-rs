//! Whether a finished turn asks the user to rate the session.
//!
//! Reference `vibe/core/feedback.py`: a Mistral model with telemetry on, a
//! conversation of at least three user messages, no recent prompt, answer,
//! or snooze, and then a small random chance. The timestamps live in the
//! `user_feedback` section of the shared `cache.toml`.

use std::path::Path;

use crate::cache_store::CacheStore;

/// Reference `FEEDBACK_PROBABILITY`.
const FEEDBACK_PROBABILITY: f64 = 0.05;
/// Reference `FEEDBACK_COOLDOWN_SECONDS`.
const FEEDBACK_COOLDOWN_SECONDS: i64 = 3_600;
/// Reference `FEEDBACK_RESPONDED_COOLDOWN_SECONDS`.
const FEEDBACK_RESPONDED_COOLDOWN_SECONDS: i64 = 86_400;
/// Reference `FEEDBACK_SNOOZED_COOLDOWN_SECONDS`, which a client is also told
/// so it can say how long a snooze lasts.
pub const FEEDBACK_SNOOZED_COOLDOWN_SECONDS: i64 = 604_800;
/// Reference `MIN_USER_MESSAGES_FOR_FEEDBACK`.
const MIN_USER_MESSAGES_FOR_FEEDBACK: usize = 3;

const CACHE_SECTION: &str = "user_feedback";
const LAST_SHOWN_KEY: &str = "last_shown_at";
const RESPONDED_AT_KEY: &str = "responded_at";
const SNOOZED_AT_KEY: &str = "snoozed_at";

/// What the user did with a feedback prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackAction {
    Asked,
    Given,
    Snoozed,
}

impl FeedbackAction {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "asked" => Some(Self::Asked),
            "given" => Some(Self::Given),
            "snoozed" => Some(Self::Snoozed),
            _ => None,
        }
    }

    const fn key(self) -> &'static str {
        match self {
            Self::Asked => LAST_SHOWN_KEY,
            Self::Given => RESPONDED_AT_KEY,
            Self::Snoozed => SNOOZED_AT_KEY,
        }
    }
}

/// The `user_feedback` section of `cache.toml` under one Vibe home.
#[derive(Debug, Clone)]
pub struct FeedbackCache {
    store: CacheStore,
}

impl FeedbackCache {
    #[must_use]
    pub fn new(vibe_home: &Path) -> Self {
        Self {
            store: CacheStore::new(vibe_home),
        }
    }

    /// Reference `should_show_feedback`. `roll` is the uniform draw in
    /// `[0, 1)` the reference takes from `random.random()`.
    #[must_use]
    pub fn should_show(
        &self,
        telemetry_active: bool,
        is_mistral_model: bool,
        user_message_count: usize,
        now: i64,
        roll: f64,
    ) -> bool {
        if !telemetry_active || !is_mistral_model {
            return false;
        }
        if user_message_count < MIN_USER_MESSAGES_FOR_FEEDBACK {
            return false;
        }
        if self.within_cooldown(now) {
            return false;
        }
        roll <= FEEDBACK_PROBABILITY
    }

    /// Reference `record_feedback_asked`, `record_feedback_given`, and
    /// `record_feedback_snoozed`. A write that fails is logged and dropped,
    /// as the reference's cache store drops it.
    pub fn record(&self, action: FeedbackAction, now: i64) {
        let mut data = toml::Table::new();
        data.insert(action.key().to_owned(), toml::Value::Integer(now));
        self.store.write_section(CACHE_SECTION, data);
    }

    /// Reference `_is_within_cooldown`.
    fn within_cooldown(&self, now: i64) -> bool {
        let section = self.store.read_section(CACHE_SECTION);
        let stamp = |key: &str| section.get(key).and_then(toml::Value::as_integer);
        if stamp(SNOOZED_AT_KEY)
            .is_some_and(|snoozed| now - snoozed < FEEDBACK_SNOOZED_COOLDOWN_SECONDS)
        {
            return true;
        }
        if stamp(RESPONDED_AT_KEY)
            .is_some_and(|responded| now - responded < FEEDBACK_RESPONDED_COOLDOWN_SECONDS)
        {
            return true;
        }
        stamp(LAST_SHOWN_KEY)
            .is_some_and(|shown| shown > 0 && now - shown < FEEDBACK_COOLDOWN_SECONDS)
    }
}

/// A uniform draw in `[0, 1)`, or `1.0` (which never shows) when the system
/// has no randomness to give.
#[must_use]
pub fn uniform_roll() -> f64 {
    let mut bytes = [0_u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return 1.0;
    }
    let bits = u64::from_le_bytes(bytes) >> 11;
    // 53 random bits over 2^53 is exact in an f64.
    let scale = f64::from(1_u32 << 26) * f64::from(1_u32 << 27);
    let high = u32::try_from(bits >> 21).unwrap_or(u32::MAX);
    let low = u32::try_from(bits & 0x1f_ffff).unwrap_or(0);
    (f64::from(high) * f64::from(1_u32 << 21) + f64::from(low)) / scale
}
