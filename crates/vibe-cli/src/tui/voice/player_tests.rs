//! Decoding and frame writing, both measurable without an audio device.

use super::*;

/// A 16-bit PCM WAVE container, built the way a speech response carries one.
fn wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
    wav_with(sample_rate, channels, 16, 1, samples, &[])
}

/// The same container with a declared bit depth, a declared format tag and an
/// optional chunk sitting between the header and the data.
fn wav_with(
    sample_rate: u32,
    channels: u16,
    bits: u16,
    tag: u16,
    samples: &[i16],
    extra_chunk: &[u8],
) -> Vec<u8> {
    let mut data = Vec::new();
    for sample in samples {
        data.extend_from_slice(&sample.to_le_bytes());
    }
    let block_align = channels * bits / 8;
    let mut format = Vec::new();
    format.extend_from_slice(&tag.to_le_bytes());
    format.extend_from_slice(&channels.to_le_bytes());
    format.extend_from_slice(&sample_rate.to_le_bytes());
    format.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
    format.extend_from_slice(&block_align.to_le_bytes());
    format.extend_from_slice(&bits.to_le_bytes());

    let mut body = Vec::new();
    body.extend_from_slice(b"WAVE");
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(
        &u32::try_from(format.len())
            .unwrap_or_default()
            .to_le_bytes(),
    );
    body.extend_from_slice(&format);
    if !extra_chunk.is_empty() {
        body.extend_from_slice(b"LIST");
        body.extend_from_slice(
            &u32::try_from(extra_chunk.len())
                .unwrap_or_default()
                .to_le_bytes(),
        );
        body.extend_from_slice(extra_chunk);
        // RIFF pads an odd-sized chunk to an even boundary, which is the rule
        // the decoder walks the container by.
        if extra_chunk.len() % 2 == 1 {
            body.push(0);
        }
    }
    body.extend_from_slice(b"data");
    body.extend_from_slice(&u32::try_from(data.len()).unwrap_or_default().to_le_bytes());
    body.extend_from_slice(&data);

    let mut container = Vec::new();
    container.extend_from_slice(b"RIFF");
    container.extend_from_slice(&u32::try_from(body.len()).unwrap_or_default().to_le_bytes());
    container.extend_from_slice(&body);
    container
}

/// Reference `decode_wav`: the rate, the channel count and the payload are read
/// off the container rather than assumed from the shipped defaults.
#[test]
fn the_container_supplies_the_rate_the_channels_and_the_payload() {
    let samples = [1_i16, -1, 2_000, -2_000];
    let decoded = decode_wav(&wav(22_050, 2, &samples)).expect("a PCM container decodes");
    assert_eq!(decoded.sample_rate, 22_050);
    assert_eq!(decoded.channels, 2);
    assert_eq!(decoded.samples, samples);

    let mono = decode_wav(&wav(8_000, 1, &samples)).expect("a mono container decodes");
    assert_eq!(mono.sample_rate, 8_000);
    assert_eq!(mono.channels, 1);
}

/// A container that carries chunks between its header and its data is still
/// decoded from the two chunks that matter.
#[test]
fn a_chunk_between_the_header_and_the_data_is_skipped() {
    let decoded = decode_wav(&wav_with(16_000, 1, 16, 1, &[7_i16, 8], b"INFOfixture"))
        .expect("the extra chunk is skipped");
    assert_eq!(decoded.sample_rate, 16_000);
    assert_eq!(decoded.samples, [7_i16, 8]);
}

