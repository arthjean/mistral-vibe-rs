//! Audio output: WAV decoding and playback of a decoded buffer.
//!
//! Reference `AudioPlayer` (`vibe/cli/audio_player/audio_player.py`) decodes
//! the container with `wave` (`vibe/cli/audio_player/utils.py`) and opens the
//! default output device through miniaudio at the container's own sample rate
//! and channel count, in signed 16-bit frames and 200 ms blocks, and
//! miniaudio converts that stream into whatever the device runs. It reports
//! four causes: a second playback while one runs, an absent backend, an absent
//! device and a container it cannot decode (`audio_player_port.py`).
//!
//! This module reproduces that shape over CPAL. Decoding is a pure function
//! that runs before any device is opened, so an undecodable payload never
//! reaches the audio layer. CPAL opens a device only in a configuration the
//! device offers, so the conversion miniaudio performs is done here, before
//! playback starts: the decoded frames are mapped onto the device's channels
//! and resampled linearly to its rate. The device is addressed through
//! [`AudioOutput`] so the speech path is exercised in a test that has neither a
//! backend nor a device.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig, SupportedStreamConfig,
};
use tokio::sync::watch;

use crate::capture::{LinearResampler, block_frames};

/// Reference `DEFAULT_BUFFER_MS`: the length of one output block.
pub const PLAYBACK_BUFFER_MS: u32 = 200;
/// The sample format the decoded stream is played in.
pub const PLAYBACK_SAMPLE_FORMAT: &str = "int16";
/// Reference `DEFAULT_SAMPLE_WIDTH`: the byte width of one such sample.
pub const PLAYBACK_SAMPLE_WIDTH: u32 = 2;

/// Reference `AlreadyPlayingError`, `AudioBackendUnavailableError`,
/// `NoAudioOutputDeviceError` and `UnsupportedAudioFormatError`, kept as causes
/// rather than as message text so a caller can decide by kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaybackError {
    /// A second playback was requested while one was still running.
    AlreadyPlaying,
    /// No audio backend answered at all.
    BackendUnavailable(String),
    /// A backend answered and named no output device.
    NoOutputDevice(String),
    /// The payload is not a container this port can decode.
    UnsupportedFormat(String),
}

impl PlaybackError {
    /// The class name the reference's exception carries, which is what a
    /// read-aloud failure reports as its error type.
    #[must_use]
    pub const fn class_name(&self) -> &'static str {
        match self {
            Self::AlreadyPlaying => "AlreadyPlayingError",
            Self::BackendUnavailable(_) => "AudioBackendUnavailableError",
            Self::NoOutputDevice(_) => "NoAudioOutputDeviceError",
            Self::UnsupportedFormat(_) => "UnsupportedAudioFormatError",
        }
    }
}

impl std::fmt::Display for PlaybackError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyPlaying => formatter.write_str("Already playing"),
            Self::BackendUnavailable(detail) => {
                write!(formatter, "No audio output backend is available: {detail}")
            }
            Self::NoOutputDevice(detail) => {
                write!(formatter, "No audio output device available: {detail}")
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
pub struct DecodedAudio {
    pub sample_rate: u32,
    pub channels: u16,
    /// The frames the container's data chunk holds, as read.
    pub pcm: Vec<u8>,
    /// The same bytes read as interleaved 16-bit frames, which is how the
    /// reference's player hands them to its `SIGNED16` device whatever width
    /// the container declared; a trailing odd byte is padded with the silence
    /// the player pads its last block with.
    pub samples: Vec<i16>,
}

/// Why a payload is not a container `decode_wav` reads, named by the class the
/// reference's `wave` module raises: `Error` for a container it refuses,
/// `EOFError` for one that ends inside a header it needs, and `RuntimeError`
/// for a chunk that claims to extend past the container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WavError {
    pub class: &'static str,
    pub detail: String,
}

impl WavError {
    fn refused(detail: impl Into<String>) -> Self {
        Self {
            class: "Error",
            detail: detail.into(),
        }
    }

    fn truncated() -> Self {
        Self {
            class: "EOFError",
            detail: "the container ends inside a header".to_owned(),
        }
    }
}

impl std::fmt::Display for WavError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Spoken audio could not be decoded: {}",
            self.detail
        )
    }
}

/// Python's `wave._Chunk` header: a name, a declared size, and where the
/// content starts in the parent.
struct Chunk {
    name: [u8; 4],
    size: usize,
    offset: usize,
}

