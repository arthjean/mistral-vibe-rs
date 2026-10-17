//! A session under ConPTY returns everything its child wrote and leaves no
//! descendant behind, as a session under a POSIX terminal does (US-017).
//!
//! The children run under Git Bash, whose `/proc/<pid>/winpid` names the
//! Windows process behind a background job; a host without it skips.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{ProcessSpec, TerminalManager, TerminalState};

fn git_bash() -> Option<PathBuf> {
    crate::tools::shell::HostShells::detect().git_bash
}

fn terminal_spec(bash: PathBuf, script: &str, cwd: &std::path::Path) -> ProcessSpec {
    let mut spec = ProcessSpec::new(bash, cwd);
    spec.arguments = vec!["-c".to_owned(), script.to_owned()];
    spec.terminal = true;
    spec.max_output_bytes = usize::MAX;
    spec
}

/// Whether the Windows process `pid` is still running, as `tasklist` sees it.
fn alive(pid: u32) -> bool {
    let listed = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .expect("tasklist runs");
    String::from_utf8_lossy(&listed.stdout).contains(&format!("\"{pid}\""))
}

/// Reads the session until its output holds a line that is a number: the
/// Windows ID of the background job the script started.
async fn background_pid(manager: &TerminalManager, terminal_id: &str) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let read = manager.read(terminal_id).await.expect("the session reads");
        seen.extend(read.chunks.into_iter().flat_map(|chunk| chunk.bytes));
        if let Some(pid) = String::from_utf8_lossy(&seen)
            .lines()
            .find_map(|line| line.trim().parse::<u32>().ok())
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    unreachable!(
        "the script never printed its background job: {}",
        String::from_utf8_lossy(&seen)
    );
}

async fn assert_gone_within_two_seconds(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(pid), "process {pid} survived its session");
}

/// The reader reaches the end of a mebibyte within two seconds of the child's
/// exit, drops nothing and loses no byte, in fifty runs out of fifty.
#[tokio::test]
async fn a_conpty_session_returns_its_whole_output_every_time() {
    const LINES: usize = 25_000;
    // Forty characters, ended by the terminal's CRLF.
    const LINE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let Some(bash) = git_bash() else {
        eprintln!("skipping: no Git Bash on this host");
        return;
    };
    let directory = tempfile::tempdir().expect("tempdir");
    let script = format!("yes {LINE} | head -n {LINES}");
    for run in 0..50 {
        let manager = TerminalManager::default();
        let terminal_id = manager
            .run(terminal_spec(bash.clone(), &script, directory.path()))
            .await
            .expect("the session starts");
        let mut output = Vec::new();
        let mut dropped = false;
        let exited = loop {
            let read = manager.read(&terminal_id).await.expect("the session reads");
            dropped |= read.backpressure_dropped;
            output.extend(read.chunks.into_iter().flat_map(|chunk| chunk.bytes));
            if read.state != TerminalState::Running {
                break Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let last = manager
            .wait(&terminal_id)
            .await
            .expect("the session drains");
        let drained = exited.elapsed();
        dropped |= last.backpressure_dropped;
        output.extend(last.chunks.into_iter().flat_map(|chunk| chunk.bytes));
        manager
            .release(&terminal_id)
            .await
            .expect("the session releases");
        assert!(
            drained < Duration::from_secs(2),
            "run {run}: the reader ended {drained:?} after the exit"
        );
        assert!(!dropped, "run {run}: output was dropped");
        assert_eq!(
            output.len(),
            LINES * (LINE.len() + 2),
            "run {run}: {} bytes",
            output.len()
        );
        assert!(
            output
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .all(|line| line == format!("{LINE}\r").as_bytes()),
            "run {run}: a line was altered"
        );
    }
}

/// Killing a session ends the background job its child started.
#[tokio::test]
async fn an_interrupted_conpty_session_leaves_no_descendant() {
    let Some(bash) = git_bash() else {
        eprintln!("skipping: no Git Bash on this host");
        return;
    };
    let directory = tempfile::tempdir().expect("tempdir");
    let manager = TerminalManager::default();
    let terminal_id = manager
        .run(terminal_spec(
            bash,
            "sleep 300 & cat /proc/$!/winpid; sleep 300",
            directory.path(),
        ))
        .await
        .expect("the session starts");
    let descendant = background_pid(&manager, &terminal_id).await;
    assert!(alive(descendant), "the background job never ran");
    manager
        .interrupt(&terminal_id)
        .await
        .expect("the session is interrupted");
    manager
        .release(&terminal_id)
        .await
        .expect("the session releases");
    assert_gone_within_two_seconds(descendant).await;
}

/// A job the child left running when it exited is ended once the session is
/// released, through the pseudoconsole it is still attached to.
#[tokio::test]
async fn a_released_conpty_session_ends_the_job_its_child_left_behind() {
    let Some(bash) = git_bash() else {
        eprintln!("skipping: no Git Bash on this host");
        return;
    };
    let directory = tempfile::tempdir().expect("tempdir");
    let manager = TerminalManager::default();
    let terminal_id = manager
        .run(terminal_spec(
            bash,
            "sleep 300 & cat /proc/$!/winpid",
            directory.path(),
        ))
        .await
        .expect("the session starts");
    let descendant = background_pid(&manager, &terminal_id).await;
    manager.wait(&terminal_id).await.expect("the session exits");
    manager
        .release(&terminal_id)
        .await
        .expect("the session releases");
    assert_gone_within_two_seconds(descendant).await;
}
