//! Audio output: WAV decoding and CPAL playback of a decoded buffer.
//!
//! The reference decodes the container with `wave` and hands the raw PCM to one
//! `sounddevice.RawOutputStream` opened on the default output device, at the
//! container's own sample rate and channel count, in 16-bit frames
//! (`vibe/cli/audio_player/audio_player.py:26-28,61-144` and
//! `vibe/cli/audio_player/utils.py:7-12`). It reports four causes: a second
//! playback while one runs, an absent backend, an absent device and a container
//! it cannot decode (`audio_player_port.py:8-25`).
//!
//! This module reproduces that shape over CPAL, which is what the recorder
//! already uses on the input side. Decoding is a pure function that runs before
//! any device is opened, so an undecodable payload never reaches the audio
//! layer, and the device is addressed through [`AudioOutput`] so the speech path
//! is exercised in a test that has neither a backend nor a device.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig, SupportedStreamConfig,
};
use tokio::sync::watch;

/// Reference `DEFAULT_BLOCKSIZE`: the frame count one output callback is asked
/// for.
pub(super) const PLAYBACK_BLOCK_SIZE: u32 = 4096;
/// Reference `DTYPE`: the sample format the output stream is opened in.
pub(super) const PLAYBACK_SAMPLE_FORMAT: &str = "int16";
/// Reference `DEFAULT_SAMPLE_WIDTH`: the byte width of one such sample.
pub(super) const PLAYBACK_SAMPLE_WIDTH: u32 = 2;

/// Reference `AudioFormat`: the one container this port decodes.
const WAV: &str = "wav";

/// Reference `AlreadyPlayingError`, `AudioBackendUnavailableError`,
/// `NoAudioOutputDeviceError` and `UnsupportedAudioFormatError`, kept as causes
/// rather than as message text so a caller can decide by kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PlaybackError {
    /// A second playback was requested while one was still running.
    AlreadyPlaying,
    /// No audio backend answered at all.
    BackendUnavailable(String),
    /// A backend answered and named no output device.
    NoOutputDevice(String),
    /// The payload is not a container this port can decode.
    UnsupportedFormat(String),
}

impl std::fmt::Display for PlaybackError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyPlaying => formatter.write_str("Narration is already speaking"),
            Self::BackendUnavailable(detail) => {
                write!(formatter, "No audio output backend is available: {detail}")
            }
            Self::NoOutputDevice(detail) => {
                write!(formatter, "No audio output device is available: {detail}")
            }
            Self::UnsupportedFormat(detail) => {
                write!(formatter, "Spoken audio could not be decoded: {detail}")
            }
        }
    }
}

/// What `decode_wav` answers: the three values the container carries, read from
/// it rather than assumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DecodedAudio {
    pub(super) sample_rate: u32,
    pub(super) channels: u16,
    /// Interleaved 16-bit frames, in the container's own channel order.
    pub(super) samples: Vec<i16>,
}

/// Reference `decode_wav`: the sample rate, the channel count and the PCM
/// payload come out of the RIFF header, and a payload that is not a 16-bit PCM
/// WAVE is refused before any device is opened.
pub(super) fn decode_wav(payload: &[u8]) -> Result<DecodedAudio, PlaybackError> {
    let unsupported =
        |detail: String| -> PlaybackError { PlaybackError::UnsupportedFormat(detail) };
    if payload.len() < 12 || &payload[0..4] != b"RIFF" || &payload[8..12] != b"WAVE" {
        return Err(unsupported(format!(
            "the payload is not a {WAV} container ({} bytes)",
            payload.len()
        )));
    }
    let mut format: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    let mut offset = 12;
    while offset + 8 <= payload.len() {
        let identifier = &payload[offset..offset + 4];
        let declared = u32::from_le_bytes([
            payload[offset + 4],
            payload[offset + 5],
            payload[offset + 6],
            payload[offset + 7],
        ]);
        let start = offset + 8;
        let end = usize::try_from(declared)
            .ok()
            .and_then(|size| start.checked_add(size))
            .unwrap_or(payload.len())
            .min(payload.len());
        match identifier {
            b"fmt " if end - start >= 16 => {
                let chunk = &payload[start..end];
                format = Some((
                    u16::from_le_bytes([chunk[0], chunk[1]]),
                    u16::from_le_bytes([chunk[2], chunk[3]]),
                    u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
                    u16::from_le_bytes([chunk[14], chunk[15]]),
                ));
            }
            b"data" => data = Some(&payload[start..end]),
            _ => {}
        }
        // RIFF pads every odd-sized chunk to an even boundary.
        offset = end + (end % 2);
    }
    let Some((tag, channels, sample_rate, bits)) = format else {
        return Err(unsupported(
            "the container declares no format chunk".to_owned(),
        ));
    };
    // 1 is PCM and 0xFFFE is the extensible header shipped for the same
    // uncompressed frames; anything else is a codec this port does not decode.
    if tag != 1 && tag != 0xFFFE {
        return Err(unsupported(format!(
            "the container declares the unsupported format tag {tag}"
        )));
    }
    if bits != 16 {
        return Err(unsupported(format!(
            "the container declares {bits}-bit samples and playback is {PLAYBACK_SAMPLE_FORMAT}"
        )));
    }
    if channels == 0 || sample_rate == 0 {
        return Err(unsupported(format!(
            "the container declares {channels} channels at {sample_rate} Hz"
        )));
    }
    let Some(data) = data else {
        return Err(unsupported(
            "the container declares no data chunk".to_owned(),
        ));
    };
    let samples = data
        .chunks_exact(usize::try_from(PLAYBACK_SAMPLE_WIDTH).unwrap_or(2))
        .map(|frame| i16::from_le_bytes([frame[0], frame[1]]))
        .collect();
    Ok(DecodedAudio {
        sample_rate,
        channels,
        samples,
    })
}

