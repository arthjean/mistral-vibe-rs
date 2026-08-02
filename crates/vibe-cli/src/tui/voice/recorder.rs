//! CPAL microphone capture and PCM conversion.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig, SupportedStreamConfig,
};
use tokio::sync::{mpsc, watch};

use crate::tui::chat_input::InputEvent;

const AUDIO_OVERFLOW_ERROR: &str = "Audio input could not keep up; captured audio was lost";

#[derive(Clone)]
pub(super) struct AudioFailureSignal {
    sender: watch::Sender<Option<String>>,
    reported: Arc<AtomicBool>,
}

impl AudioFailureSignal {
    pub(super) fn channel() -> (Self, watch::Receiver<Option<String>>) {
        let (sender, receiver) = watch::channel(None);
        (
            Self {
                sender,
                reported: Arc::new(AtomicBool::new(false)),
            },
            receiver,
        )
    }

    fn report(&self, message: impl FnOnce() -> String) {
        if self
            .reported
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.sender.send_replace(Some(message()));
        }
    }
}

struct CaptureOutput {
    generation: u64,
    updates: mpsc::Sender<InputEvent>,
    audio: mpsc::Sender<Vec<u8>>,
    failures: AudioFailureSignal,
}

pub(super) struct PreparedCpalAudioRecorder {
    stream: Stream,
    sample_rate: u32,
}

pub(super) struct CpalAudioRecorder {
    _stream: Stream,
}

impl PreparedCpalAudioRecorder {
    pub(super) const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub(super) fn play(self) -> Result<CpalAudioRecorder, String> {
        self.stream
            .play()
            .map_err(|error| format!("Audio input could not start: {error}"))?;
        Ok(CpalAudioRecorder {
            _stream: self.stream,
        })
    }
}

impl CpalAudioRecorder {
    pub(super) fn prepare(
        generation: u64,
        requested_sample_rate: u32,
        updates: mpsc::Sender<InputEvent>,
        audio: mpsc::Sender<Vec<u8>>,
        failures: AudioFailureSignal,
    ) -> Result<PreparedCpalAudioRecorder, String> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| "No audio input device found".to_owned())?;
        let supported = select_input_config(&device, requested_sample_rate)?;
        let sample_rate = supported.sample_rate();
        let channels = usize::from(supported.channels());
        let stream_config: StreamConfig = supported.into();
        let error_failures = failures.clone();
        let error_callback = move |error: cpal::Error| {
            error_failures.report(|| format!("Audio input failed: {error}"));
        };
        let output = CaptureOutput {
            generation,
            updates,
            audio,
            failures,
        };
        let stream = match supported.sample_format() {
            SampleFormat::I8 => {
                build_input_stream::<i8>(&device, &stream_config, channels, output, error_callback)
            }
            SampleFormat::I16 => {
                build_input_stream::<i16>(&device, &stream_config, channels, output, error_callback)
            }
            SampleFormat::I32 => {
                build_input_stream::<i32>(&device, &stream_config, channels, output, error_callback)
            }
            SampleFormat::I64 => {
                build_input_stream::<i64>(&device, &stream_config, channels, output, error_callback)
            }
            SampleFormat::U8 => {
                build_input_stream::<u8>(&device, &stream_config, channels, output, error_callback)
            }
            SampleFormat::U16 => {
                build_input_stream::<u16>(&device, &stream_config, channels, output, error_callback)
            }
            SampleFormat::U32 => {
                build_input_stream::<u32>(&device, &stream_config, channels, output, error_callback)
            }
            SampleFormat::U64 => {
                build_input_stream::<u64>(&device, &stream_config, channels, output, error_callback)
            }
            SampleFormat::F32 => {
                build_input_stream::<f32>(&device, &stream_config, channels, output, error_callback)
            }
            SampleFormat::F64 => {
                build_input_stream::<f64>(&device, &stream_config, channels, output, error_callback)
            }
            format => return Err(format!("Audio sample format `{format}` is unsupported")),
        }
        .map_err(|error| format!("Audio input could not start: {error}"))?;
        Ok(PreparedCpalAudioRecorder {
            stream,
            sample_rate,
        })
    }
}

fn select_input_config(
    device: &cpal::Device,
    requested_sample_rate: u32,
) -> Result<SupportedStreamConfig, String> {
    let ranges = device
        .supported_input_configs()
        .map_err(|error| format!("Audio input configuration is unavailable: {error}"))?
        .filter(|range| !range.sample_format().is_dsd())
        .collect::<Vec<_>>();
    let mut rates = [requested_sample_rate, 48_000, 44_100, 22_050, 16_000, 8_000]
        .into_iter()
        .collect::<Vec<_>>();
    rates.dedup();
    for rate in rates {
        if let Some(config) = ranges
            .iter()
            .filter(|range| range.min_sample_rate() <= rate && rate <= range.max_sample_rate())
            .min_by_key(|range| range.channels())
            .and_then(|range| range.try_with_sample_rate(rate))
        {
            return Ok(config);
        }
    }
    Err("The audio input device has no transcription-compatible sample rate".to_owned())
}

fn build_input_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    channels: usize,
    output: CaptureOutput,
    error_callback: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream, cpal::Error>
where
    T: Sample + SizedSample + Copy,
    i16: FromSample<T>,
{
    let last_level = Arc::new(AtomicU8::new(u8::MAX));
    device.build_input_stream(
        *config,
        move |samples: &[T], _| {
            let mut pcm = Vec::with_capacity(samples.len().saturating_mul(2) / channels.max(1));
            let mut peak = 0_u16;
            for frame in samples.chunks_exact(channels.max(1)) {
                let sum = frame
                    .iter()
                    .map(|sample| i32::from(i16::from_sample(*sample)))
                    .sum::<i32>();
                let mono = (sum / i32::try_from(frame.len()).unwrap_or(1)) as i16;
                peak = peak.max(mono.unsigned_abs());
                pcm.extend_from_slice(&mono.to_le_bytes());
            }
            if !pcm.is_empty() {
                enqueue_audio(&output.audio, &output.failures, pcm);
            }
            let level = u8::try_from((u32::from(peak) * 8 / 32_768).min(7)).unwrap_or(7);
            if last_level.swap(level, Ordering::Relaxed) != level {
                let _ = output.updates.try_send(InputEvent::VoicePeak {
                    generation: output.generation,
                    level,
                });
            }
        },
        error_callback,
        None,
    )
}

pub(super) fn enqueue_audio(
    audio: &mpsc::Sender<Vec<u8>>,
    failures: &AudioFailureSignal,
    pcm: Vec<u8>,
) {
    match audio.try_send(pcm) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            failures.report(|| AUDIO_OVERFLOW_ERROR.to_owned());
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}
