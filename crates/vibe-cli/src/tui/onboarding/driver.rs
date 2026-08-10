//! The interactive loop that hosts the onboarding model: it owns the
//! terminal, feeds keys and timers into the machine, and executes the
//! machine's effects with real services, chiefly the sign-in worker built
//! from the working provider entry.

use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{mpsc, oneshot};
use toml::Table;
use vibe_core::auth::{
    HttpSignInGateway, SignInErrorCode, SignInEvent, SignInService, SystemSignInRuntime,
};

use super::model::{
    KeyPress, ModelEffect, ModelEvent, OnboardingModel, OnboardingOutcome, OnboardingPorts,
    SIGN_IN_URL_HELP_DELAY_SECONDS, SUCCESS_EXIT_DELAY_SECONDS,
};
use super::render;
use crate::CliError;
use crate::tui::clipboard::{SystemClipboard, SystemClipboardPort};
use crate::tui::terminal::{CrosstermOps, TerminalGuard};

/// What one full drive of the screens produced.
pub struct OnboardingRun {
    pub outcome: OnboardingOutcome,
    pub selected_theme: &'static str,
}

/// A running sign-in worker: dropping the sender cancels it, and the worker
/// closes its service on either path out.
struct SignInWorker {
    cancel: oneshot::Sender<()>,
}

impl SignInWorker {
    fn cancel(self) {
        let _ = self.cancel.send(());
    }
}

fn terminal_error(error: impl std::fmt::Display) -> CliError {
    CliError::Terminal(error.to_string())
}

/// Runs the screens until the machine exits, then restores the terminal.
pub async fn run_flow(
    context: super::context::OnboardingContext,
    ports: &mut dyn OnboardingPorts,
) -> Result<OnboardingRun, CliError> {
    let mut model = OnboardingModel::new(context);
    let mut guard = TerminalGuard::enter(CrosstermOps::stdout()).map_err(terminal_error)?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend).map_err(terminal_error)?;
    let mut events = EventStream::new();
    let (event_sender, mut forwarded) = mpsc::unbounded_channel::<ModelEvent>();
    let mut animation = render::Animation::default();
    let mut worker: Option<SignInWorker> = None;
    let mut ticker = tokio::time::interval(Duration::from_millis(40));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut needs_redraw = true;
    let outcome = 'flow: loop {
        if needs_redraw {
            terminal
                .draw(|frame| render::draw(frame, &model, &animation))
                .map_err(terminal_error)?;
            needs_redraw = false;
        }
        let event = tokio::select! {
            _ = ticker.tick() => {
                let typing_finished = animation.advance();
                // A tick only redraws a screen that is animating: the welcome
                // typing and the gradient it keeps cycling, and the sign-in
                // confirm step's animated detail. Idle screens repaint on
                // input alone.
                needs_redraw = render::animates(&model);
                (typing_finished && !model.typing_done())
                    .then_some(ModelEvent::WelcomeTypingFinished)
            }
            terminal_event = events.next() => match terminal_event {
                Some(Ok(Event::Key(key)))
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    key_press(key.code, key.modifiers).map(ModelEvent::Key)
                }
                Some(Ok(Event::Resize(_, _))) => {
                    needs_redraw = true;
                    None
                }
                Some(Ok(_)) => None,
                Some(Err(error)) => break 'flow Err(terminal_error(error)),
                None => break 'flow Ok(OnboardingOutcome::Cancelled),
            },
            event = forwarded.recv() => event,
        };
        let Some(event) = event else {
            continue;
        };
        needs_redraw = true;
        for effect in model.handle(event, ports) {
            match effect {
                ModelEffect::StartSignIn { attempt } => {
                    worker = Some(spawn_sign_in(attempt, model.provider(), &event_sender));
                }
                ModelEffect::CancelSignIn => {
                    if let Some(worker) = worker.take() {
                        worker.cancel();
                    }
                }
                ModelEffect::CopyUrl { url } => {
                    let _ = SystemClipboardPort::copy_text(&SystemClipboard, &url);
                }
                ModelEffect::ScheduleUrlHelp { attempt } => {
                    let sender = event_sender.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs_f64(SIGN_IN_URL_HELP_DELAY_SECONDS))
                            .await;
                        let _ = sender.send(ModelEvent::UrlHelpElapsed { attempt });
                    });
                }
                ModelEffect::ScheduleSuccessExit => {
                    let sender = event_sender.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs_f64(SUCCESS_EXIT_DELAY_SECONDS))
                            .await;
                        let _ = sender.send(ModelEvent::SuccessDelayElapsed);
                    });
                }
                ModelEffect::Exit(outcome) => break 'flow Ok(outcome),
            }
        }
    };
    if let Some(worker) = worker.take() {
        worker.cancel();
    }
    drop(terminal);
    guard.restore().map_err(terminal_error)?;
    Ok(OnboardingRun {
        outcome: outcome?,
        selected_theme: model.selected_theme(),
    })
}

/// One terminal key as the machine's vocabulary; anything else is inert.
fn key_press(code: KeyCode, modifiers: KeyModifiers) -> Option<KeyPress> {
    if modifiers.contains(KeyModifiers::CONTROL) {
        return matches!(code, KeyCode::Char('c')).then_some(KeyPress::CtrlC);
    }
    match code {
        KeyCode::Enter => Some(KeyPress::Enter),
        KeyCode::Esc => Some(KeyPress::Escape),
        KeyCode::Up => Some(KeyPress::Up),
        KeyCode::Down => Some(KeyPress::Down),
        KeyCode::Backspace => Some(KeyPress::Backspace),
        KeyCode::Char(character) => Some(KeyPress::Char(character)),
        _ => None,
    }
}

/// Spawns the sign-in worker for `attempt`. The worker forwards service
/// events tagged with its attempt number, closes the service whether it
/// finished or was cancelled, and reports its end afterward, matching the
/// reference worker whose `finally` closes before the outcome is handled.
fn spawn_sign_in(
    attempt: u64,
    provider: &Table,
    events: &mpsc::UnboundedSender<ModelEvent>,
) -> SignInWorker {
    let (cancel_sender, cancel_receiver) = oneshot::channel::<()>();
    let events = events.clone();
    let gateway = HttpSignInGateway::for_provider(provider);
    tokio::spawn(async move {
        let Some(gateway) = gateway else {
            let _ = events.send(ModelEvent::SignInFailed {
                attempt,
                code: Some(SignInErrorCode::StartFailed),
                message: SignInErrorCode::StartFailed.message().to_owned(),
            });
            return;
        };
        let mut service = SignInService::new(gateway, SystemSignInRuntime);
        let event_sender = events.clone();
        let mut sink = move |event: SignInEvent| {
            let mapped = match event {
                SignInEvent::AttemptStarted { sign_in_url, .. } => ModelEvent::SignInStarted {
                    attempt,
                    sign_in_url,
                },
                SignInEvent::StatusChanged(status) => ModelEvent::SignInStatus { attempt, status },
            };
            let _ = event_sender.send(mapped);
        };
        let result = tokio::select! {
            result = service.authenticate(&mut sink) => Some(result),
            _ = cancel_receiver => None,
        };
        service.close().await;
        match result {
            Some(Ok(api_key)) => {
                let _ = events.send(ModelEvent::SignInCompleted { attempt, api_key });
            }
            Some(Err(error)) => {
                let _ = events.send(ModelEvent::SignInFailed {
                    attempt,
                    code: Some(error.code),
                    message: error.message().to_owned(),
                });
            }
            None => {}
        }
    });
    SignInWorker {
        cancel: cancel_sender,
    }
}
