//! What a session reports for the terminal behind it.
//!
//! The backend name travels to the model in every session document and into
//! every manifest on disk, so a client can branch on it. That makes the
//! spelling a contract rather than a label, and this module holds it: the names
//! are the reference's, the order is the reference's, and the one rung this
//! port cannot open is recorded rather than silently missing.

use std::collections::BTreeMap;
use std::path::Path;

use super::{POSIX_BACKENDS, PTY_BACKENDS, PtySpec, WINDOWS_BACKENDS, spawn};

/// The vocabulary reference `_posix.py` and `_windows.py` publish, in the order
/// `_windows.py` tries them. A name outside this set is a name no client can
/// branch on, and the lowercase `conpty` this port used to emit is outside it.
const REFERENCE_NAMES: [&str; 3] = ["posix", "ConPTY", "WinPTY"];

#[test]
fn every_backend_is_named_the_way_the_reference_names_it() {
    // Both ladders, not just the host's: the Windows spelling is what a client
    // branches on and it would otherwise be asserted by no runner this project
    // has.
    for backend in POSIX_BACKENDS.iter().chain(WINDOWS_BACKENDS) {
        assert!(
            REFERENCE_NAMES.contains(backend),
            "`{backend}` is not one of the reference's backend names"
        );
    }
    assert_eq!(POSIX_BACKENDS.first(), Some(&"posix"));
    assert_eq!(WINDOWS_BACKENDS.first(), Some(&"ConPTY"));
    let expected = if cfg!(windows) { "ConPTY" } else { "posix" };
    assert_eq!(PTY_BACKENDS.first(), Some(&expected));
}

/// The ladder is tried in the reference's order, and no rung repeats.
#[test]
fn the_ladder_keeps_the_reference_order() {
    for ladder in [POSIX_BACKENDS, WINDOWS_BACKENDS] {
        let positions = ladder
            .iter()
            .map(|backend| REFERENCE_NAMES.iter().position(|name| name == backend))
            .collect::<Vec<_>>();
        assert!(positions.iter().all(Option::is_some), "{ladder:?}");
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "the ladder is out of the reference's order: {ladder:?}"
        );
    }
}

/// `portable-pty` publishes no WinPTY implementation, so the Windows ladder is
/// one rung where the reference's is two. That is an accepted divergence in
/// `docs/parity.md`, and this is what fails if a rung is added without
/// restating the row, or if the row is deleted while the gap stands.
#[test]
fn the_missing_rung_is_the_one_the_scorecard_records() {
    // The Windows ladder by name rather than the host's, so adding `WinPTY` is
    // a failure on the POSIX runner that actually runs this suite.
    assert_eq!(
        WINDOWS_BACKENDS,
        ["ConPTY"],
        "a second backend now starts; restate the `WinPTY` row of `docs/parity.md`"
    );
    let scorecard = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .join("docs/parity.md");
    let document = std::fs::read_to_string(&scorecard).expect("the scorecard is readable");
    assert!(
        document.contains("WinPTY"),
        "the scorecard no longer records the missing backend"
    );
}

/// The name a session reports is the name of the rung that actually took the
/// child, read back through the same call the session manager uses.
#[test]
fn a_started_terminal_reports_the_rung_that_took_it() {
    let program = if cfg!(windows) {
        Path::new("cmd.exe")
    } else {
        Path::new("/bin/sh")
    };
    let arguments = if cfg!(windows) {
        vec!["/c".to_owned(), "exit".to_owned()]
    } else {
        vec!["-c".to_owned(), "exit 0".to_owned()]
    };
    let environment = BTreeMap::new();
    let (backend, mut terminal, streams) = spawn(PtySpec {
        program,
        arguments: &arguments,
        working_directory: &std::env::temp_dir(),
        environment: &environment,
        unset_environment: &[],
    })
    .expect("the host opens a terminal");
    assert_eq!(Some(&backend), PTY_BACKENDS.first());
    drop(streams);
    let _ = terminal.try_wait();
}