impl Chunk {
    /// Reads a chunk header from `parent`, as `_Chunk.__init__` does: four
    /// name bytes and a little-endian size, or `EOFError`.
    fn open(parent: &mut Window<'_>) -> Result<Self, WavError> {
        let name = parent.read(4);
        let name: [u8; 4] = name.try_into().map_err(|_| WavError::truncated())?;
        let size = parent.read(4);
        let size: [u8; 4] = size.try_into().map_err(|_| WavError::truncated())?;
        Ok(Self {
            name,
            size: usize::try_from(u32::from_le_bytes(size)).unwrap_or(usize::MAX),
            offset: parent.position,
        })
    }
}

/// A reader over the bytes inside one chunk, positioned in its parent.
struct Window<'a> {
    bytes: &'a [u8],
    /// Where the window's content starts in `bytes`.
    start: usize,
    /// The declared content size.
    size: usize,
    /// How much of the content has been consumed, padding included.
    position: usize,
}

impl Window<'_> {
    /// `_Chunk.read`: at most `wanted` bytes, never past the declared size nor
    /// the payload, consuming the pad byte once an odd-sized chunk is read out.
    fn read(&mut self, wanted: usize) -> &[u8] {
        if self.position >= self.size {
            return &[];
        }
        let wanted = wanted.min(self.size - self.position);
        let from = self
            .start
            .saturating_add(self.position)
            .min(self.bytes.len());
        let to = from.saturating_add(wanted).min(self.bytes.len());
        self.position += to - from;
        let data = &self.bytes[from..to];
        if self.position == self.size && self.size % 2 == 1 {
            let pad = self.start.saturating_add(self.position);
            if pad < self.bytes.len() {
                self.position += 1;
            }
        }
        data
    }

    /// `_Chunk.seek(n, 1)`: a move past the declared size raises
    /// `RuntimeError`, which `wave` does not catch.
    fn skip(&mut self, count: usize) -> Result<(), WavError> {
        let target = self.position.saturating_add(count);
        if target > self.size {
            return Err(WavError {
                class: "RuntimeError",
                detail: "a chunk extends past the container".to_owned(),
            });
        }
        self.position = target;
        Ok(())
    }

    /// The window over `chunk`'s content. Its reads go through this one, so
    /// they stop where this window's declared content does.
    fn child(&self, chunk: &Chunk) -> Window<'_> {
        let end = self.start.saturating_add(self.size).min(self.bytes.len());
        Window {
            bytes: &self.bytes[..end],
            start: self.start.saturating_add(chunk.offset),
            size: chunk.size,
            position: 0,
        }
    }
}

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// `KSDATAFORMAT_SUBTYPE_PCM` as its little-endian bytes.
const SUBTYPE_PCM: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// Reference `decode_wav` (`vibe/cli/audio_player/utils.py`), which reads the
/// payload with Python's `wave` module: the rate, the channel count and every
/// frame of the first data chunk, at whatever sample width the format chunk
/// declares.
///
/// # Errors
///
/// A payload `wave.open` refuses, named by the class it raises.
pub fn decode_wav(payload: &[u8]) -> Result<DecodedAudio, WavError> {
    let mut file = Window {
        bytes: payload,
        start: 0,
        size: payload.len(),
        position: 0,
    };
    let riff = Chunk::open(&mut file)?;
    if &riff.name != b"RIFF" {
        return Err(WavError::refused("file does not start with RIFF id"));
    }
    let mut container = file.child(&riff);
    if container.read(4) != b"WAVE" {
        return Err(WavError::refused("not a WAVE file"));
    }
    let mut format: Option<(u16, u32, usize)> = None;
    let mut data: Option<Chunk> = None;
    while let Ok(chunk) = Chunk::open(&mut container) {
        match &chunk.name {
            b"fmt " => {
                format = Some(read_format(&mut container.child(&chunk))?);
                container.skip(chunk_span(&chunk))?;
            }
            b"data" => {
                if format.is_none() {
                    return Err(WavError::refused("data chunk before fmt chunk"));
                }
                data = Some(chunk);
                break;
            }
            _ => container.skip(chunk_span(&chunk))?,
        }
    }
    let (Some((channels, sample_rate, frame_size)), Some(data)) = (format, data) else {
        return Err(WavError::refused("fmt chunk and/or data chunk missing"));
    };
    let mut window = container.child(&data);
    let frames = data.size / frame_size;
    let pcm = window.read(frames.saturating_mul(frame_size)).to_vec();
    let samples = pcm
        .chunks(usize::try_from(PLAYBACK_SAMPLE_WIDTH).unwrap_or(2))
        .map(|frame| i16::from_le_bytes([frame[0], frame.get(1).copied().unwrap_or(0)]))
        .collect();
    Ok(DecodedAudio {
        sample_rate,
        channels,
        pcm,
        samples,
    })
}

