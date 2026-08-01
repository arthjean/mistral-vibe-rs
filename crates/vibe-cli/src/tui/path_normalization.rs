use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use tokio::sync::mpsc::{self, Receiver};

use super::attachments::normalize_pasted_text;
use super::chat_input::{EditorSnapshot, InputEffect, InputEvent};

pub(super) struct PathNormalizationManager {
    state: Arc<(Mutex<WorkerState>, Condvar)>,
    results: Receiver<NormalizationResult>,
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
        let (results_tx, results) = mpsc::channel(1);
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
            InputEffect::NormalizePastedPath { text, snapshot } => {
                Some(NormalizationRequest::Paste {
                    text: text.clone(),
                    snapshot: snapshot.clone(),
                })
            }
            InputEffect::NormalizeCurrentText { snapshot } => Some(NormalizationRequest::Current {
                snapshot: snapshot.clone(),
            }),
            _ => None,
        });
        let Some(request) = request else {
            return Ok(());
        };
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| "path normalization generation exhausted".to_owned())?;
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
        loop {
            let result = self.results.recv().await?;
            if let Some(event) = self.accept_result(result) {
                return Some(event);
            }
        }
    }

    fn accept_result(&mut self, result: NormalizationResult) -> Option<InputEvent> {
        if result.generation != self.generation {
            return None;
        }
        self.settled_generation = result.generation;
        Some(result.event)
    }

    pub fn shutdown(&mut self) {
        self.results.close();
        let (state, ready) = self.state.as_ref();
        if let Ok(mut state) = state.lock() {
            state.shutdown = true;
            state.pending = None;
            ready.notify_one();
        }
        // Filesystem metadata calls can block indefinitely on remote or FUSE
        // mounts. Dropping the handle detaches the worker so TUI shutdown stays
        // bounded; the shared shutdown flag lets it exit if the call returns.
        drop(self.worker.take());
    }
}

impl Drop for PathNormalizationManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum NormalizationRequest {
    Paste {
        text: String,
        snapshot: EditorSnapshot,
    },
    Current {
        snapshot: EditorSnapshot,
    },
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
            Self::Paste { text, snapshot } => InputEvent::PasteNormalized {
                snapshot,
                text: normalize_pasted_text(&text),
            },
            Self::Current { snapshot } => {
                let text = normalize_pasted_text(&snapshot.text);
                InputEvent::TextNormalized { snapshot, text }
            }
        }
    }
}

fn worker_loop(
    shared: &Arc<(Mutex<WorkerState>, Condvar)>,
    results: &mpsc::Sender<NormalizationResult>,
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
        if results.blocking_send(result).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(text: &str) -> EditorSnapshot {
        EditorSnapshot {
            text: text.to_owned(),
            cursor: text.chars().count(),
            selection: None,
        }
    }

    #[test]
    fn only_the_current_generation_can_settle_normalization() {
        let (_sender, results) = mpsc::channel(1);
        let mut manager = PathNormalizationManager {
            state: Arc::new((Mutex::new(WorkerState::default()), Condvar::new())),
            results,
            worker: None,
            generation: 2,
            settled_generation: 0,
        };
        let event = |generation| NormalizationResult {
            generation,
            event: InputEvent::TextNormalized {
                snapshot: snapshot("/tmp/image.png"),
                text: "@/tmp/image.png".to_owned(),
            },
        };

        assert!(manager.accept_result(event(1)).is_none());
        assert!(manager.has_pending());
        assert!(manager.accept_result(event(2)).is_some());
        assert!(!manager.has_pending());
    }
}
