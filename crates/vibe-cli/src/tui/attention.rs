//! Focus-aware attention effects pinned to the reference
//! `TextualNotificationAdapter`
//! (`vibe/cli/textual_ui/notifications/adapters/textual_notification_adapter.py`).
//!
//! The reducer decides; a terminal port writes. Nothing here performs I/O, so
//! notification order, throttling, focus and the tab state stay replayable.
//! Every transition renders the terminal title, as the reference does.

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

/// Reference `default_title="Vibe"`, also what a blank session title resets to.
pub const DEFAULT_TITLE: &str = "Vibe";

/// Reference `NOTIFICATION_THROTTLE_SECONDS`.
pub const THROTTLE_MS: u64 = 1_000;

/// Reference `_RUNNING_INDICATOR` and `_WAITING_INDICATOR`.
const RUNNING_INDICATOR: &str = ">>";
const WAITING_INDICATOR: &str = "?";

/// Reference `_TabState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabState {
    Idle,
    Running,
    Waiting,
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
    /// Reference `get_enabled`: `enable_notifications`.
    enabled: bool,
    /// Reference `get_title_enabled`: `experimental_enable_tab_status`.
    title_enabled: bool,
    has_focus: bool,
    /// Starts at zero like the reference's monotonic `0.0`.
    last_notification_ms: u64,
    state: TabState,
    /// The context of the bell that rang while blurred, whose suffix the title
    /// carries until focus acknowledges it.
    bell_context: Option<NotificationContext>,
    default_title: String,
    /// The busy state last reported through [`Self::sync_busy`].
    reported_busy: bool,
}

impl Default for AttentionNotifier {
    fn default() -> Self {
        Self {
            enabled: true,
            title_enabled: true,
            // The reference starts focused, so a turn that completes before the
            // first focus event never rings.
            has_focus: true,
            last_notification_ms: 0,
            state: TabState::Idle,
            bell_context: None,
            default_title: DEFAULT_TITLE.to_owned(),
            reported_busy: false,
        }
    }
}

impl AttentionNotifier {
    /// Reference `get_enabled`, read on every notification.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Reference `get_title_enabled`, read on every render.
    pub fn set_title_enabled(&mut self, enabled: bool) {
        self.title_enabled = enabled;
    }

    #[must_use]
    pub const fn has_focus(&self) -> bool {
        self.has_focus
    }

    /// Reference `_on_busy_state_changed`: reports a busy transition of the
    /// client, and nothing while the busy state holds.
    pub fn sync_busy(&mut self, busy: bool) -> Option<AttentionEffect> {
        (busy != self.reported_busy).then(|| self.set_running(busy))
    }

    /// Reference `notify`.
    pub fn notify(&mut self, context: NotificationContext, now_ms: u64) -> AttentionEffect {
        self.state = match context {
            NotificationContext::ActionRequired => TabState::Waiting,
            // A completion is terminal: whatever ran or waited is over.
            NotificationContext::Complete => TabState::Idle,
        };
        if !self.enabled {
            return self.render(false);
        }
        if !self.has_focus {
            self.bell_context = Some(context);
        }
        let bell = self.fire_bell(now_ms);
        self.render(bell)
    }

    /// Reference `set_running`.
    pub fn set_running(&mut self, active: bool) -> AttentionEffect {
        self.reported_busy = active;
        self.state = if active {
            TabState::Running
        } else {
            TabState::Idle
        };
        self.bell_context = None;
        self.render(false)
    }

    /// Reference `on_focus`: focus acknowledges the current bell episode.
    pub fn on_focus(&mut self) -> AttentionEffect {
        self.has_focus = true;
        self.bell_context = None;
        self.render(false)
    }

    /// Reference `on_blur`.
    pub fn on_blur(&mut self) -> AttentionEffect {
        self.has_focus = false;
        self.render(false)
    }

    /// Reference `clear_waiting`: only a waiting tab returns to idle.
    pub fn clear_waiting(&mut self) -> AttentionEffect {
        if self.state == TabState::Waiting {
            self.state = TabState::Idle;
            self.bell_context = None;
        }
        self.render(false)
    }

    /// Reference `set_default_title`: the session title, with a blank one
    /// resetting to the product name. An unchanged title renders nothing.
    pub fn set_default_title(&mut self, title: &str) -> Option<AttentionEffect> {
        let trimmed = title.trim();
        let normalized = if trimmed.is_empty() {
            DEFAULT_TITLE
        } else {
            trimmed
        };
        if normalized == self.default_title {
            return None;
        }
        normalized.clone_into(&mut self.default_title);
        Some(self.render(false))
    }

