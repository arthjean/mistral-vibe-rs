//! Loop-level tests: the stale input a failed launch must never let answer its
//! own acknowledgment prompt.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use crossterm::event::{KeyCode, KeyModifiers};
use futures_util::FutureExt;

use super::*;

struct PanicAfterCompletionInterrupt {
    ready: bool,
    completed: bool,
}

impl Future for PanicAfterCompletionInterrupt {
    type Output = Result<(), std::io::Error>;

    fn poll(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(!self.completed, "completed interrupt was polled again");
        if self.ready {
            self.completed = true;
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

#[test]
fn stale_inputs_are_drained_before_fatal_acknowledgment_arms() {
    let queued = Arc::new(Mutex::new(VecDeque::from([Ok::<_, std::io::Error>(
        Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
    )])));
    let polled = Arc::clone(&queued);
    let mut events = futures_util::stream::poll_fn(move |_| {
        polled
            .lock()
            .expect("event queue lock")
            .pop_front()
            .map_or(Poll::Pending, |event| Poll::Ready(Some(event)))
    });
    let mut startup = startup::MountedStartup::FatalPendingRender(CliError::Terminal(
        "initialization failed".to_owned(),
    ));

    assert_eq!(
        drain_ready_terminal_events(&mut events).expect("stale input drains"),
        ReadyInputDrain::Empty
    );
    let mut stale_interrupt = Box::pin(PanicAfterCompletionInterrupt {
        ready: true,
        completed: false,
    });
    let mut replacements = VecDeque::from([true, false]);
    assert_eq!(
        drain_ready_interrupts(&mut stale_interrupt, || {
            PanicAfterCompletionInterrupt {
                ready: replacements.pop_front().expect("bounded replacement"),
                completed: false,
            }
        })
        .expect("stale interrupt drains"),
        2
    );
    assert!(stale_interrupt.as_mut().now_or_never().is_none());
    assert!(!startup.is_awaiting_fatal_key());
    startup.arm_fatal_acknowledgment();
    assert!(startup.is_awaiting_fatal_key());
    assert!(events.next().now_or_never().is_none());

    queued
        .lock()
        .expect("event queue lock")
        .push_back(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::NONE,
        ))));
    assert!(matches!(
        events.next().now_or_never(),
        Some(Some(Ok(Event::Key(KeyEvent {
            code: KeyCode::Char('n'),
            ..
        }))))
    ));
}
