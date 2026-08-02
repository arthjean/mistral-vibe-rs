//! Voice session orchestration across microphone capture and transcription.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use secrecy::SecretString;
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinError, JoinHandle};

use crate::tui::chat_input::InputEvent;

use super::realtime::{DRAIN_TIMEOUT, START_TIMEOUT, VoiceConfig, run_transcription};
use super::recorder::CpalAudioRecorder;
use super::{VoiceControl, VoiceSessionFactory};

const AUDIO_QUEUE_CAPACITY: usize = 32;
const MAX_RECORDING_DURATION: Duration = Duration::from_secs(300);

pub(super) struct ProductionVoiceSessionFactory {
    credential: SecretString,
    config: VoiceConfig,
}

impl ProductionVoiceSessionFactory {
    pub(super) fn new(credential: String, config: VoiceConfig) -> Self {
        Self {
            credential: SecretString::from(credential),
            config,
        }
    }
}

impl VoiceSessionFactory for ProductionVoiceSessionFactory {
    fn spawn(
        &self,
        generation: u64,
        updates: mpsc::Sender<InputEvent>,
        control: watch::Receiver<VoiceControl>,
    ) -> JoinHandle<()> {
        let credential = self.credential.clone();
        let config = self.config.clone();
        tokio::spawn(async move {
            run_voice_session(generation, updates, control, credential, config).await;
        })
    }
}

async fn run_voice_session(
    generation: u64,
    updates: mpsc::Sender<InputEvent>,
    mut control: watch::Receiver<VoiceControl>,
    credential: SecretString,
    config: VoiceConfig,
) {
    let (audio_tx, audio_rx) = mpsc::channel(AUDIO_QUEUE_CAPACITY);
    let start_updates = updates.clone();
    let requested_sample_rate = config.requested_sample_rate;
    let start = tokio::task::spawn_blocking(move || {
        CpalAudioRecorder::start(generation, requested_sample_rate, start_updates, audio_tx)
    });
    tokio::pin!(start);
    let recorder = tokio::select! {
        result = &mut start => match result {
            Ok(Ok(recorder)) => recorder,
            Ok(Err(error)) => {
                send_start_result(&updates, generation, Some(error)).await;
                return;
            }
            Err(error) => {
                send_start_result(
                    &updates,
                    generation,
                    Some(format!("Audio input worker failed: {error}")),
                ).await;
                return;
            }
        },
        _ = tokio::time::sleep(START_TIMEOUT) => {
            send_start_result(
                &updates,
                generation,
                Some("Audio input startup timed out".to_owned()),
            ).await;
            // `spawn_blocking` cannot be cancelled once CPAL has started. Keep
            // this session task alive until the worker returns so VoiceManager
            // retains it as retiring and never overlaps a second recorder.
            dispose_start_result(start.await).await;
            return;
        }
        changed = control.changed() => {
            if changed.is_err() || *control.borrow() == VoiceControl::Cancel {
                // See the timeout branch: waiting here is the resource-safety
                // barrier, while reducer cancellation has already restored UI.
                dispose_start_result(start.await).await;
                return;
            }
            match start.await {
                Ok(Ok(recorder)) => recorder,
                Ok(Err(error)) => {
                    send_start_result(&updates, generation, Some(error)).await;
                    return;
                }
                Err(error) => {
                    send_start_result(
                        &updates,
                        generation,
                        Some(format!("Audio input worker failed: {error}")),
                    ).await;
                    return;
                }
            }
        }
    };
    let pending_control = *control.borrow();
    if pending_control == VoiceControl::Cancel {
        dispose_recorder(recorder).await;
        return;
    }
    let mut config = config;
    config.requested_sample_rate = recorder.sample_rate();
    let recorder = Arc::new(Mutex::new(Some(recorder)));
    send_start_result(&updates, generation, None).await;

    let transcribe_updates = updates.clone();
    let mut transcription = tokio::spawn(async move {
        run_transcription(
            audio_rx,
            &credential,
            &config,
            generation,
            &transcribe_updates,
        )
        .await
    });
    let mut maximum = Box::pin(tokio::time::sleep(MAX_RECORDING_DURATION));

    if pending_control != VoiceControl::Stop {
        loop {
            tokio::select! {
                result = &mut transcription => {
                    drop_recorder(recorder.clone()).await;
                    send_transcription_result(&updates, generation, result).await;
                    return;
                }
                changed = control.changed() => {
                    if changed.is_err() || *control.borrow() == VoiceControl::Cancel {
                        drop_recorder(recorder.clone()).await;
                        transcription.abort();
                        let _ = transcription.await;
                        return;
                    }
                    if *control.borrow() == VoiceControl::Stop {
                        break;
                    }
                }
                _ = &mut maximum => break,
            }
        }
    }

    drop_recorder(recorder).await;
    let _ = updates
        .send(InputEvent::VoiceStopResolved {
            generation,
            error: None,
        })
        .await;
    let mut drain_timeout = Box::pin(tokio::time::sleep(DRAIN_TIMEOUT));
    loop {
        tokio::select! {
            result = &mut transcription => {
                send_transcription_result(&updates, generation, result).await;
                return;
            }
            changed = control.changed() => {
                if changed.is_err() || *control.borrow() == VoiceControl::Cancel {
                    transcription.abort();
                    let _ = transcription.await;
                    return;
                }
            }
            _ = &mut drain_timeout => {
                transcription.abort();
                let _ = transcription.await;
                let _ = updates
                    .send(InputEvent::VoiceStopResolved {
                        generation,
                        error: Some("Transcription timed out".to_owned()),
                    })
                    .await;
                return;
            }
        }
    }
}

pub(super) async fn send_transcription_result(
    updates: &mpsc::Sender<InputEvent>,
    generation: u64,
    result: Result<Result<(), String>, JoinError>,
) {
    let event = match result {
        Ok(Ok(())) => InputEvent::VoiceDone { generation },
        Ok(Err(error)) => InputEvent::VoiceStopResolved {
            generation,
            error: Some(error),
        },
        Err(error) => InputEvent::VoiceStopResolved {
            generation,
            error: Some(format!("Transcription worker failed: {error}")),
        },
    };
    let _ = updates.send(event).await;
}

async fn send_start_result(
    updates: &mpsc::Sender<InputEvent>,
    generation: u64,
    error: Option<String>,
) {
    let _ = updates
        .send(InputEvent::VoiceStartResolved { generation, error })
        .await;
}

async fn dispose_start_result(result: Result<Result<CpalAudioRecorder, String>, JoinError>) {
    if let Ok(Ok(recorder)) = result {
        dispose_recorder(recorder).await;
    }
}

async fn dispose_recorder(recorder: CpalAudioRecorder) {
    let _ = tokio::task::spawn_blocking(move || drop(recorder)).await;
}

async fn drop_recorder(recorder: Arc<Mutex<Option<CpalAudioRecorder>>>) {
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(mut recorder) = recorder.lock() {
            recorder.take();
        }
    })
    .await;
}
