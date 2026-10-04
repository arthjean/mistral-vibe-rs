//! Microphone capture.
//!
//! Reference `AudioRecorder` (`vibe/cli/audio_recorder/audio_recorder.py`)
//! opens the default capture device through miniaudio in mono signed 16-bit
//! frames at the sample rate the active transcription model declares, in
//! 200 ms blocks, and miniaudio converts whatever the device delivers into
//! that format. Every block updates the peak level and whether any signal
//! reached the recorder at all, and is queued for the transcription stream; a
//! stop or a cancellation ends the stream, and a recording left running is
//! stopped after five minutes.
//!
//! This module reproduces that contract over CPAL. CPAL opens a device in a
//! configuration the device offers, so the conversion miniaudio performs is
//! done here: the device's frames are mixed down to one channel and resampled
//! linearly, which is miniaudio's default resampler, to the requested rate.
//! What leaves the recorder is therefore the stream the reference sends,
//! whatever the hardware runs at.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig, SupportedBufferSize,
    SupportedStreamConfig,
};
use tokio::sync::mpsc;

/// Reference `DEFAULT_BUFFER_MS`: the length of one captured block.
pub const CAPTURE_BUFFER_MS: u32 = 200;
/// Reference `SILENCE_PEAK_THRESHOLD`: a block whose peak exceeds this level
/// carried a real signal; a denied or muted microphone delivers pure silence.
pub const SILENCE_PEAK_THRESHOLD: f32 = 0.001;
/// Reference `DEFAULT_MAX_DURATION`: a recording still running after this long
/// is stopped.
pub const MAX_RECORDING_DURATION: Duration = Duration::from_secs(300);
/// Reference `INT16_ABS_MAX`.
const INT16_ABS_MAX: f32 = 32_767.0;

/// The frame format of every captured block: the reference's
/// `SampleFormat.SIGNED16`, little-endian.
pub const CAPTURE_SAMPLE_FORMAT: &str = "int16";
/// The channel count of every captured block, the reference's
/// `DEFAULT_CHANNELS`.
pub const CAPTURE_CHANNELS: u16 = 1;

/// The audio a recording produces, block by block, in [`CAPTURE_SAMPLE_FORMAT`]
/// frames on [`CAPTURE_CHANNELS`] channel at the requested rate. The stream
/// ends when the recording stops.
pub type AudioStream = mpsc::UnboundedReceiver<Vec<u8>>;

/// Reference `AlreadyRecordingError`, `AudioBackendUnavailableError` and
/// `NoAudioInputDeviceError`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecorderError {
    AlreadyRecording,
    BackendUnavailable(String),
    NoInputDevice,
}

/// Reference `AudioRecorderPort` in streaming mode, the only mode any caller
/// of the reference records in.
pub trait AudioRecorder: Send + Sync + 'static {
    /// Opens the default input device and starts streaming blocks at
    /// `sample_rate`.
    ///
    /// # Errors
    ///
    /// A recording already running, no audio backend, or no input device.
    fn start(&self, sample_rate: u32) -> Result<AudioStream, RecorderError>;

    /// Ends the recording and its stream, answering how long it ran; a
    /// recorder that was not recording answers zero.
    fn stop(&self) -> Duration;

    /// Ends the recording and its stream without reporting anything.
    fn cancel(&self);

    /// The last block's peak, normalized to `[0.0, 1.0]`.
    fn peak(&self) -> f32;

    /// Whether any block since the last start exceeded the silence floor.
    fn has_signal(&self) -> bool;
}

/// The level readings a recorder exposes while its callback writes them.
#[derive(Debug, Default)]
pub struct SignalMeter {
    peak: AtomicU32,
    signal: AtomicBool,
}

impl SignalMeter {
    /// Reference `start`: both readings start over.
    pub fn reset(&self) {
        self.peak.store(0.0_f32.to_bits(), Ordering::Relaxed);
        self.signal.store(false, Ordering::Relaxed);
    }

    /// Reference `_process_audio`: the block's absolute peak over the int16
    /// range, clamped to one, and whether it cleared the silence floor.
    pub fn observe(&self, samples: &[i16]) {
        let Some(maximum) = samples.iter().map(|sample| sample.unsigned_abs()).max() else {
            return;
        };
        let peak = (f32::from(maximum) / INT16_ABS_MAX).min(1.0);
        self.peak.store(peak.to_bits(), Ordering::Relaxed);
        if peak > SILENCE_PEAK_THRESHOLD {
            self.signal.store(true, Ordering::Relaxed);
        }
    }

    #[must_use]
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak.load(Ordering::Relaxed))
    }

    #[must_use]
    pub fn has_signal(&self) -> bool {
        self.signal.load(Ordering::Relaxed)
    }
}

