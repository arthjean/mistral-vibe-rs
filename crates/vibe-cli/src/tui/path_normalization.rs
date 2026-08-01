use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use tokio::sync::mpsc::{self, UnboundedReceiver};

use super::attachments::normalize_pasted_text;
use super::chat_input::{InputEffect, InputEvent};

pub(super) struct PathNormalizationManager {
    state: Arc<(Mutex<WorkerState>, Condvar)>,
    results: UnboundedReceiver<NormalizationResult>,
    worker: Option<JoinHandle<()>>,
    generation: u64,
    settled_generation: u64,
}

#[derive(Default)]
struct WorkerState {
    pending: Option<NormalizationWork>,
    shutdown: bool,
}

impl PathNormalizationManager {
    pub fn new() -> Result<Self, String> {
        let state = Arc::new((Mutex::new(WorkerState::default()), Condvar::new()));
        let thread_state = Arc::clone(&state);
        let (results_tx, results) = mpsc::unbounded_channel();
        let worker = std::thread::Builder::new()
            .name("vibe-path-normalization".to_owned())
            .spawn(move || worker_loop(&thread_state, &results_tx))
            .map_err(|error| format!("path normalization worker failed to start: {error}"))?;
        Ok(Self {
            state,
            results,
            worker: Some(worker),
            generation: 0,
            settled_generation: 0,
        })
    }

    pub fn schedule_effects(&mut self, effects: &[InputEffect]) -> Result<(), String> {
        let request = effects.iter().rev().find_map(|effect| match effect {
            InputEffect::NormalizePastedPath { text, document } => {
                Some(NormalizationRequest::Paste {
                    text: text.clone(),
                    document: document.clone(),
                })
            }
            InputEffect::NormalizeCurrentText { text } => {
                Some(NormalizationRequest::Current { text: text.clone() })
            }
            _ => None,
        });
        let Some(request) = request else {
            return Ok(());
        };
        self.generation = self.generation.saturating_add(1);
        let work = NormalizationWork {
            generation: self.generation,
            request,
        };
        let (state, ready) = self.state.as_ref();
        let mut state = state
            .lock()
            .map_err(|_| "path normalization worker lock is poisoned".to_owned())?;
        state.pending = Some(work);
        ready.notify_one();
        Ok(())
    }

    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.settled_generation < self.generation
    }

    pub async fn next_event(&mut self) -> Option<InputEvent> {
        let result = self.results.recv().await?;
        self.settled_generation = self.settled_generation.max(result.generation);
        Some(result.event)
    }

    pub fn shutdown(&mut self) {
        let (state, ready) = self.state.as_ref();
        if let Ok(mut state) = state.lock() {
            state.shutdown = true;
            state.pending = None;
            ready.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for PathNormalizationManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum NormalizationRequest {
    Paste { text: String, document: String },
    Current { text: String },
}

struct NormalizationWork {
    generation: u64,
    request: NormalizationRequest,
}

struct NormalizationResult {
    generation: u64,
    event: InputEvent,
}

impl NormalizationRequest {
    fn resolve(self) -> InputEvent {
        match self {
            Self::Paste { text, document } => InputEvent::PasteNormalized {
                document,
                text: normalize_pasted_text(&text),
            },
            Self::Current { text } => InputEvent::TextNormalized {
                original: text.clone(),
                text: normalize_pasted_text(&text),
            },
        }
    }
}

fn worker_loop(
    shared: &Arc<(Mutex<WorkerState>, Condvar)>,
    results: &mpsc::UnboundedSender<NormalizationResult>,
) {
    loop {
        let work = {
            let (state, ready) = shared.as_ref();
            let Ok(mut state) = state.lock() else {
                return;
            };
            while state.pending.is_none() && !state.shutdown {
                let Ok(next) = ready.wait(state) else {
                    return;
                };
                state = next;
            }
            if state.shutdown {
                return;
            }
            state.pending.take()
        };
        let Some(work) = work else {
            continue;
        };
        let result = NormalizationResult {
            generation: work.generation,
            event: work.request.resolve(),
        };
        if results.send(result).is_err() {
            return;
        }
    }
}