/// A playback in progress. Dropping it closes the stream, which is how the
/// reference's `stop` ends one, and `finished` carries the completion the
/// exhausted buffer raises.
pub(super) struct Playback {
    finished: watch::Receiver<bool>,
    /// The open stream, held only to keep it alive. CPAL streams are dropped on
    /// the blocking pool by the caller, exactly as the recorder disposes of its
    /// own.
    _resource: Box<dyn Send>,
}

impl Playback {
    pub(super) fn new(finished: watch::Receiver<bool>, resource: Box<dyn Send>) -> Self {
        Self {
            finished,
            _resource: resource,
        }
    }

    /// Resolves once the buffer is exhausted or the stream is gone.
    pub(super) async fn finished(&mut self) {
        let _ = self.finished.wait_for(|finished| *finished).await;
    }
}

/// The device boundary. Production opens a CPAL output stream; a test drives the
/// same speech path with no backend at all.
pub(super) trait AudioOutput: Send + Sync + 'static {
    fn start(&self, audio: DecodedAudio) -> Result<Playback, PlaybackError>;
}

/// Signals the exhausted buffer exactly once, from the audio callback.
#[derive(Clone)]
struct CompletionSignal {
    sender: Arc<watch::Sender<bool>>,
    signaled: Arc<AtomicBool>,
}

impl CompletionSignal {
    fn channel() -> (Self, watch::Receiver<bool>) {
        let (sender, receiver) = watch::channel(false);
        (
            Self {
                sender: Arc::new(sender),
                signaled: Arc::new(AtomicBool::new(false)),
            },
            receiver,
        )
    }

    fn signal(&self) {
        if self
            .signaled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.sender.send_replace(true);
        }
    }
}

/// Reference `AudioPlayer`: the default output device, opened at the container's
/// own rate.
pub(super) struct CpalAudioOutput;

impl AudioOutput for CpalAudioOutput {
    fn start(&self, audio: DecodedAudio) -> Result<Playback, PlaybackError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| PlaybackError::NoOutputDevice("the host names none".to_owned()))?;
        let supported = select_output_config(&device, audio.sample_rate, audio.channels)?;
        let device_channels = usize::from(supported.channels());
        let sample_format = supported.sample_format();
        let mut stream_config: StreamConfig = supported.into();
        stream_config.buffer_size = cpal::BufferSize::Fixed(PLAYBACK_BLOCK_SIZE);
        let (completion, finished) = CompletionSignal::channel();
        let source_channels = usize::from(audio.channels).max(1);
        let samples = Arc::new(audio.samples);
        let cursor = Arc::new(AtomicUsize::new(0));
        let error_completion = completion.clone();
        let error_callback = move |_: cpal::Error| {
            // A stream that fails mid-buffer never raises its own completion, so
            // the waiting caller is released here instead of hanging.
            error_completion.signal();
        };
        let feed = Feed {
            samples,
            cursor,
            source_channels,
            device_channels,
            completion,
        };
        let stream = match sample_format {
            SampleFormat::I16 => build_output_stream::<i16>(
                &device,
                &stream_config,
                feed,
                error_callback,
                sample_format,
            ),
            SampleFormat::I32 => build_output_stream::<i32>(
                &device,
                &stream_config,
                feed,
                error_callback,
                sample_format,
            ),
            SampleFormat::U16 => build_output_stream::<u16>(
                &device,
                &stream_config,
                feed,
                error_callback,
                sample_format,
            ),
            SampleFormat::F32 => build_output_stream::<f32>(
                &device,
                &stream_config,
                feed,
                error_callback,
                sample_format,
            ),
            SampleFormat::F64 => build_output_stream::<f64>(
                &device,
                &stream_config,
                feed,
                error_callback,
                sample_format,
            ),
            format => {
                return Err(PlaybackError::BackendUnavailable(format!(
                    "the output device speaks the unsupported sample format `{format}`"
                )));
            }
        }?;
        stream
            .play()
            .map_err(|error| PlaybackError::NoOutputDevice(error.to_string()))?;
        Ok(Playback::new(finished, Box::new(stream)))
    }
}

