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
#[cfg(windows)]
use crate::child::TerminationError;

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

/// US-017: a pseudoconsole that does not close in time is reported as a
/// failure, and the child's tree is ended all the same, since the tree kill
/// does not wait on the console.
///
/// The grandchild is a native process PowerShell starts, so it is in the
/// leader's tree by Windows parentage: a Git Bash background job is not once
/// Cygwin's `exec` has replaced it, and only the console's close ends that one
/// (`process::conpty_tests`).
#[cfg(windows)]
#[tokio::test]
async fn a_console_that_does_not_close_is_reported_and_the_tree_still_dies() {
    use std::io::Read as _;
    use std::time::{Duration, Instant};

    let Some(powershell) = crate::tools::shell::HostShells::detect().powershell else {
        eprintln!("skipping: no PowerShell on this host");
        return;
    };
    let directory = tempfile::tempdir().expect("tempdir");
    let arguments = vec![
        "-NoLogo".to_owned(),
        "-NoProfile".to_owned(),
        "-Command".to_owned(),
        "$p = Start-Process -NoNewWindow -PassThru ping -ArgumentList '-n','300','127.0.0.1'; Write-Output $p.Id; Start-Sleep 300"
            .to_owned(),
    ];
    let environment = BTreeMap::new();
    let (_, mut terminal, streams) = spawn(PtySpec {
        program: &powershell,
        arguments: &arguments,
        working_directory: directory.path(),
        environment: &environment,
        unset_environment: &[],
    })
    .unwrap_or_else(|failures| unreachable!("no terminal opened: {failures:?}"));
    let (sender, received) = std::sync::mpsc::channel();
    let mut reader = streams.reader;
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut seen = Vec::new();
    let descendant = loop {
        let chunk = received
            .recv_timeout(Duration::from_secs(30))
            .expect("the script prints its background job");
        seen.extend(chunk);
        if let Some(pid) = String::from_utf8_lossy(&seen)
            .lines()
            .find_map(|line| line.trim().parse::<u32>().ok())
        {
            break pid;
        }
    };
    let alive = |pid: u32| {
        let listed = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .expect("tasklist runs");
        String::from_utf8_lossy(&listed.stdout).contains(&format!("\"{pid}\""))
    };
    assert!(alive(descendant), "the background job never ran");

    // The console takes longer to close than the session waits for it.
    let console = terminal.master.take().expect("the console is open");
    terminal.master = Some(Box::new(SlowConsole(Some(console))));
    let failed = terminal.reap_group(Duration::from_millis(500), false).await;
    assert!(
        matches!(failed, Err(TerminationError::Console(_))),
        "{failed:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while alive(descendant) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(descendant), "the background job survived");
}

/// A pseudoconsole whose close outlasts any grace a session gives it.
#[cfg(windows)]
struct SlowConsole(Option<Box<dyn Send>>);

#[cfg(windows)]
impl Drop for SlowConsole {
    fn drop(&mut self) {
        std::thread::sleep(std::time::Duration::from_secs(3));
        drop(self.0.take());
    }
}
