use super::*;

#[test]
fn an_identity_resampler_passes_every_frame_through() {
    let mut resampler = LinearResampler::new(16_000, 16_000);
    let mut output = Vec::new();
    resampler.process(&[1, 2, 3], &mut output);
    assert_eq!(output, [1, 2, 3]);
}

#[test]
fn downsampling_keeps_the_target_rate_across_blocks() {
    // 48 kHz to 16 kHz: one frame out for every three in, whatever the block
    // boundaries are.
    let mut resampler = LinearResampler::new(48_000, 16_000);
    let input = (0..4_800)
        .map(|index| (index % 300) as i16)
        .collect::<Vec<_>>();
    let mut joined = Vec::new();
    for block in input.chunks(437) {
        resampler.process(block, &mut joined);
    }
    let mut whole = Vec::new();
    LinearResampler::new(48_000, 16_000).process(&input, &mut whole);
    assert_eq!(joined, whole);
    assert!((1_599..=1_600).contains(&joined.len()), "{}", joined.len());
}

#[test]
fn upsampling_interpolates_between_neighbours() {
    let mut resampler = LinearResampler::new(8_000, 16_000);
    let mut output = Vec::new();
    resampler.process(&[0, 100, 200], &mut output);
    assert_eq!(output, [0, 50, 100, 150]);
}

#[test]
fn channels_are_averaged_into_one() {
    let mut mono = Vec::new();
    downmix(&[100_i16, 300, -50, 50], 2, &mut mono);
    assert_eq!(mono, [200, 0]);
}

#[test]
fn the_meter_reports_the_block_peak_and_latches_a_signal() {
    let meter = SignalMeter::default();
    meter.observe(&[0, 0, 0]);
    assert!(!meter.has_signal());
    assert!(meter.peak().abs() < f32::EPSILON);
    // 32 over 32 767 is just below the floor, 33 just above it.
    meter.observe(&[32, -32]);
    assert!(!meter.has_signal());
    meter.observe(&[-33]);
    assert!(meter.has_signal());
    meter.observe(&[i16::MIN]);
    assert!((meter.peak() - 1.0).abs() < f32::EPSILON);
    meter.observe(&[0]);
    assert!(
        meter.has_signal(),
        "a signal stays detected until the next start"
    );
    meter.reset();
    assert!(!meter.has_signal());
}

#[test]
fn a_block_is_two_hundred_milliseconds_within_the_device_range() {
    let range = SupportedBufferSize::Range {
        min: 64,
        max: 8_192,
    };
    assert_eq!(block_frames(16_000, &range), cpal::BufferSize::Fixed(3_200));
    assert_eq!(block_frames(48_000, &range), cpal::BufferSize::Fixed(8_192));
    assert_eq!(
        block_frames(48_000, &SupportedBufferSize::Unknown),
        cpal::BufferSize::Default
    );
}
