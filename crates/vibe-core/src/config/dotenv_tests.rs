//! The global dotenv file: parsing, precedence, and the paths that carry no
//! file at all.

use std::fs;

use super::dotenv::{DotenvValues, resolve};

#[test]
fn a_variable_absent_from_the_process_is_read_from_the_file() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path();
    fs::write(
        home.join(".env"),
        concat!(
            "# a comment\n",
            "\n",
            "VIBE_DOTENV_FIXTURE=from-file\n",
            "export VIBE_DOTENV_EXPORTED='quoted value'\n",
            "VIBE_DOTENV_DOUBLE=\"line\\nbreak\"\n",
            "VIBE_DOTENV_EMPTY=\n",
            "VIBE_DOTENV_QUOTED_EMPTY=''\n",
            "malformed line without a separator\n",
            "not a key=ignored\n",
        ),
    )
    .expect("dotenv fixture");

    let values = DotenvValues::global(home);

    assert_eq!(
        values.variable("VIBE_DOTENV_FIXTURE").as_deref(),
        Some("from-file")
    );
    assert_eq!(
        values.variable("VIBE_DOTENV_EXPORTED").as_deref(),
        Some("quoted value")
    );
    assert_eq!(
        values.variable("VIBE_DOTENV_DOUBLE").as_deref(),
        Some("line\nbreak")
    );
    // An empty value leaves the variable unset, quoted or not.
    assert_eq!(values.variable("VIBE_DOTENV_EMPTY"), None);
    assert_eq!(values.variable("VIBE_DOTENV_QUOTED_EMPTY"), None);
    // A malformed line is skipped rather than failing the read, and nothing it
    // carried reaches the resolved variables.
    let composed = values.environment();
    assert_eq!(
        composed
            .keys()
            .filter(|key| key.starts_with("VIBE_DOTENV_"))
            .count(),
        3
    );
    assert!(!composed.contains_key("not a key"));
    assert!(!composed.contains_key("malformed line without a separator"));
}

#[test]
fn a_non_empty_process_value_wins_over_the_file() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path();
    // `PATH` is set in every environment this suite runs in, so it stands in
    // for a variable the operator exported.
    let process = std::env::var("PATH").expect("PATH is set");
    fs::write(
        home.join(".env"),
        "PATH=from-file\nVIBE_DOTENV_COMPOSED=composed\n",
    )
    .expect("dotenv fixture");

    let values = DotenvValues::global(home);

    assert_eq!(
        values.variable("PATH").as_deref(),
        Some(process.as_str()),
        "the file value is parsed, it just loses"
    );
    assert_eq!(
        values
            .environment()
            .get("VIBE_DOTENV_COMPOSED")
            .map(String::as_str),
        Some("composed"),
        "the composed environment carries what the process does not set"
    );
}

#[test]
fn an_empty_process_value_falls_back_on_the_file() {
    assert_eq!(
        resolve(Some(String::new()), Some(&"from-file".to_owned())).as_deref(),
        Some("from-file")
    );
    assert_eq!(
        resolve(None, Some(&"from-file".to_owned())).as_deref(),
        Some("from-file")
    );
    assert_eq!(
        resolve(Some("exported".to_owned()), Some(&"from-file".to_owned())).as_deref(),
        Some("exported")
    );
    assert_eq!(resolve(None, None), None);
}

#[test]
fn a_missing_file_yields_no_values_and_no_error() {
    let temporary = tempfile::tempdir().expect("temporary root");

    let values = DotenvValues::global(temporary.path());

    assert_eq!(values, DotenvValues::default());
    assert_eq!(values.variable("VIBE_DOTENV_MISSING"), None);
}

/// The reference accepts a FIFO at the dotenv path so a secret manager can
/// stream the file. The path is opened rather than stat-ed, which is what makes
/// that work.
#[cfg(unix)]
#[test]
fn a_fifo_standing_in_for_the_file_is_read() {
    use std::io::Write as _;
    use std::process::Command;

    let temporary = tempfile::tempdir().expect("temporary root");
    let path = temporary.path().join(".env");
    let created = Command::new("mkfifo")
        .arg(&path)
        .status()
        .is_ok_and(|status| status.success());
    if !created {
        // No `mkfifo` on this host; the open-don't-stat rule is unchanged.
        return;
    }
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        if let Ok(mut file) = fs::OpenOptions::new().write(true).open(&writer_path) {
            let _ = file.write_all(b"VIBE_DOTENV_FIFO=streamed\n");
        }
    });

    let values = DotenvValues::load(&path);
    writer.join().expect("writer thread");

    assert_eq!(
        values.variable("VIBE_DOTENV_FIFO").as_deref(),
        Some("streamed")
    );
}
