//! Voice session orchestration across microphone capture and transcription.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use secrecy::SecretString;
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinError, JoinHandle};

use crate::tui::chat_input::InputEvent;

use super::realtime::{DRAIN_TIMEOUT, START_TIMEOUT, VoiceConfig, prepare_transcription};
use super::recorder::{AudioFailureSignal, CpalAudioRecorder};
use super::{VoiceControl, VoiceSessionFactory, VoiceSignal};

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
        signals: mpsc::Sender<VoiceSignal>,
        control: watch::Receiver<VoiceControl>,
    ) -> JoinHandle<()> {
        let credential = self.credential.clone();
        let config = self.config.clone();
        tokio::spawn(async move {
            run_voice_session(generation, updates, signals, control, credential, config).await;
        })
    }
}

async fn run_voice_session(
    generation: u64,
    updates: mpsc::Sender<InputEvent>,
    signals: mpsc::Sender<VoiceSignal>,
    mut control: watch::Receiver<VoiceControl>,
    credential: SecretString,
    config: VoiceConfig,
) {
    let (audio_tx, audio_rx) = mpsc::channel(AUDIO_QUEUE_CAPACITY);
    let (failures, failure_rx) = AudioFailureSignal::channel();
    let start_updates = updates.clone();
    let requested_sample_rate = config.requested_sample_rate;
    let start = tokio::task::spawn_blocking(move || {
        CpalAudioRecorder::prepare(
            generation,
            requested_sample_rate,
            start_updates,
            audio_tx,
            failures,
        )
    });
    tokio::pin!(start);
    let prepare_result = tokio::select! {
        result = &mut start => result,
        _ = tokio::time::sleep(START_TIMEOUT) => {
            send_start_result(
                &updates,
                generation,
                Some("Audio input startup timed out".to_owned()),
            ).await;
            // `spawn_blocking` cannot be cancelled once the CPAL worker runs. Keep
            // this session task alive until the worker returns so VoiceManager
            // retains it as retiring and never overlaps a second recorder.
            dispose_audio_result(start.await).await;
            return;
        }
        changed = control.changed() => {
            if changed.is_err() || *control.borrow() == VoiceControl::Cancel {
                // See the timeout branch: waiting here is the resource-safety
                // barrier, while reducer cancellation has already restored UI.
                dispose_audio_result(start.await).await;
                return;
            }
            start.await
        }
    };
    let recorder = match audio_worker_result(prepare_result) {
        Ok(recorder) => recorder,
        Err(error) => {
            send_start_result(&updates, generation, Some(error)).await;
            return;
        }
    };
    if *control.borrow() == VoiceControl::Cancel {
        dispose_audio_resource(recorder).await;
        return;
    }
    let config = config.with_sample_rate(recorder.sample_rate());

    let connection = prepare_transcription(&credential, &config);
    tokio::pin!(connection);
    let connection_result = loop {
        tokio::select! {
            result = &mut connection => break result,
            changed = control.changed() => {
                if changed.is_err() || *control.borrow() == VoiceControl::Cancel {
                    dispose_audio_resource(recorder).await;
                    return;
                }
            }
        }
    };
    let prepared_transcription = match connection_result {
        Ok(transcription) => transcription,
        Err(error) => {
            dispose_audio_resource(recorder).await;
            send_start_result(&updates, generation, Some(error)).await;
            return;
        }
    };
    if *control.borrow() == VoiceControl::Cancel {
        dispose_audio_resource(recorder).await;
        return;
    }
    // The endpoint named the session while it was being opened, which is the
    // point the reference's `TranscribeSessionCreated` reaches its manager.
    let _ = signals
        .send(VoiceSignal::SessionCreated {
            generation,
            recording_id: prepared_transcription.recording_id().to_owned(),
        })
        .await;

    let transcribe_updates = updates.clone();
    let (consumer_ready, consumer_started) = tokio::sync::oneshot::channel();
    let mut transcription = tokio::spawn(async move {
        prepared_transcription
            .run(
                audio_rx,
                failure_rx,
                consumer_ready,
                generation,
                &transcribe_updates,
            )
            .await
    });
    tokio::pin!(consumer_started);
    let consumer_result = loop {
        tokio::select! {
            result = &mut consumer_started => break result,
            changed = control.changed() => {
                if changed.is_err() || *control.borrow() == VoiceControl::Cancel {
                    transcription.abort();
                    let _ = transcription.await;
                    dispose_audio_resource(recorder).await;
                    return;
                }
            }
        }
    };
    if consumer_result.is_err() {
        let error = transcription_start_error(transcription.await);
        dispose_audio_resource(recorder).await;
        send_start_result(&updates, generation, Some(error)).await;
        return;
    }

    let play = tokio::task::spawn_blocking(move || recorder.play());
    tokio::pin!(play);
    let play_result = tokio::select! {
        result = &mut play => result,
        _ = tokio::time::sleep(START_TIMEOUT) => {
            send_start_result(
                &updates,
                generation,
                Some("Audio input startup timed out".to_owned()),
            ).await;
            transcription.abort();
            let _ = transcription.await;
            dispose_audio_result(play.await).await;
            return;
        }
        changed = control.changed() => {
            if changed.is_err() || *control.borrow() == VoiceControl::Cancel {
                transcription.abort();
                let _ = transcription.await;
                dispose_audio_result(play.await).await;
                return;
            }
            play.await
        }
    };
    let recorder = match audio_worker_result(play_result) {
        Ok(recorder) => recorder,
        Err(error) => {
            transcription.abort();
            let _ = transcription.await;
            send_start_result(&updates, generation, Some(error)).await;
            return;
        }
    };
    let pending_control = *control.borrow();
    let recorder = Arc::new(Mutex::new(Some(recorder)));
    send_start_result(&updates, generation, None).await;
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

fn audio_worker_result<T>(result: Result<Result<T, String>, JoinError>) -> Result<T, String> {
    result.map_err(|error| format!("Audio input worker failed: {error}"))?
}

fn transcription_start_error(result: Result<Result<(), String>, JoinError>) -> String {
    match result {
        Ok(Err(error)) => error,
        Ok(Ok(())) => "Transcription ended before audio input started".to_owned(),
        Err(error) => format!("Transcription worker failed: {error}"),
    }
}

async fn dispose_audio_result<T: Send + 'static>(result: Result<Result<T, String>, JoinError>) {
    if let Ok(resource) = audio_worker_result(result) {
        dispose_audio_resource(resource).await;
    }
}

async fn dispose_audio_resource<T: Send + 'static>(resource: T) {
    let _ = tokio::task::spawn_blocking(move || drop(resource)).await;
}

async fn drop_recorder(recorder: Arc<Mutex<Option<CpalAudioRecorder>>>) {
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(mut recorder) = recorder.lock() {
            recorder.take();
        }
    })
    .await;
}