    /// Reference `_fire_bell`: only while blurred, at most once a second.
    fn fire_bell(&mut self, now_ms: u64) -> bool {
        if self.has_focus || now_ms.saturating_sub(self.last_notification_ms) < THROTTLE_MS {
            return false;
        }
        self.last_notification_ms = now_ms;
        true
    }

    /// Reference `_render`: the suffix wins while blurred, the indicator
    /// shows when tab status is on, and control characters never reach the
    /// OSC sequence.
    fn render(&self, bell: bool) -> AttentionEffect {
        let title = match self.bell_context {
            Some(context) if !self.has_focus => {
                format!("{} - {}", self.default_title, context.title_suffix())
            }
            _ if self.title_enabled => match self.state {
                TabState::Running => format!("{RUNNING_INDICATOR} {}", self.default_title),
                TabState::Waiting => format!("{WAITING_INDICATOR} {}", self.default_title),
                TabState::Idle => self.default_title.clone(),
            },
            _ => self.default_title.clone(),
        };
        AttentionEffect {
            bell,
            title: title
                .chars()
                .filter(|character| !matches!(u32::from(*character), 0x00..=0x1f | 0x7f..=0x9f))
                .collect(),
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

    fn title(effect: &AttentionEffect) -> &str {
        &effect.title
    }

    #[test]
    fn a_focused_notification_renders_the_state_without_a_bell() {
        let mut notifier = AttentionNotifier::default();
        let effect = notifier.notify(NotificationContext::ActionRequired, 5_000);
        assert!(!effect.bell);
        assert_eq!(title(&effect), "? Vibe");
        assert_eq!(title(&notifier.set_running(true)), ">> Vibe");
        assert_eq!(
            title(&notifier.notify(NotificationContext::Complete, 6_000)),
            "Vibe"
        );
    }

    #[test]
    fn unfocused_notifications_throttle_and_carry_the_suffix() {
        let mut notifier = AttentionNotifier::default();
        assert_eq!(title(&notifier.on_blur()), "Vibe");
        let effect = notifier.notify(NotificationContext::ActionRequired, 1_000);
        assert!(effect.bell);
        assert_eq!(
            effect.sequence(),
            "\u{7}\u{1b}]0;Vibe - Action Required\u{7}"
        );
        let throttled = notifier.notify(NotificationContext::Complete, 1_999);
        assert!(!throttled.bell, "the reference throttles for one second");
        assert_eq!(title(&throttled), "Vibe - Task Complete");
        assert!(notifier.notify(NotificationContext::Complete, 2_000).bell);
        assert_eq!(title(&notifier.on_focus()), "Vibe");
    }

    #[test]
    fn disabled_notifications_still_render_the_waiting_indicator() {
        let mut notifier = AttentionNotifier::default();
        notifier.set_enabled(false);
        notifier.on_blur();
        let effect = notifier.notify(NotificationContext::ActionRequired, 5_000);
        assert!(!effect.bell);
        assert_eq!(title(&effect), "? Vibe");
    }

    #[test]
    fn tab_status_off_keeps_the_plain_title() {
        let mut notifier = AttentionNotifier::default();
        notifier.set_title_enabled(false);
        assert_eq!(title(&notifier.set_running(true)), "Vibe");
    }

    #[test]
    fn clear_waiting_leaves_a_running_tab_alone() {
        let mut notifier = AttentionNotifier::default();
        notifier.set_running(true);
        assert_eq!(title(&notifier.clear_waiting()), ">> Vibe");
        notifier.notify(NotificationContext::ActionRequired, 5_000);
        assert_eq!(title(&notifier.clear_waiting()), "Vibe");
    }

    #[test]
    fn the_session_title_replaces_the_default_and_is_sanitized() {
        let mut notifier = AttentionNotifier::default();
        assert_eq!(notifier.set_default_title("  "), None);
        let effect = notifier
            .set_default_title("fix\u{7} bug")
            .expect("a new title renders");
        assert_eq!(title(&effect), "fix bug");
        notifier.set_running(true);
        assert_eq!(title(&notifier.set_running(true)), ">> fix bug");
        assert!(notifier.set_default_title("").is_some());
        assert_eq!(title(&notifier.set_running(false)), "Vibe");
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
        let error = write_attention(&mut Failing, &AttentionNotifier::default().on_focus())
            .expect_err("write failure");
        assert!(error.starts_with("Could not signal the terminal: "));
    }
}
