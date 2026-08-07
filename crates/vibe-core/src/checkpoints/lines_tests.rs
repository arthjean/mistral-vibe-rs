//! What Python's line splitting does that a naive split does not.

use super::lines::{FileState, decode_lines, split_lines};

#[test]
fn a_newline_ends_a_line_and_stays_on_it() {
    assert_eq!(split_lines("one\ntwo\n"), vec!["one\n", "two\n"]);
    assert_eq!(
        split_lines("one\ntwo\n"),
        "one\ntwo\n".split_inclusive('\n').collect::<Vec<_>>()
    );
}

#[test]
fn every_boundary_character_python_splits_on_ends_a_line() {
    for separator in [
        '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}', '\u{2029}',
    ] {
        let text = format!("head{separator}tail");
        assert_eq!(
            split_lines(&text),
            vec![format!("head{separator}"), "tail".to_owned()],
            "U+{:04X} did not end a line",
            u32::from(separator)
        );
    }
}

#[test]
fn a_carriage_return_and_a_newline_are_one_boundary() {
    assert_eq!(
        split_lines("one\r\ntwo\rthree"),
        vec!["one\r\n", "two\r", "three"]
    );
}

#[test]
fn an_empty_string_holds_no_line_at_all() {
    assert!(split_lines("").is_empty());
}

#[test]
fn a_final_line_without_a_separator_is_still_a_line() {
    assert_eq!(split_lines("one\ntwo"), vec!["one\n", "two"]);
}

#[test]
fn joining_the_lines_reproduces_the_input() {
    for text in [
        "",
        "\n",
        "one",
        "one\n",
        "one\ntwo\nthree",
        "\n\n\n",
        "head\u{c}tail\u{2028}end\n",
        "trailing spaces   \n\tindented\n",
        "\u{85}leading boundary",
    ] {
        assert_eq!(
            split_lines(text).concat(),
            text,
            "round trip failed on {text:?}"
        );
    }
}

#[test]
fn an_absent_file_has_no_lines_and_does_not_fail() {
    let absent = FileState::absent();
    assert!(!absent.exists());
    assert!(!absent.is_binary());
    assert_eq!(decode_lines(&absent), Some(Vec::new()));
}

#[test]
fn a_file_carrying_a_nul_signals_binary_rather_than_returning_lines() {
    let binary = FileState::from_bytes(b"before\0after\n".to_vec());
    assert!(binary.exists());
    assert!(binary.is_binary());
    assert_eq!(decode_lines(&binary), None);
}

#[test]
fn an_empty_file_exists_and_holds_no_line() {
    let empty = FileState::from_text("");
    assert!(empty.exists());
    assert!(!empty.is_binary());
    assert_eq!(decode_lines(&empty), Some(Vec::new()));
}

#[test]
fn line_endings_are_normalized_before_the_split() {
    // `decode` collapses CRLF to LF, so the lines a region addresses are the
    // same whichever ending the file was written with.
    let crlf = FileState::from_bytes(b"one\r\ntwo\r\n".to_vec());
    assert_eq!(
        decode_lines(&crlf),
        Some(vec!["one\n".to_owned(), "two\n".to_owned()])
    );
}
