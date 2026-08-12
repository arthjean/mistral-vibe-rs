//! What the corpus cannot reach: the sink, its rotation, and the reader over a
//! file this process wrote rather than one the capture authored.

use std::collections::BTreeMap;
use std::fs;

use super::*;

fn environment(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn reader_over(lines: &[&str]) -> (tempfile::TempDir, FileLog) {
    let enclosure = tempfile::tempdir().expect("a log enclosure");
    let log = FileLog::in_home(enclosure.path(), LogSettings::default());
    fs::create_dir_all(log.path().parent().expect("the log directory")).expect("the directory");
    fs::write(log.path(), format!("{}\n", lines.join("\n"))).expect("the log file");
    (enclosure, log)
}

#[test]
fn a_written_record_reads_back_as_the_entry_it_was() {
    let enclosure = tempfile::tempdir().expect("a log enclosure");
    let log = FileLog::in_home(enclosure.path(), LogSettings::default());
    log.write(LogLevel::Error, "the turn failed\nafter one step", None)
        .expect("the record is written");

    let page = log.reader().get_logs(LOG_DEFAULT_PAGE_LIMIT, 0);
    let entry = page.entries.first().expect("one entry");
    let (ppid, pid) = process_identifiers();
    assert_eq!(entry.level, "ERROR");
    assert_eq!(entry.message, "the turn failed\\nafter one step");
    assert_eq!(
        decode_log_message(&entry.message),
        "the turn failed\nafter one step",
        "the newline survives the round trip rather than splitting the line"
    );
    assert_eq!(entry.pid, i64::from(pid));
    assert_eq!(entry.ppid, i64::from(ppid));
    assert!(!page.has_more);
    assert_eq!(page.cursor, None);
    assert_eq!(
        entry_identity(&entry.raw_line),
        entry_identity(&entry.raw_line),
        "the identity is a function of the line alone"
    );
}

#[test]
fn a_record_below_the_resolved_level_is_not_written() {
    let enclosure = tempfile::tempdir().expect("a log enclosure");
    let log = FileLog::in_home(
        enclosure.path(),
        LogSettings {
            level: LogLevel::Error,
            max_bytes: LOG_DEFAULT_MAX_BYTES,
        },
    );
    log.write(LogLevel::Warning, "quiet", None)
        .expect("the record is dropped");
    log.write(LogLevel::Critical, "loud", None)
        .expect("the record is written");

    let page = log.reader().get_logs(LOG_DEFAULT_PAGE_LIMIT, 0);
    assert_eq!(
        page.entries
            .iter()
            .map(|entry| entry.message.as_str())
            .collect::<Vec<_>>(),
        vec!["loud"]
    );
}

#[test]
fn the_file_rotates_at_its_ceiling_and_keeps_nothing_behind() {
    let enclosure = tempfile::tempdir().expect("a log enclosure");
    let log = FileLog::new(
        enclosure.path().join("vibe.log"),
        LogSettings {
            level: LogLevel::Debug,
            max_bytes: 200,
        },
    );
    for index in 0..8 {
        log.write(LogLevel::Info, &format!("record {index}"), None)
            .expect("the record is written");
    }
    let size = fs::metadata(log.path()).expect("the log file").len();
    assert!(size < 200, "the ceiling holds: {size} bytes");
    let page = log.reader().get_logs(LOG_DEFAULT_PAGE_LIMIT, 0);
    assert_eq!(
        page.entries
            .first()
            .map(|entry| entry.message.clone())
            .unwrap_or_default(),
        "record 7",
        "the newest record survives the rotation"
    );
}

#[test]
fn an_unwritable_path_fails_initialization_without_installing_anything() {
    let enclosure = tempfile::tempdir().expect("a log enclosure");
    let occupied = enclosure.path().join("logs");
    fs::write(&occupied, "not a directory").expect("the blocking file");
    let error = init_file_logging(&occupied.join("vibe.log"), &|_| None)
        .expect_err("a file where the directory belongs cannot be initialized");
    assert!(
        matches!(error, LogInitError::Directory { .. }),
        "the failure names the directory: {error}"
    );
}

#[test]
fn a_rotation_ceiling_that_is_not_a_number_fails_after_the_directory_is_made() {
    let enclosure = tempfile::tempdir().expect("a log enclosure");
    let path = enclosure.path().join("logs").join("vibe.log");
    let variables = environment(&[(LOG_MAX_BYTES_VARIABLE, "not-a-number")]);
    let error = init_file_logging(&path, &|name| variables.get(name).cloned())
        .expect_err("the ceiling is refused");
    assert!(
        matches!(error, LogInitError::Settings(_)),
        "the failure names the setting: {error}"
    );
    assert!(
        path.parent().is_some_and(Path::is_dir),
        "the reference creates the directory before it reads the ceiling"
    );
}

#[test]
fn the_environment_decides_the_level_and_the_ceiling() {
    for (variables, level, max_bytes) in [
        (Vec::new(), LogLevel::Warning, LOG_DEFAULT_MAX_BYTES),
        (
            vec![(LOG_LEVEL_VARIABLE, "info")],
            LogLevel::Info,
            LOG_DEFAULT_MAX_BYTES,
        ),
        (
            vec![(LOG_LEVEL_VARIABLE, "TRACE")],
            LogLevel::Warning,
            LOG_DEFAULT_MAX_BYTES,
        ),
        (
            vec![(LOG_LEVEL_VARIABLE, "ERROR"), (DEBUG_MODE_VARIABLE, "true")],
            LogLevel::Debug,
            LOG_DEFAULT_MAX_BYTES,
        ),
        (
            vec![(LOG_LEVEL_VARIABLE, "ERROR"), (DEBUG_MODE_VARIABLE, "1")],
            LogLevel::Error,
            LOG_DEFAULT_MAX_BYTES,
        ),
        (
            vec![(LOG_MAX_BYTES_VARIABLE, "4096")],
            LogLevel::Warning,
            4_096,
        ),
    ] {
        let variables = environment(&variables);
        let settings = LogSettings::resolve(&|name| variables.get(name).cloned())
            .expect("the settings resolve");
        assert_eq!(settings.level, level, "{variables:?}");
        assert_eq!(settings.max_bytes, max_bytes, "{variables:?}");
    }
}

#[test]
fn a_page_walks_backward_over_more_than_one_chunk() {
    let enclosure = tempfile::tempdir().expect("a log enclosure");
    let log = FileLog::new(
        enclosure.path().join("vibe.log"),
        LogSettings {
            level: LogLevel::Debug,
            max_bytes: 0,
        },
    );
    // Past the 8 KiB read window, so the chunked walk and its carried remainder
    // are what answer rather than one read.
    for index in 0..400 {
        log.write(LogLevel::Info, &format!("record {index}"), None)
            .expect("the record is written");
    }
    let size = fs::metadata(log.path()).expect("the log file").len();
    assert!(
        size > u64::try_from(LOG_READ_CHUNK_SIZE).unwrap_or_default(),
        "the file spans more than one chunk: {size} bytes"
    );

    let page = log.reader().get_logs(5, 0);
    assert_eq!(
        page.entries
            .iter()
            .map(|entry| entry.message.clone())
            .collect::<Vec<_>>(),
        vec![
            "record 399",
            "record 398",
            "record 397",
            "record 396",
            "record 395"
        ]
    );
    assert!(page.has_more);
    assert_eq!(page.cursor, Some(5));

    let oldest = log.reader().get_logs(2, 398);
    assert_eq!(
        oldest
            .entries
            .iter()
            .map(|entry| entry.message.clone())
            .collect::<Vec<_>>(),
        vec!["record 1", "record 0"],
        "the last page reaches the first line written"
    );
    // A page that filled its limit reports more even when the file holds
    // nothing older, which is what the reference answers: the emptiness is
    // discovered by the read that follows.
    assert!(oldest.has_more);
    assert_eq!(oldest.cursor, Some(400));
    let past_the_end = log.reader().get_logs(2, 400);
    assert_eq!(past_the_end, LogPage::default());
}

#[test]
fn a_file_that_shrank_since_the_last_read_is_read_from_its_new_end() {
    let (_enclosure, log) = reader_over(&[
        "2026-02-21T10:28:51.529718+00:00 12 34 WARNING first",
        "2026-02-21T10:28:52.529718+00:00 12 34 ERROR second",
        "2026-02-21T10:28:53.529718+00:00 12 34 ERROR third",
    ]);
    let before = log.reader().get_logs(LOG_DEFAULT_PAGE_LIMIT, 0);
    assert_eq!(before.entries.len(), 3);

    // Reference resets a held position when the file shrank. This reader holds
    // none: every page seeks the end again, so a truncated file answers its own
    // content rather than a window past its end.
    fs::write(
        log.path(),
        "2026-02-21T10:28:51.529718+00:00 12 34 WARNING first\n",
    )
    .expect("the truncated file");
    let after = log.reader().get_logs(LOG_DEFAULT_PAGE_LIMIT, 0);
    assert_eq!(
        after
            .entries
            .iter()
            .map(|entry| (entry.message.clone(), entry.line_number))
            .collect::<Vec<_>>(),
        vec![("first".to_owned(), 1)]
    );
    assert!(!after.has_more);
    assert_eq!(after.cursor, None);
}

#[test]
fn an_absent_file_answers_an_empty_page() {
    let enclosure = tempfile::tempdir().expect("a log enclosure");
    let page = LogReader::new(enclosure.path().join("absent.log")).get_logs(10, 0);
    assert_eq!(page, LogPage::default());
}

#[test]
fn a_line_the_pattern_refuses_is_skipped_rather_than_failing_the_page() {
    let (_enclosure, log) = reader_over(&[
        "2026-02-21T10:28:51.529718+00:00 12 34 WARNING first",
        "an operator pasted this",
        "",
        "2026-02-21T10:28:52.529718+00:00 12 34 ERROR second",
    ]);
    let page = log.reader().get_logs(LOG_DEFAULT_PAGE_LIMIT, 0);
    assert_eq!(
        page.entries
            .iter()
            .map(|entry| (entry.message.clone(), entry.line_number))
            .collect::<Vec<_>>(),
        vec![("second".to_owned(), 1), ("first".to_owned(), 3)],
        "the unparsed line still advances the numbering it occupies"
    );
}

#[test]
fn the_pattern_publishes_the_five_captures_an_entry_is_built_from() {
    assert_eq!(log_pattern_groups(), 5);
}
