//! Pastes recovered from key events on Windows.
//!
//! crossterm reads the Windows console through `ReadConsoleInput` and never
//! emits `Event::Paste` there: Windows Terminal and conhost type a paste into
//! the console as one key event per character, so each pasted line break would
//! submit a prompt of its own. A burst of key events no person types (three
//! within [`BURST_WINDOW`]) is therefore read as a paste and handed on as one
//! `Event::Paste` once the keys stop for [`BURST_SILENCE`], the same event a
//! terminal with bracketed paste produces. Keys that may open a burst are held
//! for at most [`BURST_WINDOW`] before they reach the session as typed.
//!
//! Silence is measured from when the session looked for more input and found
//! none, never between two reads: a repaint can keep the session from reading
//! while the rest of a paste already waits in the console, so the session
//! drains what is waiting before it calls [`PasteBurst::expire`]. A key read
//! into an open burst extends it, unless the session found the console empty
//! since the burst's last key and the key came [`BURST_WINDOW`] or more after
//! it: a console paste is waiting in full, while keys typed during a stall
//! that opened a burst keep arriving at a person's pace and must type again.
//!
//! Other platforms receive bracketed pastes, so there the detector is off and
//! every event passes through in order, untouched.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use super::shortcuts::{accepts_key_event, typed_key};

#[cfg(test)]
mod paste_burst_tests;

/// How close three key events must fall to open a burst. A person reaches
/// about 20 ms between keys at best, a console paste a few microseconds.
pub(super) const BURST_WINDOW: Duration = Duration::from_millis(8);

/// The silence that closes a burst, long enough to bridge the chunks a
/// terminal splits a large paste into.
pub(super) const BURST_SILENCE: Duration = Duration::from_millis(30);

/// The key events that open a burst before it is one.
const BURST_KEYS: usize = 3;

pub(super) struct PasteBurst {
    enabled: bool,
    /// Keys that may still open a burst, with when each arrived.
    held: Vec<(KeyEvent, Instant)>,
    /// The text of the open burst and when its last key arrived.
    burst: Option<(String, Instant)>,
    /// Whether the session found the console empty since the burst's last key.
    drained: bool,
    /// Events released to the session, in arrival order.
    ready: VecDeque<Event>,
}

impl PasteBurst {
    /// A detector for the running platform: on for Windows only.
    pub(super) fn for_platform() -> Self {
        Self::new(cfg!(windows))
    }

    pub(super) const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            held: Vec::new(),
            burst: None,
            drained: false,
            ready: VecDeque::new(),
        }
    }

    /// Takes one terminal event that arrived at `now`.
    pub(super) fn push(&mut self, event: Event, now: Instant) {
        if !self.enabled {
            self.ready.push_back(event);
            return;
        }
        self.release_stale_keys(now);
        let key = match event {
            Event::Key(key) if accepts_key_event(key.kind) => key,
            // A key release is ignored by the session anyway, and a console
            // paste interleaves one after every key it types.
            Event::Key(_) if self.is_collecting() => return,
            event => {
                self.release();
                self.ready.push_back(event);
                return;
            }
        };
        let mut text = String::new();
        if !extend_burst(&mut text, key) {
            self.release();
            self.ready.push_back(Event::Key(key));
            return;
        }
        if self.drained
            && self
                .burst
                .as_ref()
                .is_some_and(|(_, last)| now.saturating_duration_since(*last) >= BURST_WINDOW)
        {
            self.release();
        }
        if let Some((burst, last)) = &mut self.burst {
            burst.push_str(&text);
            *last = now;
            self.drained = false;
            return;
        }
        self.held.push((key, now));
        if self.held.len() >= BURST_KEYS {
            let mut opened = String::new();
            for (key, _) in self.held.drain(..) {
                extend_burst(&mut opened, key);
            }
            self.burst = Some((opened, now));
        }
    }

    /// Releases what has waited long enough at `now`: a burst silent for
    /// [`BURST_SILENCE`] as one paste, and held keys older than
    /// [`BURST_WINDOW`] as typed. The caller has just found no more input
    /// waiting.
    pub(super) fn expire(&mut self, now: Instant) {
        if let Some((_, last)) = &self.burst
            && now.saturating_duration_since(*last) >= BURST_SILENCE
        {
            self.release();
        }
        self.release_stale_keys(now);
    }

    fn release_stale_keys(&mut self, now: Instant) {
        let stale = self
            .held
            .iter()
            .take_while(|(_, at)| now.saturating_duration_since(*at) >= BURST_WINDOW)
            .count();
        self.ready
            .extend(self.held.drain(..stale).map(|(key, _)| Event::Key(key)));
    }

    /// Records that the session looked for more input and found none.
    pub(super) fn input_drained(&mut self) {
        self.drained = self.burst.is_some();
    }

    /// When [`Self::expire`] next has something to release.
    pub(super) fn deadline(&self) -> Option<Instant> {
        match (&self.burst, self.held.first()) {
            (Some((_, last)), _) => Some(*last + BURST_SILENCE),
            (None, Some((_, at))) => Some(*at + BURST_WINDOW),
            (None, None) => None,
        }
    }

    /// Whether keys are held or a burst is open, so more input already
    /// waiting belongs with them rather than behind a repaint.
    pub(super) fn is_collecting(&self) -> bool {
        self.burst.is_some() || !self.held.is_empty()
    }

    /// The next event the session handles.
    pub(super) fn next_ready(&mut self) -> Option<Event> {
        self.ready.pop_front()
    }

    /// Releases the open burst as a paste and the held keys as typed.
    fn release(&mut self) {
        if let Some((text, _)) = self.burst.take() {
            self.ready.push_back(Event::Paste(text));
        }
        self.drained = false;
        self.ready
            .extend(self.held.drain(..).map(|(key, _)| Event::Key(key)));
    }
}

/// Appends what `key` contributes to a paste, or answers `false` when the
/// key cannot belong to one.
///
/// Enter is a line break and Tab a tab, so neither submits nor completes from
/// inside a paste. Esc contributes nothing: a pasted escape character must not
/// cancel the composer, and painting one would corrupt the terminal.
fn extend_burst(text: &mut String, key: KeyEvent) -> bool {
    let key = typed_key(key);
    let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
    match key.code {
        KeyCode::Char(character) if plain => text.push(character),
        KeyCode::Enter if plain => text.push('\n'),
        KeyCode::Tab if plain => text.push('\t'),
        KeyCode::Esc if key.modifiers.is_empty() => {}
        _ => return false,
    }
    true
}
