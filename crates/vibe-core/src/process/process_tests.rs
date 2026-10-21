use super::*;
use tempfile::tempdir;

#[tokio::test]
async fn managed_process_streams_and_reaps_owned_group() {
    let directory = tempdir().expect("tempdir");
    let manager = TerminalManager::default();
    let mut spec = ProcessSpec::new("/bin/sh", directory.path());
    spec.arguments = vec!["-c".to_owned(), "printf out; printf err >&2".to_owned()];
    let terminal_id = manager.run(spec).await.expect("spawn");
    let output = manager.wait(&terminal_id).await.expect("wait");
    assert_eq!(
        output.state,
        TerminalState::Exited {
            code: Some(0),
            success: true,
            signal: None,
        }
    );
    let stdout = output
        .chunks
        .iter()
        .filter(|chunk| chunk.stream == ProcessStream::Stdout)
        .flat_map(|chunk| chunk.bytes.iter().copied())
        .collect::<Vec<_>>();
    let stderr = output
        .chunks
        .iter()
        .filter(|chunk| chunk.stream == ProcessStream::Stderr)
        .flat_map(|chunk| chunk.bytes.iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(stdout, b"out");
    assert_eq!(stderr, b"err");
    manager.release(&terminal_id).await.expect("release");
    assert!(manager.list().await.is_empty());
}

#[tokio::test]
async fn release_reaps_descendants_after_process_group_leader_exits() {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let directory = tempdir().expect("tempdir");
    let manager = TerminalManager::with_cleanup_grace(Duration::from_millis(200));
    let mut spec = ProcessSpec::new("/bin/sh", directory.path());
    spec.arguments = vec![
        "-c".to_owned(),
        "sleep 30 </dev/null >/dev/null 2>&1 & printf %s $!".to_owned(),
    ];
    let terminal_id = manager.run(spec).await.expect("spawn");
    let output = manager.wait(&terminal_id).await.expect("wait");
    let descendant = String::from_utf8(
        output
            .chunks
            .iter()
            .filter(|chunk| chunk.stream == ProcessStream::Stdout)
            .flat_map(|chunk| chunk.bytes.iter().copied())
            .collect(),
    )
    .expect("pid")
    .parse::<i32>()
    .expect("numeric pid");
    manager.release(&terminal_id).await.expect("release");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        match kill(Pid::from_raw(descendant), None) {
            Err(Errno::ESRCH) => break,
            Ok(()) | Err(Errno::EPERM) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            result => {
                let gone = matches!(&result, Err(Errno::ESRCH));
                assert!(gone, "descendant remained after release: {result:?}");
                break;
            }
        }
    }
}

#[tokio::test]
async fn cleanup_signals_all_processes_and_leaves_no_handles() {
    let directory = tempdir().expect("tempdir");
    let manager = TerminalManager::with_cleanup_grace(Duration::from_millis(200));
    for _ in 0..2 {
        let mut spec = ProcessSpec::new("/bin/sh", directory.path());
        spec.arguments = vec!["-c".to_owned(), "sleep 30".to_owned()];
        manager.run(spec).await.expect("spawn");
    }
    let outcomes = manager.cleanup_all().await.expect("cleanup");
    assert_eq!(outcomes.len(), 2);
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome.state, TerminalState::Interrupted { .. }))
    );
}
