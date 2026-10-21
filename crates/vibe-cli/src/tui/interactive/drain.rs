//! Draining terminal input that is already queued: the stale keys and
//! interrupts a failed launch must not let answer its acknowledgment prompt,
//! and the burst of keys one console paste types.

use std::time::{Duration, Instant};

use crossterm::event::Event;
use futures_util::StreamExt;

use super::super::CliError;
use super::super::paste_burst::PasteBurst;

/// A launch that failed before it mounted paints its error and then waits for
/// one acknowledgment. Input queued before the failure would answer that prompt
/// for the operator, so it is drained first.
const MAX_FATAL_INPUT_DRAIN: usize = 256;
pub(super) const INPUT_ENDED: &str =
    "Terminal input ended; recoverable session state was preserved";
/// How long a drain waits for the next key of a burst: far below the burst
/// window, far above the gap between two keys of one console paste.
const NEXT_KEY_WAIT: Duration = Duration::from_millis(2);
const MAX_READY_INTERRUPTS_TO_DRAIN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadyInputDrain {
    Empty,
    Saturated,
    Closed,
}

pub(super) fn drain_ready_terminal_events<S, E>(events: &mut S) -> Result<ReadyInputDrain, CliError>
where
    S: futures_util::Stream<Item = Result<Event, E>> + Unpin,
    E: std::fmt::Display,
{
    use futures_util::FutureExt;
    for _ in 0..MAX_FATAL_INPUT_DRAIN {
        match events.next().now_or_never() {
            Some(Some(Ok(_))) => {}
            Some(Some(Err(error))) => return Err(CliError::Terminal(error.to_string())),
            Some(None) => return Ok(ReadyInputDrain::Closed),
            None => return Ok(ReadyInputDrain::Empty),
        }
    }
    Ok(ReadyInputDrain::Saturated)
}

/// Feeds the paste detector the events that arrive while it collects, so a
/// console paste is read as it arrives rather than one pass of the event loop
/// per key. Answers the transport failure that ended the drain, if one did.
///
/// The console host types a paste a key at a time, so the drain waits up to
/// [`NEXT_KEY_WAIT`] for the next one. Readiness is asked of crossterm before
/// the stream is polled: a stream polled empty here would hand its wake-up to
/// a waker nothing listens to.
pub(super) fn drain_ready_into<S>(events: &mut S, burst: &mut PasteBurst) -> Option<String>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    use futures_util::FutureExt;
    while burst.is_collecting() && crossterm::event::poll(NEXT_KEY_WAIT).unwrap_or(false) {
        match events.next().now_or_never() {
            Some(Some(Ok(event))) => burst.push(event, Instant::now()),
            Some(Some(Err(error))) => return Some(error.to_string()),
            Some(None) => return Some(INPUT_ENDED.to_owned()),
            None => break,
        }
    }
    None
}

pub(super) fn drain_ready_interrupts<F, E, R>(
    interrupt: &mut std::pin::Pin<Box<F>>,
    mut recreate: R,
) -> Result<usize, CliError>
where
    F: std::future::Future<Output = Result<(), E>>,
    E: std::fmt::Display,
    R: FnMut() -> F,
{
    use futures_util::FutureExt;
    let mut drained = 0;
    while drained < MAX_READY_INTERRUPTS_TO_DRAIN {
        let Some(signal) = interrupt.as_mut().now_or_never() else {
            return Ok(drained);
        };
        signal.map_err(|error| CliError::Terminal(error.to_string()))?;
        drained += 1;
        interrupt.set(recreate());
    }
    Ok(drained)
}
