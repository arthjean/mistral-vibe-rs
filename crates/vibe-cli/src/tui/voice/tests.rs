use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::{SinkExt, StreamExt};
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use super::realtime::{message_json, run_transcription, session_update};
use super::recorder::AudioMessage;
use super::session::send_transcription_result;
use super::*;

struct ScriptedFactory {
    launches: Arc<AtomicUsize>,
    cancellations: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    maximum_active: Arc<AtomicUsize>,
    cancel_gate: Option<Arc<Notify>>,
}

impl VoiceSessionFactory for ScriptedFactory {
    fn spawn(
        &self,
        generation: u64,
        updates: mpsc::Sender<InputEvent>,
        mut control: watch::Receiver<VoiceControl>,
    ) -> JoinHandle<()> {
        self.launches.fetch_add(1, Ordering::Relaxed);
        let current = self.active.fetch_add(1, Ordering::Relaxed) + 1;
        self.maximum_active.fetch_max(current, Ordering::Relaxed);
        let cancellations = self.cancellations.clone();
        let active = self.active.clone();
        let cancel_gate = self.cancel_gate.clone();
        tokio::spawn(async move {
            let _ = updates
                .send(InputEvent::VoiceStartResolved {
                    generation,
                    error: None,
                })
                .await;
            while control.changed().await.is_ok() {
                let command = *control.borrow();
                match command {
                    VoiceControl::Stop => {
                        let _ = updates.send(InputEvent::VoiceDone { generation }).await;
                        active.fetch_sub(1, Ordering::Relaxed);
                        return;
                    }
                    VoiceControl::Cancel => {
                        cancellations.fetch_add(1, Ordering::Relaxed);
                        if let Some(gate) = cancel_gate {
                            gate.notified().await;
                        }
                        active.fetch_sub(1, Ordering::Relaxed);
                        return;
                    }
                    VoiceControl::Running => {}
                }
            }
            active.fetch_sub(1, Ordering::Relaxed);
        })
    }
}

async fn next_event(manager: &mut VoiceManager) -> InputEvent {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(event) = manager.try_next_event() {
                return event;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("scripted voice event")
}

fn scripted_factory(cancel_gate: Option<Arc<Notify>>) -> (Arc<ScriptedFactory>, Arc<AtomicUsize>) {
    let maximum_active = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(ScriptedFactory {
            launches: Arc::new(AtomicUsize::new(0)),
            cancellations: Arc::new(AtomicUsize::new(0)),
            active: Arc::new(AtomicUsize::new(0)),
            maximum_active: maximum_active.clone(),
            cancel_gate,
        }),
        maximum_active,
    )
}

