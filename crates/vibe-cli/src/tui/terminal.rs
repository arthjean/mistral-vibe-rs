use std::io::{Stdout, Write};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalStep {
    RawMode,
    AlternateScreen,
    MouseCapture,
    BracketedPaste,
    FocusReporting,
    CursorHidden,
}

impl TerminalStep {
    const ENTER_ORDER: [Self; 6] = [
        Self::RawMode,
        Self::AlternateScreen,
        Self::MouseCapture,
        Self::BracketedPaste,
        Self::FocusReporting,
        Self::CursorHidden,
    ];
}

pub trait TerminalOps {
    fn enter(&mut self, step: TerminalStep) -> Result<(), String>;
    fn leave(&mut self, step: TerminalStep) -> Result<(), String>;
}

pub struct CrosstermOps<W> {
    writer: W,
}

impl CrosstermOps<Stdout> {
    #[must_use]
    pub fn stdout() -> Self {
        Self {
            writer: std::io::stdout(),
        }
    }
}

impl<W> CrosstermOps<W> {
    #[must_use]
    pub fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W> TerminalOps for CrosstermOps<W>
where
    W: Write,
{
    fn enter(&mut self, step: TerminalStep) -> Result<(), String> {
        let result = match step {
            TerminalStep::RawMode => enable_raw_mode(),
            TerminalStep::AlternateScreen => execute!(self.writer, EnterAlternateScreen),
            TerminalStep::MouseCapture => execute!(self.writer, EnableMouseCapture),
            TerminalStep::BracketedPaste => execute!(self.writer, EnableBracketedPaste),
            TerminalStep::FocusReporting => execute!(self.writer, EnableFocusChange),
            TerminalStep::CursorHidden => execute!(self.writer, Hide),
        };
        result.map_err(|error| error.to_string())
    }

    fn leave(&mut self, step: TerminalStep) -> Result<(), String> {
        let result = match step {
            TerminalStep::RawMode => disable_raw_mode(),
            TerminalStep::AlternateScreen => execute!(self.writer, LeaveAlternateScreen),
            TerminalStep::MouseCapture => execute!(self.writer, DisableMouseCapture),
            TerminalStep::BracketedPaste => execute!(self.writer, DisableBracketedPaste),
            TerminalStep::FocusReporting => execute!(self.writer, DisableFocusChange),
            TerminalStep::CursorHidden => execute!(self.writer, Show),
        };
        result.map_err(|error| error.to_string())
    }
}

pub struct TerminalGuard<O>
where
    O: TerminalOps,
{
    ops: O,
    active: Vec<TerminalStep>,
    restore_errors: Vec<String>,
}

impl<O> TerminalGuard<O>
where
    O: TerminalOps,
{
    pub fn enter(mut ops: O) -> Result<Self, TerminalEnterError> {
        let mut active = Vec::new();
        for step in TerminalStep::ENTER_ORDER {
            if let Err(message) = ops.enter(step) {
                let cleanup = restore_steps(&mut ops, &mut active);
                return Err(TerminalEnterError {
                    step,
                    message,
                    cleanup,
                });
            }
            active.push(step);
        }
        Ok(Self {
            ops,
            active,
            restore_errors: Vec::new(),
        })
    }

    pub fn restore(&mut self) -> Result<(), TerminalRestoreError> {
        self.restore_errors
            .extend(restore_steps(&mut self.ops, &mut self.active));
        if self.restore_errors.is_empty() {
            Ok(())
        } else {
            Err(TerminalRestoreError {
                failures: self.restore_errors.clone(),
            })
        }
    }

    pub fn resume(&mut self) -> Result<(), TerminalEnterError> {
        if !self.active.is_empty() {
            return Ok(());
        }
        for step in TerminalStep::ENTER_ORDER {
            if let Err(message) = self.ops.enter(step) {
                let cleanup = restore_steps(&mut self.ops, &mut self.active);
                return Err(TerminalEnterError {
                    step,
                    message,
                    cleanup,
                });
            }
            self.active.push(step);
        }
        Ok(())
    }

    #[must_use]
    pub fn is_restored(&self) -> bool {
        self.active.is_empty()
    }
}

impl<O> Drop for TerminalGuard<O>
where
    O: TerminalOps,
{
    fn drop(&mut self) {
        self.restore_errors
            .extend(restore_steps(&mut self.ops, &mut self.active));
    }
}

fn restore_steps<O>(ops: &mut O, active: &mut Vec<TerminalStep>) -> Vec<String>
where
    O: TerminalOps,
{
    let mut failures = Vec::new();
    while let Some(step) = active.pop() {
        if let Err(message) = ops.leave(step) {
            failures.push(format!("{step:?}: {message}"));
        }
    }
    failures
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("terminal setup failed at {step:?}: {message}")]
pub struct TerminalEnterError {
    pub step: TerminalStep,
    pub message: String,
    pub cleanup: Vec<String>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("terminal restoration failed: {failures:?}")]
pub struct TerminalRestoreError {
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    NormalExit,
    Panic,
    Cancellation,
    Sigint,
    TerminalLoss,
    NestedError,
    AdapterFailure,
    Timeout,
    TestCancellation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCandidate {
    pub name: &'static str,
    pub immutable_snapshots: bool,
    pub input_events: bool,
    pub resize: bool,
    pub unicode: bool,
    pub mouse: bool,
    pub clipboard: &'static str,
    pub linux: bool,
    pub macos: bool,
    pub windows: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalStackDecision {
    pub selected: TerminalCandidate,
    pub alternative: TerminalCandidate,
    pub rationale: &'static str,
    pub native_test_needs: &'static [&'static str],
}

#[must_use]
pub fn terminal_stack_decision() -> TerminalStackDecision {
    TerminalStackDecision {
        selected: TerminalCandidate {
            name: "ratatui-0.29.0+crossterm-0.28.1",
            immutable_snapshots: true,
            input_events: true,
            resize: true,
            unicode: true,
            mouse: true,
            clipboard: "injected port; terminal support is capability-dependent",
            linux: true,
            macos: true,
            windows: true,
        },
        alternative: TerminalCandidate {
            name: "direct-crossterm-0.28.1",
            immutable_snapshots: false,
            input_events: true,
            resize: true,
            unicode: true,
            mouse: true,
            clipboard: "injected port; terminal support is capability-dependent",
            linux: true,
            macos: true,
            windows: true,
        },
        rationale: "Ratatui supplies a deterministic TestBackend and layout buffer while retaining crossterm's cross-platform event and restoration primitives.",
        native_test_needs: &[
            "SIGINT and terminal-loss restoration",
            "Windows legacy console input and bracketed-paste fallback",
            "macOS and Linux clipboard integration",
            "terminal image protocol detection",
        ],
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone)]
    struct RecordingOps {
        transcript: Arc<Mutex<Vec<(bool, TerminalStep)>>>,
        fail_enter: Option<TerminalStep>,
        fail_leave: Option<TerminalStep>,
    }