/// Where `_Chunk.skip` leaves the parent, counted from the chunk's content:
/// past the whole content and its pad byte, however much of it was read.
fn chunk_span(chunk: &Chunk) -> usize {
    chunk.size.saturating_add(chunk.size % 2)
}

/// `Wave_read._read_fmt_chunk`: the channel count, the rate and the frame
/// size.
fn read_format(window: &mut Window<'_>) -> Result<(u16, u32, usize), WavError> {
    let header = window.read(14);
    if header.len() < 14 {
        return Err(WavError::truncated());
    }
    let tag = u16::from_le_bytes([header[0], header[1]]);
    let channels = u16::from_le_bytes([header[2], header[3]]);
    let sample_rate = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if tag != WAVE_FORMAT_PCM && tag != WAVE_FORMAT_EXTENSIBLE {
        return Err(WavError::refused(format!("unknown format: {tag}")));
    }
    let bits = window.read(2);
    let bits: [u8; 2] = bits.try_into().map_err(|_| WavError::truncated())?;
    let bits = usize::from(u16::from_le_bytes(bits));
    if tag == WAVE_FORMAT_EXTENSIBLE {
        if window.read(8).len() < 8 {
            return Err(WavError::truncated());
        }
        let subformat = window.read(16);
        if subformat.len() < 16 {
            return Err(WavError::truncated());
        }
        if subformat != SUBTYPE_PCM {
            return Err(WavError::refused("unknown extended format"));
        }
    }
    let width = bits.div_ceil(8);
    if width == 0 {
        return Err(WavError::refused("bad sample width"));
    }
    if channels == 0 {
        return Err(WavError::refused("bad # of channels"));
    }
    Ok((channels, sample_rate, usize::from(channels) * width))
}

/// A running playback: completion is observed through `finished`, and the
/// device stays open as long as the value lives.
pub struct Playback {
    finished: watch::Receiver<bool>,
    _resource: Box<dyn Send>,
}

impl Playback {
    #[must_use]
    pub fn new(finished: watch::Receiver<bool>, resource: Box<dyn Send>) -> Self {
        Self {
            finished,
            _resource: resource,
        }
    }

    pub async fn finished(&mut self) {
        let _ = self.finished.wait_for(|finished| *finished).await;
    }
}

/// The output device, behind a seam a test can script.
pub trait AudioOutput: Send + Sync + 'static {
    /// Opens the default output device and starts playing `audio`.
    ///
    /// # Errors
    ///
    /// No backend, no device, or a device that refuses every stream.
    fn start(&self, audio: DecodedAudio) -> Result<Playback, PlaybackError>;
}

#[derive(Clone)]
pub(crate) struct CompletionSignal {
    sender: Arc<watch::Sender<bool>>,
    signaled: Arc<AtomicBool>,
}