/// A streaming linear resampler over mono frames, carrying its position and
/// the last frame of the previous block so consecutive blocks join without a
/// seam.
#[derive(Debug, Clone)]
pub struct LinearResampler {
    step: f64,
    identity: bool,
    position: f64,
    previous: Option<i16>,
}

impl LinearResampler {
    #[must_use]
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        Self {
            step: f64::from(from_rate.max(1)) / f64::from(to_rate.max(1)),
            identity: from_rate == to_rate,
            position: 0.0,
            previous: None,
        }
    }

    /// Appends to `output` every frame of the target rate that `input`
    /// completes.
    pub fn process(&mut self, input: &[i16], output: &mut Vec<i16>) {
        if self.identity {
            output.extend_from_slice(input);
            return;
        }
        let frames = self
            .previous
            .iter()
            .copied()
            .chain(input.iter().copied())
            .collect::<Vec<_>>();
        let Some(last) = frames.last().copied() else {
            return;
        };
        let span = (frames.len() - 1) as f64;
        while self.position < span {
            let index = self.position.floor();
            let fraction = self.position - index;
            // `index` is a non-negative whole number below `span`.
            let index = index as usize;
            let (left, right) = (f64::from(frames[index]), f64::from(frames[index + 1]));
            output.push((left + (right - left) * fraction).round() as i16);
            self.position += self.step;
        }
        self.position -= span;
        self.previous = Some(last);
    }
}

/// Mixes interleaved frames down to one channel by averaging them.
pub fn downmix<T>(samples: &[T], channels: usize, output: &mut Vec<i16>)
where
    T: Sample + Copy,
    i16: FromSample<T>,
{
    let channels = channels.max(1);
    output.extend(samples.chunks_exact(channels).map(|frame| {
        let sum = frame
            .iter()
            .map(|sample| i32::from(i16::from_sample(*sample)))
            .sum::<i32>();
        // The mean of `channels` int16 values is itself an int16 value.
        (sum / i32::try_from(frame.len()).unwrap_or(1)) as i16
    }));
}

/// The block length CPAL is asked for: 200 ms of frames, within the range the
/// device accepts, or the device's default when it states none.
#[must_use]
pub fn block_frames(sample_rate: u32, supported: &SupportedBufferSize) -> cpal::BufferSize {
    let wanted = sample_rate.saturating_mul(CAPTURE_BUFFER_MS) / 1000;
    match supported {
        SupportedBufferSize::Range { min, max } if *min <= *max => {
            cpal::BufferSize::Fixed(wanted.clamp(*min, *max))
        }
        _ => cpal::BufferSize::Default,
    }
}

struct ActiveCapture {
    id: u64,
    stream: Stream,
    sender: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>>,
    started: Instant,
    /// Dropping it releases the expiry thread early.
    _expiry: std::sync::mpsc::Sender<()>,
}

impl ActiveCapture {
    /// Reference `_stop_stream` followed by `_push_sentinel`: no block is
    /// delivered after this, and the consumer sees the stream end once it has
    /// drained what was queued.
    fn close(self) -> Duration {
        let duration = self.started.elapsed();
        drop(self.stream);
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        }
        duration
    }
}

/// [`AudioRecorder`] over the default CPAL host.
#[derive(Default)]
pub struct CpalRecorder {
    active: Arc<Mutex<Option<ActiveCapture>>>,
    meter: Arc<SignalMeter>,
    next_id: AtomicU64,
}

impl CpalRecorder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn open(
        &self,
        sample_rate: u32,
        sender: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>>,
    ) -> Result<Stream, RecorderError> {
        let host = cpal::default_host();
        // Reference `_guard_audio_input`: a host that lists no capture device,
        // or cannot list them at all, has no input.
        let listed = host
            .input_devices()
            .map_err(|_| RecorderError::NoInputDevice)?
            .next()
            .is_some();
        if !listed {
            return Err(RecorderError::NoInputDevice);
        }
        let device = host
            .default_input_device()
            .ok_or(RecorderError::NoInputDevice)?;
        let supported = select_input_config(&device, sample_rate)?;
        let device_rate = supported.sample_rate();
        let channels = usize::from(supported.channels());
        let mut config: StreamConfig = supported.config();
        config.buffer_size = block_frames(device_rate, supported.buffer_size());
        let capture = CaptureSink {
            channels,
            resampler: LinearResampler::new(device_rate, sample_rate),
            meter: Arc::clone(&self.meter),
            sender,
        };
        let stream = match supported.sample_format() {
            SampleFormat::I8 => build::<i8>(&device, &config, capture),
            SampleFormat::I16 => build::<i16>(&device, &config, capture),
            SampleFormat::I32 => build::<i32>(&device, &config, capture),
            SampleFormat::I64 => build::<i64>(&device, &config, capture),
            SampleFormat::U8 => build::<u8>(&device, &config, capture),
            SampleFormat::U16 => build::<u16>(&device, &config, capture),
            SampleFormat::U32 => build::<u32>(&device, &config, capture),
            SampleFormat::U64 => build::<u64>(&device, &config, capture),
            SampleFormat::F32 => build::<f32>(&device, &config, capture),
            SampleFormat::F64 => build::<f64>(&device, &config, capture),
            format => {
                return Err(RecorderError::BackendUnavailable(format!(
                    "the input device delivers the unsupported sample format `{format}`"
                )));
            }
        }?;
        stream
            .play()
            .map_err(|error| RecorderError::BackendUnavailable(error.to_string()))?;
        Ok(stream)
    }

    /// Reference `_start_max_duration_timer`: a recording still running when
    /// the timer fires is stopped exactly as `stop` stops it.
    fn arm_expiry(&self, id: u64) -> std::sync::mpsc::Sender<()> {
        let (release, released) = std::sync::mpsc::channel::<()>();
        let active = Arc::clone(&self.active);
        std::thread::spawn(move || {
            if released.recv_timeout(MAX_RECORDING_DURATION)
                != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            {
                return;
            }
            let expired = active.lock().ok().and_then(|mut slot| {
                slot.as_ref()
                    .is_some_and(|capture| capture.id == id)
                    .then(|| slot.take())
                    .flatten()
            });
            if let Some(capture) = expired {
                capture.close();
            }
        });
        release
    }

    fn take_active(&self) -> Option<ActiveCapture> {
        self.active.lock().ok().and_then(|mut slot| slot.take())
    }
}