    impl TerminalOps for RecordingOps {
        fn enter(&mut self, step: TerminalStep) -> Result<(), String> {
            self.transcript
                .lock()
                .expect("terminal transcript")
                .push((true, step));
            if self.fail_enter == Some(step) {
                Err("injected enter failure".to_owned())
            } else {
                Ok(())
            }
        }

        fn leave(&mut self, step: TerminalStep) -> Result<(), String> {
            self.transcript
                .lock()
                .expect("terminal transcript")
                .push((false, step));
            if self.fail_leave == Some(step) {
                Err("injected restore failure".to_owned())
            } else {
                Ok(())
            }
        }
    }

    fn ops(
        fail_enter: Option<TerminalStep>,
        fail_leave: Option<TerminalStep>,
    ) -> (RecordingOps, Arc<Mutex<Vec<(bool, TerminalStep)>>>) {
        let transcript = Arc::new(Mutex::new(Vec::new()));
        (
            RecordingOps {
                transcript: transcript.clone(),
                fail_enter,
                fail_leave,
            },
            transcript,
        )
    }

    #[test]
    fn every_shutdown_path_restores_in_reverse_order() {
        for reason in [
            ShutdownReason::NormalExit,
            ShutdownReason::Panic,
            ShutdownReason::Cancellation,
            ShutdownReason::Sigint,
            ShutdownReason::TerminalLoss,
            ShutdownReason::NestedError,
            ShutdownReason::AdapterFailure,
            ShutdownReason::Timeout,
            ShutdownReason::TestCancellation,
        ] {
            let (ops, transcript) = ops(None, None);
            {
                let _guard = TerminalGuard::enter(ops).expect("terminal enters");
                let _ = reason;
            }
            let transcript = transcript.lock().expect("terminal transcript");
            assert_eq!(transcript.len(), 12);
            assert_eq!(
                &transcript[6..],
                &[
                    (false, TerminalStep::CursorHidden),
                    (false, TerminalStep::FocusReporting),
                    (false, TerminalStep::BracketedPaste),
                    (false, TerminalStep::MouseCapture),
                    (false, TerminalStep::AlternateScreen),
                    (false, TerminalStep::RawMode),
                ]
            );
        }
    }

