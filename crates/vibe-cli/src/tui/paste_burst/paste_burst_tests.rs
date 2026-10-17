use crossterm::event::KeyEventKind;

use super::*;

/// How far apart a console paste delivers its key events.
const PASTED: Duration = Duration::from_micros(20);

fn press(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn release(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new_with_kind(
        code,
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ))
}

fn key_of(character: char) -> KeyCode {
    match character {
        '\n' => KeyCode::Enter,
        '\t' => KeyCode::Tab,
        '\u{1b}' => KeyCode::Esc,
        character => KeyCode::Char(character),
    }
}

/// Types `text` the way conhost pastes it: a press and a release per
/// character, `spacing` apart. Answers when the last key arrived.
fn type_text(detector: &mut PasteBurst, text: &str, start: Instant, spacing: Duration) -> Instant {
    let mut now = start;
    for character in text.chars() {
        detector.push(press(key_of(character)), now);
        detector.push(release(key_of(character)), now);
        now += spacing;
    }
    now - spacing
}

fn drain(detector: &mut PasteBurst) -> Vec<Event> {
    std::iter::from_fn(|| detector.next_ready()).collect()
}

#[test]
fn a_fifty_line_paste_lands_as_one_paste_and_submits_nothing() {
    let mut detector = PasteBurst::new(true);
    let pasted = (1..=50)
        .map(|line| format!("at frame {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let start = Instant::now();
    let last = type_text(&mut detector, &pasted, start, PASTED);
    assert!(
        drain(&mut detector).is_empty(),
        "the burst was released early"
    );
    assert_eq!(detector.deadline(), Some(last + BURST_SILENCE));

    detector.expire(last + BURST_SILENCE);
    assert_eq!(drain(&mut detector), vec![Event::Paste(pasted)]);
    assert!(!detector.is_collecting());
}

#[test]
fn a_ten_thousand_character_paste_lands_within_a_second() {
    let mut detector = PasteBurst::new(true);
    let pasted = "0123456789".repeat(1_000);
    let started = Instant::now();
    let last = type_text(&mut detector, &pasted, started, PASTED);
    detector.expire(last + BURST_SILENCE);
    let events = drain(&mut detector);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the paste took {:?}",
        started.elapsed()
    );
    assert_eq!(events, vec![Event::Paste(pasted)]);
}

#[test]
fn typing_twenty_milliseconds_apart_reaches_the_session_as_keys() {
    let mut detector = PasteBurst::new(true);
    let start = Instant::now();
    let last = type_text(&mut detector, "hi\n", start, Duration::from_millis(20));
    detector.expire(last + BURST_WINDOW);
    let keys = drain(&mut detector)
        .into_iter()
        .filter(|event| matches!(event, Event::Key(key) if key.kind == KeyEventKind::Press))
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        vec![
            press(KeyCode::Char('h')),
            press(KeyCode::Char('i')),
            press(KeyCode::Enter),
        ]
    );
}

#[test]
fn a_held_key_waits_one_window_at_most() {
    let mut detector = PasteBurst::new(true);
    let start = Instant::now();
    detector.push(press(KeyCode::Char('x')), start);
    assert_eq!(detector.deadline(), Some(start + BURST_WINDOW));
    detector.expire(start + BURST_WINDOW - Duration::from_millis(1));
    assert!(drain(&mut detector).is_empty());
    detector.expire(start + BURST_WINDOW);
    assert_eq!(drain(&mut detector), vec![press(KeyCode::Char('x'))]);
    assert_eq!(detector.deadline(), None);
}

#[test]
fn tab_and_escape_inside_a_paste_neither_complete_nor_cancel() {
    let mut detector = PasteBurst::new(true);
    let start = Instant::now();
    let last = type_text(&mut detector, "@src\tnext\u{1b}end", start, PASTED);
    detector.expire(last + BURST_SILENCE);
    assert_eq!(
        drain(&mut detector),
        vec![Event::Paste("@src\tnextend".to_owned())]
    );
}

#[test]
fn a_trailing_newline_is_part_of_the_paste() {
    let mut detector = PasteBurst::new(true);
    let start = Instant::now();
    let last = type_text(&mut detector, "first\nsecond\n", start, PASTED);
    detector.expire(last + BURST_SILENCE);
    assert_eq!(
        drain(&mut detector),
        vec![Event::Paste("first\nsecond\n".to_owned())]
    );
}

#[test]
fn a_key_no_paste_types_closes_the_burst_before_it_is_handled() {
    let mut detector = PasteBurst::new(true);
    let start = Instant::now();
    let last = type_text(&mut detector, "abc", start, PASTED);
    detector.push(press(KeyCode::Backspace), last + PASTED);
    assert_eq!(
        drain(&mut detector),
        vec![Event::Paste("abc".to_owned()), press(KeyCode::Backspace)]
    );
}

#[test]
fn chunks_within_the_silence_extend_one_paste() {
    let mut detector = PasteBurst::new(true);
    let start = Instant::now();
    let first = type_text(&mut detector, "one\n", start, PASTED);
    let resumed = first + BURST_SILENCE - Duration::from_millis(1);
    let last = type_text(&mut detector, "two", resumed, PASTED);
    detector.expire(last + BURST_SILENCE);
    assert_eq!(
        drain(&mut detector),
        vec![Event::Paste("one\ntwo".to_owned())]
    );
}

#[test]
fn a_paste_read_after_a_slow_repaint_stays_one_paste() {
    let mut detector = PasteBurst::new(true);
    let start = Instant::now();
    let first = type_text(&mut detector, "first half ", start, PASTED);
    // The session repainted for longer than the silence before it read on.
    let resumed = first + BURST_SILENCE * 3;
    let last = type_text(&mut detector, "second half", resumed, PASTED);
    detector.expire(last + BURST_SILENCE);
    assert_eq!(
        drain(&mut detector),
        vec![Event::Paste("first half second half".to_owned())]
    );
}

#[test]
fn a_disabled_detector_passes_every_event_through_in_order() {
    let mut detector = PasteBurst::new(false);
    let start = Instant::now();
    let events = vec![
        press(KeyCode::Char('a')),
        Event::Paste("bracketed\npaste\n".to_owned()),
        press(KeyCode::Enter),
    ];
    for event in events.clone() {
        detector.push(event, start);
    }
    assert!(!detector.is_collecting());
    assert_eq!(detector.deadline(), None);
    assert_eq!(drain(&mut detector), events);
}