/// A payload that is not a container this port decodes is refused by cause, and
/// the refusal happens before any device is addressed: `decode_wav` opens
/// nothing at all.
#[test]
fn an_unsupported_payload_is_refused_by_cause() {
    let mp3 = [
        0xFF_u8, 0xFB, 0x90, 0x44, 0x00, 0x00, 0x00, 0x00, 0, 0, 0, 0,
    ];
    assert!(matches!(
        decode_wav(&mp3),
        Err(PlaybackError::UnsupportedFormat(_))
    ));
    assert!(matches!(
        decode_wav(&[]),
        Err(PlaybackError::UnsupportedFormat(_))
    ));
    // A WAVE the reference's own `wave` module refuses too: a compressed tag.
    let compressed = wav_with(16_000, 1, 16, 0x0055, &[1_i16], &[]);
    assert!(matches!(
        decode_wav(&compressed),
        Err(PlaybackError::UnsupportedFormat(_))
    ));
    // A depth this port's `int16` output cannot carry.
    let deep = wav_with(16_000, 1, 24, 1, &[1_i16], &[]);
    let error = decode_wav(&deep).expect_err("a 24-bit container is refused");
    assert!(
        error.to_string().contains(PLAYBACK_SAMPLE_FORMAT),
        "{error}"
    );
}

fn feed(
    samples: Vec<i16>,
    source_channels: usize,
    device_channels: usize,
) -> (Feed, watch::Receiver<bool>) {
    let (completion, finished) = CompletionSignal::channel();
    (
        Feed {
            samples: Arc::new(samples),
            cursor: Arc::new(AtomicUsize::new(0)),
            source_channels,
            device_channels,
            completion,
        },
        finished,
    )
}

/// The buffer is written frame by frame, a mono source is heard on every device
/// channel, and the exhausted tail is silence rather than a repeat.
#[test]
fn frames_are_written_until_the_buffer_is_exhausted() {
    let (source, mut finished) = feed(vec![10_i16, 20, 30], 1, 2);
    let mut output = [0_i16; 4];
    write_frames(&source, &mut output);
    assert_eq!(output, [10, 10, 20, 20]);
    assert!(!*finished.borrow_and_update(), "one frame is still pending");

    let mut output = [7_i16; 4];
    write_frames(&source, &mut output);
    assert_eq!(output, [30, 30, 0, 0], "the tail is padded with silence");
    assert!(
        *finished.borrow_and_update(),
        "the exhausted buffer raises its completion"
    );

    // A second callback on an exhausted buffer keeps writing silence rather than
    // replaying the payload.
    let mut output = [7_i16; 2];
    write_frames(&source, &mut output);
    assert_eq!(output, [0, 0]);
}

/// A stereo source keeps its channels where the device has them.
#[test]
fn a_multi_channel_source_keeps_its_channels() {
    let (source, _finished) = feed(vec![1_i16, 2, 3, 4], 2, 2);
    let mut output = [0_i16; 4];
    write_frames(&source, &mut output);
    assert_eq!(output, [1, 2, 3, 4]);
}

/// The three playback constants are the reference's own.
#[test]
fn the_playback_constants_match_the_reference() {
    assert_eq!(PLAYBACK_BLOCK_SIZE, 4_096);
    assert_eq!(PLAYBACK_SAMPLE_FORMAT, "int16");
    assert_eq!(PLAYBACK_SAMPLE_WIDTH, 2);
}

/// A default output device commonly names configurations in formats the stream
/// builder refuses, and one of those is what a real host selected before the
/// format became part of the selection. Only a format this port writes is
/// eligible, and the reference's own `int16` is preferred over the rest.
#[test]
fn only_a_writable_sample_format_is_eligible_and_int16_comes_first() {
    for refused in [
        SampleFormat::U8,
        SampleFormat::I8,
        SampleFormat::U32,
        SampleFormat::U64,
        SampleFormat::I64,
    ] {
        assert_eq!(
            output_format_rank(refused),
            None,
            "`{refused}` is not a format `build_output_stream` handles"
        );
    }
    let ranks = OUTPUT_SAMPLE_FORMATS.map(|format| {
        output_format_rank(format).unwrap_or_else(|| panic!("`{format}` ranks itself"))
    });
    assert_eq!(ranks, [0, 1, 2, 3, 4]);
    assert_eq!(
        OUTPUT_SAMPLE_FORMATS[0],
        SampleFormat::I16,
        "the reference opens its stream in int16"
    );
}