impl CompletionSignal {
    pub(crate) fn channel() -> (Self, watch::Receiver<bool>) {
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

/// [`AudioOutput`] over the default CPAL host.
pub struct CpalAudioOutput;

impl AudioOutput for CpalAudioOutput {
    fn start(&self, audio: DecodedAudio) -> Result<Playback, PlaybackError> {
        let host = cpal::default_host();
        // Reference `_guard_audio_output`: a host that lists no playback
        // device, or cannot list them at all, has no output.
        let listed = host
            .output_devices()
            .map_err(|error| PlaybackError::NoOutputDevice(error.to_string()))?
            .next()
            .is_some();
        if !listed {
            return Err(PlaybackError::NoOutputDevice(
                "the host lists no playback device".to_owned(),
            ));
        }
        let device = host
            .default_output_device()
            .ok_or_else(|| PlaybackError::NoOutputDevice("the host names none".to_owned()))?;
        let supported = select_output_config(&device, audio.sample_rate, audio.channels)?;
        let device_channels = usize::from(supported.channels());
        let device_rate = supported.sample_rate();
        let sample_format = supported.sample_format();
        let mut stream_config: StreamConfig = supported.config();
        stream_config.buffer_size = block_frames(device_rate, supported.buffer_size());
        let samples = convert_for_device(&audio, device_rate, device_channels);
        let (completion, finished) = CompletionSignal::channel();
        let error_completion = completion.clone();
        let error_callback = move |_: cpal::Error| {
            error_completion.signal();
        };
        let feed = Feed {
            samples: Arc::new(samples),
            cursor: Arc::new(AtomicUsize::new(0)),
            source_channels: device_channels,
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

/// The decoded frames in the device's layout: each device channel takes the
/// source channel of the same index, the last one when the source has fewer,
/// or the mean of every source channel for a mono device; each channel is then
/// resampled to the device's rate. Miniaudio performs the same two
/// conversions behind the reference's stream.
#[must_use]
pub fn convert_for_device(
    audio: &DecodedAudio,
    device_rate: u32,
    device_channels: usize,
) -> Vec<i16> {
    let source_channels = usize::from(audio.channels).max(1);
    let device_channels = device_channels.max(1);
    let frames = audio.samples.chunks_exact(source_channels);
    let mut channels = vec![Vec::with_capacity(frames.len()); device_channels];
    for frame in frames {
        for (index, channel) in channels.iter_mut().enumerate() {
            let sample = if device_channels == 1 && source_channels > 1 {
                let sum = frame.iter().map(|sample| i32::from(*sample)).sum::<i32>();
                // The mean of int16 values is an int16 value.
                (sum / i32::try_from(source_channels).unwrap_or(1)) as i16
            } else {
                frame[index.min(source_channels - 1)]
            };
            channel.push(sample);
        }
    }
    let resampled = channels
        .iter()
        .map(|channel| {
            let mut output = Vec::new();
            let mut resampler = LinearResampler::new(audio.sample_rate, device_rate);
            resampler.process(channel, &mut output);
            // The last source frame is only emitted by the next block; there
            // is none, so it is appended as is.
            if audio.sample_rate != device_rate
                && let Some(last) = channel.last()
            {
                output.push(*last);
            }
            output
        })
        .collect::<Vec<_>>();
    let length = resampled.iter().map(Vec::len).min().unwrap_or(0);
    let mut interleaved = Vec::with_capacity(length * device_channels);
    for frame in 0..length {
        for channel in &resampled {
            interleaved.push(channel[frame]);
        }
    }
    interleaved
}

pub(crate) struct Feed {
    pub(crate) samples: Arc<Vec<i16>>,
    pub(crate) cursor: Arc<AtomicUsize>,
    pub(crate) source_channels: usize,
    pub(crate) device_channels: usize,
    pub(crate) completion: CompletionSignal,
}

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

/// The configuration the device is opened in: one that runs at the
/// container's rate when the device offers it, the container's channel count
/// and int16 first; the device's default when it is writable; any writable
/// configuration at its highest rate otherwise. Every one but the first is
/// resampled into.
fn select_output_config(
    device: &cpal::Device,
    sample_rate: u32,
    channels: u16,
) -> Result<SupportedStreamConfig, PlaybackError> {
    let mut ranges = device
        .supported_output_configs()
        .map_err(|error| PlaybackError::BackendUnavailable(error.to_string()))?
        .filter(|range| output_format_rank(range.sample_format()).is_some())
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| {
        (
            u8::from(range.channels() != channels),
            output_format_rank(range.sample_format()).unwrap_or(usize::MAX),
            range.channels(),
        )
    });
    if let Some(exact) = ranges
        .iter()
        .filter(|range| {
            range.min_sample_rate() <= sample_rate && sample_rate <= range.max_sample_rate()
        })
        .find_map(|range| range.try_with_sample_rate(sample_rate))
    {
        return Ok(exact);
    }
    if let Ok(default) = device.default_output_config()
        && output_format_rank(default.sample_format()).is_some()
    {
        return Ok(default);
    }
    ranges
        .first()
        .map(|range| range.with_max_sample_rate())
        .ok_or_else(|| {
            PlaybackError::NoOutputDevice(
                "the default output device plays no stream this port can write".to_owned(),
            )
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

/// Reference `_playback_generator`: the buffer is written frame by frame and
/// the exhausted tail is padded with silence, which is where completion is
/// raised.
pub(crate) fn write_frames<T: Sample + FromSample<i16>>(feed: &Feed, output: &mut [T]) {
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
#[path = "playback_tests.rs"]
mod playback_tests;
