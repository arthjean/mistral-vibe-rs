//! Focus-aware attention effects pinned to the reference
//! `TextualNotificationAdapter`.
//!
//! The reducer decides; a terminal port writes. Nothing here performs I/O, so
//! notification order, throttling, and focus state stay replayable.

use std::io::Write;

/// Reference `NotificationContext`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationContext {
    ActionRequired,
    Complete,
}

impl NotificationContext {
    /// Reference `NOTIFICATION_TITLE_SUFFIXES`.
    #[must_use]
    pub const fn title_suffix(self) -> &'static str {
        match self {
            Self::ActionRequired => "Action Required",
            Self::Complete => "Task Complete",
        }
    }
}

/// Reference `default_title="Vibe"`.
pub const DEFAULT_TITLE: &str = "Vibe";

/// Reference `NOTIFICATION_THROTTLE_SECONDS`.
pub const THROTTLE_MS: u64 = 1_000;

/// When notifications may be emitted, from the `notifications` configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NotificationPolicy {
    Off,
    #[default]
    WhenUnfocused,
    /// A Rust-only superset of the reference boolean, kept from the existing
    /// configuration surface: focus no longer suppresses the effect.
    Always,
}

impl NotificationPolicy {
    #[must_use]
    pub fn from_config(value: Option<&str>) -> Self {
        match value {
            Some("off") => Self::Off,
            Some("always") => Self::Always,
            _ => Self::WhenUnfocused,
        }
    }
}

/// One terminal write: the reference rings the bell, then sets the title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionEffect {
    pub bell: bool,
    pub title: String,
}

impl AttentionEffect {
    #[must_use]
    pub fn sequence(&self) -> String {
        let bell = if self.bell { "\u{7}" } else { "" };
        format!("{bell}\u{1b}]0;{}\u{7}", self.title)
    }
}

/// Reference `TextualNotificationAdapter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionNotifier {
    policy: NotificationPolicy,
    has_focus: bool,
    last_notification_ms: Option<u64>,
}

impl Default for AttentionNotifier {
    fn default() -> Self {
        Self {
            policy: NotificationPolicy::default(),
            // The reference starts focused, so a turn that completes before the
            // first focus event never rings.
            has_focus: true,
            last_notification_ms: None,
        }
    }
}

impl AttentionNotifier {
    pub fn set_policy(&mut self, policy: NotificationPolicy) {
        self.policy = policy;
    }

    #[must_use]
    pub const fn has_focus(&self) -> bool {
        self.has_focus
    }

    /// Reference `notify`.
    pub fn notify(&mut self, context: NotificationContext, now_ms: u64) -> Option<AttentionEffect> {
        match self.policy {
            NotificationPolicy::Off => return None,
            NotificationPolicy::WhenUnfocused if self.has_focus => return None,
            NotificationPolicy::WhenUnfocused | NotificationPolicy::Always => {}
        }
        if self
            .last_notification_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < THROTTLE_MS)
        {
            return None;
        }
        self.last_notification_ms = Some(now_ms);
        Some(AttentionEffect {
            bell: true,
            title: format!("{DEFAULT_TITLE} - {}", context.title_suffix()),
        })
    }

    /// Reference `on_focus`: focus restores the plain title.
    pub fn on_focus(&mut self) -> Option<AttentionEffect> {
        self.has_focus = true;
        Some(Self::restore())
    }

    /// Reference `on_blur`.
    pub fn on_blur(&mut self) {
        self.has_focus = false;
    }

    /// Reference `restore`.
    #[must_use]
    pub fn restore() -> AttentionEffect {
        AttentionEffect {
            bell: false,
            title: DEFAULT_TITLE.to_owned(),
        }
    }
}

/// Writes an attention effect to the terminal. A failing terminal never fails a
/// turn: the caller keeps the scoped error and continues.
pub fn write_attention(writer: &mut impl Write, effect: &AttentionEffect) -> Result<(), String> {
    writer
        .write_all(effect.sequence().as_bytes())
        .and_then(|()| writer.flush())
        .map_err(|error| format!("Could not signal the terminal: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focused_and_disabled_states_emit_nothing() {
        let mut notifier = AttentionNotifier::default();
        assert_eq!(notifier.notify(NotificationContext::Complete, 10), None);
        notifier.on_blur();
        notifier.set_policy(NotificationPolicy::Off);
        assert_eq!(notifier.notify(NotificationContext::Complete, 10), None);
    }

    #[test]
    fn unfocused_notifications_throttle_and_carry_the_reference_title() {
        let mut notifier = AttentionNotifier::default();
        notifier.on_blur();
        let effect = notifier
            .notify(NotificationContext::ActionRequired, 1_000)
            .expect("first notification");
        assert_eq!(effect.title, "Vibe - Action Required");
        assert!(effect.bell);
        assert_eq!(
            effect.sequence(),
            "\u{7}\u{1b}]0;Vibe - Action Required\u{7}"
        );
        assert_eq!(
            notifier.notify(NotificationContext::Complete, 1_999),
            None,
            "the reference throttles for one second"
        );
        assert_eq!(
            notifier
                .notify(NotificationContext::Complete, 2_000)
                .map(|effect| effect.title),
            Some("Vibe - Task Complete".to_owned())
        );
    }

    #[test]
    fn always_ignores_focus_but_keeps_the_throttle() {
        let mut notifier = AttentionNotifier::default();
        notifier.set_policy(NotificationPolicy::Always);
        assert!(notifier.has_focus());
        assert!(notifier.notify(NotificationContext::Complete, 0).is_some());
        assert_eq!(notifier.notify(NotificationContext::Complete, 500), None);
    }

    #[test]
    fn focus_restores_the_default_title() {
        let mut notifier = AttentionNotifier::default();
        notifier.on_blur();
        assert!(notifier.notify(NotificationContext::Complete, 0).is_some());
        assert_eq!(
            notifier.on_focus(),
            Some(AttentionEffect {
                bell: false,
                title: "Vibe".to_owned(),
            })
        );
        assert!(notifier.has_focus());
        assert_eq!(
            AttentionNotifier::restore().sequence(),
            "\u{1b}]0;Vibe\u{7}"
        );
    }

    #[test]
    fn terminal_write_failure_is_reported_without_panicking() {
        struct Failing;
        impl Write for Failing {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("closed"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let error = write_attention(&mut Failing, &AttentionNotifier::restore())
            .expect_err("write failure");
        assert!(error.starts_with("Could not signal the terminal: "));
    }

    #[test]
    fn policy_parses_the_configured_values() {
        assert_eq!(
            NotificationPolicy::from_config(Some("off")),
            NotificationPolicy::Off
        );
        assert_eq!(
            NotificationPolicy::from_config(Some("always")),
            NotificationPolicy::Always
        );
        assert_eq!(
            NotificationPolicy::from_config(None),
            NotificationPolicy::WhenUnfocused
        );
    }
}