/// What one output callback reads from.
struct Feed {
    samples: Arc<Vec<i16>>,
    cursor: Arc<AtomicUsize>,
    source_channels: usize,
    device_channels: usize,
    completion: CompletionSignal,
}

/// The sample formats a decoded buffer is written in, most preferred first. The
/// reference opens its stream in `int16` and lets PortAudio convert, so a device
/// that names an int16 configuration is addressed in it and the rest are what a
/// device that names none is met with. A format outside this list is one this
/// port cannot write, so a configuration carrying it is never selected.
const OUTPUT_SAMPLE_FORMATS: [SampleFormat; 5] = [
    SampleFormat::I16,
    SampleFormat::F32,
    SampleFormat::I32,
    SampleFormat::U16,
    SampleFormat::F64,
];

fn output_format_rank(format: SampleFormat) -> Option<usize> {
    OUTPUT_SAMPLE_FORMATS
        .iter()
        .position(|candidate| *candidate == format)
}

/// The device configuration a decoded buffer is played through: the container's
/// own rate, its channel count where the device supports it, and a sample format
/// this port writes. A default device commonly names configurations in formats
/// the stream builder refuses, so the format is part of the selection rather
/// than something the opened stream is asked to accept.
fn select_output_config(
    device: &cpal::Device,
    sample_rate: u32,
    channels: u16,
) -> Result<SupportedStreamConfig, PlaybackError> {
    let mut ranges = device
        .supported_output_configs()
        .map_err(|error| PlaybackError::BackendUnavailable(error.to_string()))?
        .filter(|range| output_format_rank(range.sample_format()).is_some())
        .filter(|range| {
            range.min_sample_rate() <= sample_rate && sample_rate <= range.max_sample_rate()
        })
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| {
        (
            u8::from(range.channels() != channels),
            output_format_rank(range.sample_format()).unwrap_or(usize::MAX),
            range.channels(),
        )
    });
    ranges
        .first()
        .and_then(|range| range.try_with_sample_rate(sample_rate))
        .ok_or_else(|| {
            PlaybackError::NoOutputDevice(format!(
                "the default output device plays no {sample_rate} Hz stream this port can write"
            ))
        })
}

fn build_output_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    feed: Feed,
    error_callback: impl FnMut(cpal::Error) + Send + 'static,
    sample_format: SampleFormat,
) -> Result<Stream, PlaybackError>
where
    T: Sample + SizedSample + FromSample<i16>,
{
    device
        .build_output_stream(
            *config,
            move |output: &mut [T], _| {
                write_frames(&feed, output);
            },
            error_callback,
            None,
        )
        .map_err(|error| {
            PlaybackError::NoOutputDevice(format!(
                "the default output device refused a {sample_format} stream: {error}"
            ))
        })
}

/// Fills one callback buffer from the decoded frames, pads the tail with silence
/// and raises the completion the exhausted buffer implies. A container whose
/// channel count differs from the device's is spread or mixed down rather than
/// refused, which is the reference's own behavior through PortAudio.
fn write_frames<T: Sample + FromSample<i16>>(feed: &Feed, output: &mut [T]) {
    let device_channels = feed.device_channels.max(1);
    let mut consumed = 0;
    let start = feed.cursor.load(Ordering::Acquire);
    for frame in output.chunks_mut(device_channels) {
        let source = start + consumed;
        if source >= feed.samples.len() {
            for sample in frame.iter_mut() {
                *sample = T::from_sample(0_i16);
            }
            continue;
        }
        let available = feed.samples.len() - source;
        let width = feed.source_channels.min(available);
        for (index, sample) in frame.iter_mut().enumerate() {
            // A mono source is heard on every device channel, a multi-channel
            // source keeps its own channel where the device has one, and the
            // last source channel fills the rest.
            let channel = index.min(width.saturating_sub(1));
            *sample = T::from_sample(feed.samples[source + channel]);
        }
        consumed += width;
    }
    let position = start + consumed;
    feed.cursor.store(position, Ordering::Release);
    if position >= feed.samples.len() {
        feed.completion.signal();
    }
}

#[cfg(test)]
#[path = "player_tests.rs"]
mod player_tests;