    #[test]
    fn partial_setup_rolls_back_every_completed_step() {
        let (ops, transcript) = ops(Some(TerminalStep::MouseCapture), None);
        let error = TerminalGuard::enter(ops)
            .err()
            .expect("injected setup failure");
        assert_eq!(error.step, TerminalStep::MouseCapture);
        assert!(error.cleanup.is_empty());
        assert_eq!(
            *transcript.lock().expect("terminal transcript"),
            vec![
                (true, TerminalStep::RawMode),
                (true, TerminalStep::AlternateScreen),
                (true, TerminalStep::MouseCapture),
                (false, TerminalStep::AlternateScreen),
                (false, TerminalStep::RawMode),
            ]
        );
    }

    #[test]
    fn restore_attempts_all_steps_and_is_idempotent_after_error() {
        let (ops, transcript) = ops(None, Some(TerminalStep::MouseCapture));
        let mut guard = TerminalGuard::enter(ops).expect("terminal enters");
        let error = guard.restore().expect_err("injected restore failure");
        assert_eq!(error.failures.len(), 1);
        assert!(guard.is_restored());
        assert!(guard.restore().is_err());
        assert_eq!(transcript.lock().expect("terminal transcript").len(), 12);
    }

    #[test]
    fn external_program_suspend_and_resume_reenter_the_full_stack() {
        let (ops, transcript) = ops(None, None);
        let mut guard = TerminalGuard::enter(ops).expect("terminal enters");
        guard.restore().expect("terminal suspends");
        assert!(guard.is_restored());
        guard.resume().expect("terminal resumes");
        assert!(!guard.is_restored());
        drop(guard);

        let transcript = transcript.lock().expect("terminal transcript");
        assert_eq!(transcript.len(), 24);
        assert_eq!(&transcript[..6], &transcript[12..18]);
        assert_eq!(&transcript[6..12], &transcript[18..24]);
    }

    #[test]
    fn panic_unwinding_drops_the_guard_and_restores_terminal() {
        let (ops, transcript) = ops(None, None);
        let result = std::panic::catch_unwind(|| {
            let _guard = TerminalGuard::enter(ops).expect("terminal enters");
            panic!("injected panic");
        });
        assert!(result.is_err());
        assert_eq!(transcript.lock().expect("terminal transcript").len(), 12);
    }

    #[test]
    fn selected_stack_is_testable_and_cross_platform() {
        let decision = terminal_stack_decision();
        assert!(decision.selected.immutable_snapshots);
        assert!(decision.selected.linux && decision.selected.macos && decision.selected.windows);
        assert!(!decision.alternative.immutable_snapshots);
        assert!(!decision.native_test_needs.is_empty());
    }
}