#[tokio::test]
async fn manager_starts_stops_and_respects_disabled_state() {
    let (factory, _) = scripted_factory(None);
    let launches = factory.launches.clone();
    let cancellations = factory.cancellations.clone();
    let mut manager = VoiceManager::new(factory, false);
    manager.apply_effects(&[InputEffect::RecordingStartRequested], 1);
    tokio::task::yield_now().await;
    assert_eq!(launches.load(Ordering::Relaxed), 0);
    assert!(matches!(
        manager.try_next_event(),
        Some(InputEvent::VoiceStartResolved { error: Some(_), .. })
    ));

    manager.set_enabled(true);
    manager.apply_effects(&[InputEffect::RecordingStartRequested], 2);
    assert!(matches!(
        next_event(&mut manager).await,
        InputEvent::VoiceStartResolved {
            generation: 2,
            error: None
        }
    ));
    manager.apply_effects(&[InputEffect::RecordingStopRequested], 2);
    assert!(matches!(
        next_event(&mut manager).await,
        InputEvent::VoiceDone { generation: 2 }
    ));

    manager.apply_effects(&[InputEffect::RecordingStartRequested], 3);
    assert!(matches!(
        next_event(&mut manager).await,
        InputEvent::VoiceStartResolved {
            generation: 3,
            error: None
        }
    ));
    assert_eq!(launches.load(Ordering::Relaxed), 2);
    manager.shutdown().await;
    assert_eq!(cancellations.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn restart_waits_until_the_cancelled_session_has_fully_retired() {
    let gate = Arc::new(Notify::new());
    let (factory, maximum_active) = scripted_factory(Some(gate.clone()));
    let launches = factory.launches.clone();
    let mut manager = VoiceManager::new(factory, true);
    manager.apply_effects(&[InputEffect::RecordingStartRequested], 1);
    let _ = next_event(&mut manager).await;

    manager.apply_effects(
        &[
            InputEffect::RecordingCancelRequested,
            InputEffect::RecordingStartRequested,
        ],
        2,
    );
    for _ in 0..10 {
        assert!(manager.try_next_event().is_none());
        tokio::task::yield_now().await;
    }
    assert_eq!(launches.load(Ordering::Relaxed), 1);
    assert_eq!(manager.pending_start, Some(2));

    gate.notify_one();
    assert!(matches!(
        next_event(&mut manager).await,
        InputEvent::VoiceStartResolved {
            generation: 2,
            error: None
        }
    ));
    assert_eq!(launches.load(Ordering::Relaxed), 2);
    assert_eq!(maximum_active.load(Ordering::Relaxed), 1);

    gate.notify_one();
    manager.shutdown().await;
}

#[tokio::test]
async fn cancelling_while_a_restart_is_queued_drops_the_pending_generation() {
    let gate = Arc::new(Notify::new());
    let (factory, _) = scripted_factory(Some(gate.clone()));
    let launches = factory.launches.clone();
    let mut manager = VoiceManager::new(factory, true);
    manager.apply_effects(&[InputEffect::RecordingStartRequested], 1);
    let _ = next_event(&mut manager).await;

    manager.apply_effects(
        &[
            InputEffect::RecordingCancelRequested,
            InputEffect::RecordingStartRequested,
        ],
        2,
    );
    assert_eq!(manager.pending_start, Some(2));
    manager.apply_effects(&[InputEffect::RecordingCancelRequested], 2);
    assert!(manager.pending_start.is_none());

    gate.notify_one();
    for _ in 0..20 {
        let _ = manager.try_next_event();
        tokio::task::yield_now().await;
    }
    assert_eq!(launches.load(Ordering::Relaxed), 1);
    manager.shutdown().await;
}

#[tokio::test]
async fn realtime_client_streams_pcm_and_maps_delta_and_done_events() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client connection");
        let mut socket = accept_async(stream).await.expect("websocket handshake");
        socket
            .send(Message::Text(
                json!({"type": "session.created"}).to_string().into(),
            ))
            .await
            .expect("session created");
        let update = message_json(
            &socket
                .next()
                .await
                .expect("session update")
                .expect("message"),
        )
        .expect("update JSON");
        assert_eq!(update["type"], "session.update");
        let append = message_json(&socket.next().await.expect("audio append").expect("message"))
            .expect("append JSON");
        assert_eq!(append["type"], "input_audio.append");
        assert_eq!(append["audio"], "AQID");
        assert_eq!(
            message_json(&socket.next().await.expect("flush").expect("message"))
                .expect("flush JSON")["type"],
            "input_audio.flush"
        );
        assert_eq!(
            message_json(&socket.next().await.expect("end").expect("message")).expect("end JSON")["type"],
            "input_audio.end"
        );
        socket
            .send(Message::Text(
                json!({"type": "transcription.text.delta", "text": "hello"})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("delta");
        socket
            .send(Message::Text(
                json!({"type": "transcription.done"}).to_string().into(),
            ))
            .await
            .expect("done");
    });
    let config =
        VoiceConfig::from_api_base(&format!("http://{address}")).expect("test voice config");
    let (audio_tx, audio_rx) = mpsc::channel(2);
    audio_tx
        .send(AudioMessage::Chunk(vec![1, 2, 3]))
        .await
        .expect("audio chunk");
    drop(audio_tx);
    let (updates_tx, mut updates_rx) = mpsc::channel(4);
    run_transcription(
        audio_rx,
        &SecretString::from("test-key".to_owned()),
        &config,
        7,
        &updates_tx,
    )
    .await
    .expect("transcription succeeds");
    assert!(matches!(
        updates_rx.recv().await,
        Some(InputEvent::VoiceTranscriptDelta {
            generation: 7,
            ref text
        }) if text == "hello"
    ));
    send_transcription_result(&updates_tx, 7, Ok(Ok(()))).await;
    assert!(matches!(
        updates_rx.recv().await,
        Some(InputEvent::VoiceDone { generation: 7 })
    ));
    server.await.expect("test server");
}

#[test]
fn voice_url_and_session_update_match_the_mistral_contract() {
    let config = VoiceConfig::from_api_base("https://api.mistral.ai/v1/chat/completions")
        .expect("voice configuration");
    assert_eq!(
        config.endpoint.as_str(),
        "wss://api.mistral.ai/v1/audio/transcriptions/realtime?model=voxtral-mini-transcribe-realtime-2602"
    );
    let update: Value =
        serde_json::from_str(&session_update(16_000, 500)).expect("session update JSON");
    assert_eq!(update["type"], "session.update");
    assert_eq!(update["session"]["audio_format"]["encoding"], "pcm_s16le");
    assert_eq!(update["session"]["audio_format"]["sample_rate"], 16_000);
}