impl AudioRecorder for CpalRecorder {
    fn start(&self, sample_rate: u32) -> Result<AudioStream, RecorderError> {
        let mut slot = self.active.lock().map_err(|_| {
            RecorderError::BackendUnavailable("the recorder is poisoned".to_owned())
        })?;
        if slot.is_some() {
            return Err(RecorderError::AlreadyRecording);
        }
        self.meter.reset();
        let (sender, stream) = mpsc::unbounded_channel();
        let sender = Arc::new(Mutex::new(Some(sender)));
        let device_stream = self.open(sample_rate, Arc::clone(&sender))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        *slot = Some(ActiveCapture {
            id,
            stream: device_stream,
            sender,
            started: Instant::now(),
            _expiry: self.arm_expiry(id),
        });
        Ok(stream)
    }

    fn stop(&self) -> Duration {
        self.take_active()
            .map_or(Duration::ZERO, ActiveCapture::close)
    }

    fn cancel(&self) {
        if let Some(capture) = self.take_active() {
            capture.close();
        }
    }

    fn peak(&self) -> f32 {
        self.meter.peak()
    }

    fn has_signal(&self) -> bool {
        self.meter.has_signal()
    }
}

/// What the device callback writes into.
struct CaptureSink {
    channels: usize,
    resampler: LinearResampler,
    meter: Arc<SignalMeter>,
    sender: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>>,
}

impl CaptureSink {
    fn accept<T>(&mut self, samples: &[T])
    where
        T: Sample + Copy,
        i16: FromSample<T>,
    {
        let mut mono = Vec::with_capacity(samples.len() / self.channels.max(1));
        downmix(samples, self.channels, &mut mono);
        let mut block = Vec::with_capacity(mono.len());
        self.resampler.process(&mono, &mut block);
        if block.is_empty() {
            return;
        }
        self.meter.observe(&block);
        let bytes = block
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        if let Ok(sender) = self.sender.lock()
            && let Some(sender) = sender.as_ref()
        {
            let _ = sender.send(bytes);
        }
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut sink: CaptureSink,
) -> Result<Stream, RecorderError>
where
    T: Sample + SizedSample + Copy,
    i16: FromSample<T>,
{
    device
        .build_input_stream(
            *config,
            move |samples: &[T], _| sink.accept(samples),
            |_error| {},
            None,
        )
        .map_err(|error| RecorderError::BackendUnavailable(error.to_string()))
}

/// The configuration the device is opened in: one that runs at the requested
/// rate when the device offers it, fewest channels and int16 first, and the
/// device's default otherwise, whose frames are then resampled.
fn select_input_config(
    device: &cpal::Device,
    sample_rate: u32,
) -> Result<SupportedStreamConfig, RecorderError> {
    let exact = device.supported_input_configs().ok().and_then(|ranges| {
        ranges
            .filter(|range| !range.sample_format().is_dsd())
            .filter(|range| {
                range.min_sample_rate() <= sample_rate && sample_rate <= range.max_sample_rate()
            })
            .min_by_key(|range| {
                (
                    range.channels(),
                    u8::from(range.sample_format() != SampleFormat::I16),
                )
            })
            .and_then(|range| range.try_with_sample_rate(sample_rate))
    });
    match exact {
        Some(config) => Ok(config),
        None => device
            .default_input_config()
            .map_err(|error| RecorderError::BackendUnavailable(error.to_string())),
    }
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod capture_tests;
