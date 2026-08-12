//! Live paginated debug console.
//!
//! The reference keeps the newest log lines on screen, polls for new ones, and
//! loads older pages when the operator scrolls to the top. This state machine
//! reproduces that contract over the bounded `diagnostics/logs/read` window:
//! entries are absorbed once by identity, the visible page grows only on an
//! explicit request, and a refresh never disturbs the selection.

use serde_json::Value;
use vibe_core::auth::UtcTimestamp;

use super::diagnostics::{debug_log_line, log_level_color};
use super::interaction::{Overlay, OverlayItem, OverlayKind};

/// Reference `DEFAULT_LOG_PAGE_SIZE`.
pub const PAGE_SIZE: usize = 30;
/// Reference `LOG_POLL_INTERVAL`, in milliseconds.
pub const POLL_INTERVAL_MS: u64 = 500;

/// The whole seconds an entry's timestamp names, which is what a rendered line
/// shows. The page carries the stamp the log line carried, which the reference
/// publishes as a `datetime` and this port as the text it parsed.
fn epoch_seconds(timestamp: &str) -> Option<u64> {
    let micros = UtcTimestamp::parse_iso8601(timestamp)?.micros_since_epoch();
    u64::try_from(micros.div_euclid(1_000_000)).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogLine {
    id: String,
    color: &'static str,
    text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugConsole {
    lines: Vec<LogLine>,
    /// Where the next read starts, so a poll never re-reads what it has.
    next_offset: usize,
    /// First visible line: older pages are loaded on request only.
    visible_from: usize,
    /// Set once the server reports it has nothing newer to hand over.
    drained: bool,
    /// Set once the operator paged back, so polling stops chasing the tail.
    pinned: bool,
    polled_at_ms: u64,
}

impl DebugConsole {
    #[must_use]
    pub fn next_offset(&self) -> usize {
        self.next_offset
    }

    #[must_use]
    pub fn has_older(&self) -> bool {
        self.visible_from > 0
    }

    /// Reference `_try_load_previous`: the operator asked for the page above
    /// the current window.
    pub fn load_older(&mut self) -> bool {
        if self.visible_from == 0 {
            return false;
        }
        self.visible_from = self.visible_from.saturating_sub(PAGE_SIZE);
        self.pinned = true;
        true
    }

    /// Whether a poll is due. The console never polls faster than the
    /// reference interval, so a busy loop cannot flood the resource.
    #[must_use]
    pub fn poll_due(&self, now_ms: u64) -> bool {
        !self.drained || now_ms.saturating_sub(self.polled_at_ms) >= POLL_INTERVAL_MS
    }

    /// Absorbs one page. Entries already seen are dropped, so a repeated or
    /// overlapping read cannot duplicate a line, and the visible window follows
    /// the newest entries unless the operator paged back.
    pub fn absorb(&mut self, page: &Value, now_ms: u64) {
        self.polled_at_ms = now_ms;
        let entries = page
            .pointer("/logs/entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.drained = !page
            .pointer("/logs/hasMore")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let followed_tail = !self.pinned;
        for entry in &entries {
            let id = entry
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if id.is_empty() || self.lines.iter().any(|line| line.id == id) {
                continue;
            }
            let level = entry
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or("INFO")
                .to_owned();
            let text = debug_log_line(
                entry
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(epoch_seconds)
                    .unwrap_or_default(),
                &level,
                entry
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            self.lines.push(LogLine {
                id,
                color: log_level_color(&level),
                text,
            });
        }
        self.next_offset = self.next_offset.saturating_add(entries.len());
        if followed_tail {
            self.visible_from = self.lines.len().saturating_sub(PAGE_SIZE);
        }
    }

    /// Rebuilds the overlay, restoring the highlighted row when it survived the
    /// refresh so polling never moves the operator's place.
    #[must_use]
    pub fn overlay(&self, selected: Option<&str>) -> Overlay {
        let mut items = self
            .lines
            .iter()
            .skip(self.visible_from)
            .map(|line| OverlayItem::new(line.id.clone(), line.color, line.text.clone(), false))
            .collect::<Vec<_>>();
        if items.is_empty() {
            items.push(OverlayItem::new(
                "empty",
                "No debug events",
                "The runtime log buffer is empty",
                true,
            ));
        }
        let mut overlay = Overlay::new(OverlayKind::Debug, "Debug console", items);
        if self.has_older() {
            overlay.notice = Some("PgUp loads older entries".to_owned());
        }
        if let Some(selected) = selected {
            overlay.select_by_id(selected);
        }
        overlay
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// One page in the shape `diagnostics/logs/read` publishes: the timestamp
    /// is the stamp the log line carried, which the reference sends as a
    /// `datetime` and this port as the text it parsed.
    fn page(ids: &[usize], has_more: bool) -> Value {
        json!({"logs": {
            "entries": ids.iter().map(|index| json!({
                "id": format!("log-{index}"),
                "timestamp": UtcTimestamp::from_micros_since_epoch(
                    (1_754_179_200 + i64::try_from(*index).unwrap_or_default()) * 1_000_000,
                )
                .to_iso8601(),
                "level": if index % 2 == 0 { "INFO" } else { "ERROR" },
                "message": format!("entry {index}"),
            })).collect::<Vec<_>>(),
            "hasMore": has_more,
        }})
    }

    #[test]
    fn polling_appends_only_unseen_entries_and_advances_the_read_cursor() {
        let mut console = DebugConsole::default();
        console.absorb(&page(&[0, 1], true), 0);
        assert_eq!(console.next_offset(), 2);
        // A poll that re-delivers what it already has must change nothing.
        console.absorb(&page(&[0, 1], false), POLL_INTERVAL_MS);
        assert_eq!(console.overlay(None).items.len(), 2);
        console.absorb(&page(&[2], false), POLL_INTERVAL_MS * 2);
        let overlay = console.overlay(None);
        assert_eq!(overlay.items.len(), 3);
        assert_eq!(
            overlay.items[2].description,
            "2025-08-03 00:00:02 INFO     entry 2"
        );
    }

    #[test]
    fn the_window_follows_the_tail_until_the_operator_pages_back() {
        let mut console = DebugConsole::default();
        let ids = (0..PAGE_SIZE + 5).collect::<Vec<_>>();
        console.absorb(&page(&ids, false), 0);
        assert_eq!(console.overlay(None).items.len(), PAGE_SIZE);
        assert!(console.has_older());
        assert!(console.load_older());
        assert!(!console.has_older());
        assert_eq!(console.overlay(None).items.len(), PAGE_SIZE + 5);
        // With an older page pinned, a poll must not scroll the view away.
        console.absorb(&page(&[100], false), POLL_INTERVAL_MS);
        assert_eq!(console.overlay(None).items.len(), PAGE_SIZE + 6);
        assert!(!console.load_older());
    }

    #[test]
    fn an_empty_buffer_stays_selectable_and_the_selection_survives_a_refresh() {
        let console = DebugConsole::default();
        let overlay = console.overlay(None);
        assert_eq!(overlay.items.len(), 1);
        assert_eq!(overlay.items[0].label, "No debug events");

        let mut console = DebugConsole::default();
        console.absorb(&page(&[0, 1, 2], false), 0);
        let overlay = console.overlay(Some("log-1"));
        assert_eq!(
            overlay.selected_item().map(|item| item.id.as_str()),
            Some("log-1")
        );
    }

    #[test]
    fn polling_is_bounded_by_the_reference_interval_once_the_buffer_is_drained() {
        let mut console = DebugConsole::default();
        assert!(console.poll_due(0));
        console.absorb(&page(&[0], false), 1_000);
        assert!(!console.poll_due(1_400));
        assert!(console.poll_due(1_500));
        console.absorb(&page(&[1], true), 2_000);
        assert!(console.poll_due(2_001), "an undrained buffer keeps reading");
    }
}
