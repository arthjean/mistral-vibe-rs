use std::time::Duration;

use serde_json::json;
use vibe_core::platform::Platform;
use vibe_core::tools::shell::HostShells;

use super::*;

/// The shell context a session on `host` hands its processes.
fn context(workspace: &Path, host: HostShells, configured: Option<&str>) -> ShellContext {
    ShellContext {
        cwd: workspace.to_path_buf(),
        host,
        shell: configured.map(str::to_owned),
        env: BTreeMap::new(),
    }
}

/// Waits for `process_id` to leave `running` and returns its whole output.
#[cfg(windows)]
fn settled_output(manager: &ProcessManager, process_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    while manager.public()[0]["status"] == "running" && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    structured(manager.output(&json!({"processId": process_id})))
}

fn structured(result: ToolResult) -> Value {
    match result {
        ToolResult::Success {
            structured_content: Some(value),
            ..
        } => value,
        other => unreachable!("expected a success, got {other:?}"),
    }
}

#[test]
fn a_process_id_hashes_the_session_and_the_call() {
    let id = process_id("session", "action", "call");
    assert!(id.starts_with(&format!("process-{}-", &hex_digest(b"session")[..12])));
    assert_eq!(id.len(), "process-".len() + 12 + 1 + 24);
    assert_eq!(id, process_id("session", "action", "call"));
}

#[cfg(unix)]
#[test]
fn a_process_runs_on_a_terminal_and_pages_its_output() {
    let manager = ProcessManager::new();
    let workspace = tempfile::tempdir().unwrap();
    let started = structured(manager.start(
        "process-a".to_owned(),
        &json!({"command": "printf 'one\\ntwo\\n'"}),
        workspace.path(),
        None,
        &context(workspace.path(), HostShells::detect(), None),
    ));
    assert_eq!(started["status"], "running");
    let deadline = Instant::now() + Duration::from_secs(10);
    while manager.public()[0]["status"] == "running" && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let all = structured(manager.output(&json!({"processId": "process-a"})));
    assert_eq!(all["output"], "one\r\ntwo\r\n");
    assert_eq!(all["status"], "completed");
    assert_eq!(all["exitCode"], 0);
    let tail = structured(
        manager.output(&json!({"processId": "process-a", "from": "end", "maxBytes": 4})),
    );
    assert_eq!(
        (
            tail["output"].as_str(),
            tail["hasMore"].as_bool(),
            tail["nextCursor"].as_u64()
        ),
        (Some("wo\r\n"), Some(true), Some(10))
    );
    let page =
        structured(manager.output(&json!({"processId": "process-a", "cursor": 2, "maxBytes": 3})));
    assert_eq!(
        (page["output"].as_str(), page["nextCursor"].as_u64()),
        (Some("e\r\n"), Some(5))
    );
    let notifications = manager.take_notifications();
    assert_eq!(
        notifications[0]["message"],
        "Background process process-a completed."
    );
}

#[cfg(unix)]
#[test]
fn a_stopped_process_reports_stopped_and_refuses_input() {
    let manager = ProcessManager::new();
    let workspace = tempfile::tempdir().unwrap();
    let _ = manager.start(
        "process-b".to_owned(),
        &json!({"command": "sleep 30"}),
        workspace.path(),
        None,
        &context(workspace.path(), HostShells::detect(), None),
    );
    let stopped = structured(manager.stop(&json!({"processId": "process-b"})));
    assert_eq!(
        stopped,
        json!({"processId": "process-b", "status": "stopped", "exitCode": null})
    );
    let ToolResult::Failure { code, .. } =
        manager.write(&json!({"processId": "process-b", "text": "x"}))
    else {
        unreachable!("a stopped process takes no input");
    };
    assert_eq!(code, "process_not_running");
    let ToolResult::Failure { code, .. } = manager.output(&json!({"processId": "process-missing"}))
    else {
        unreachable!("an unknown process has no output");
    };
    assert_eq!(code, "unknown_process");
}

#[test]
fn a_configured_shell_that_does_not_exist_fails_the_start_naming_it() {
    let manager = ProcessManager::new();
    let workspace = tempfile::tempdir().unwrap();
    let missing =
        workspace
            .path()
            .join("missing")
            .join(if cfg!(windows) { "bash.exe" } else { "zsh" });
    let missing = missing.to_string_lossy().into_owned();
    let mut host = HostShells::detect();
    // A Windows host resolves the configured shell whichever family it
    // publishes, so the case holds without Git Bash installed too.
    if host.platform == Platform::Windows {
        host.git_bash = Some(PathBuf::from("C:/Program Files/Git/bin/bash.exe"));
    }
    let shell = context(workspace.path(), host, Some(&missing));
    let ToolResult::Failure {
        code,
        message,
        details,
        ..
    } = manager.start(
        "process-missing-shell".to_owned(),
        &json!({"command": "echo hi"}),
        workspace.path(),
        None,
        &shell,
    )
    else {
        unreachable!("a missing shell cannot start a process");
    };
    assert_eq!(code, "process_start_failed");
    assert_eq!(
        details,
        Some(json!({"processId": "process-missing-shell", "stage": "shell_resolution"}))
    );
    assert!(message.contains(&missing), "{message}");
    // The shell tool resolves through the same ladder and refuses the same
    // path, which off Windows it reports as reference
    // `PosixTerminalBackend.resolve_shell` words it, naming no path.
    let refused = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(crate::tools::shell::execute(
            &json!({"command": "echo hi"}),
            &shell,
        ));
    let ToolResult::Failure { message, .. } = refused else {
        unreachable!("the shell tool cannot run under a missing shell");
    };
    if cfg!(windows) {
        assert!(message.contains(&missing), "{message}");
    } else {
        assert_eq!(message, "configured shell is not executable");
    }
}

#[cfg(windows)]
#[test]
fn a_windows_process_runs_under_git_bash_when_the_host_has_it() {
    let host = HostShells::detect();
    let Some(git_bash) = host.git_bash.clone() else {
        eprintln!("skipping: no Git Bash on this host");
        return;
    };
    assert_eq!(
        host.command_environment(),
        vibe_core::tools::shell::CommandEnvironment::GitBash
    );
    let manager = ProcessManager::new();
    let workspace = tempfile::tempdir().unwrap();
    let shell = context(workspace.path(), host, None);
    let resolved = shell
        .host
        .command_shell(None, &BTreeMap::new())
        .expect("Git Bash resolves");
    assert_eq!(
        (resolved.executable, resolved.arguments),
        (git_bash, vec!["-c".to_owned()])
    );
    let started = structured(manager.start(
        "process-git-bash".to_owned(),
        &json!({"command": "echo hi"}),
        workspace.path(),
        None,
        &shell,
    ));
    assert_eq!(started["status"], "running");
    let all = settled_output(&manager, "process-git-bash");
    assert_eq!(
        (all["status"].as_str(), all["exitCode"].as_i64()),
        (Some("completed"), Some(0))
    );
    assert!(
        all["output"].as_str().unwrap_or_default().contains("hi"),
        "{all}"
    );
}

#[cfg(windows)]
#[test]
fn a_windows_process_runs_under_powershell_without_git_bash() {
    let detected = HostShells::detect();
    let Some(powershell) = detected.powershell.clone() else {
        eprintln!("skipping: no PowerShell on this host");
        return;
    };
    let host = HostShells {
        git_bash: None,
        ..detected
    };
    assert_eq!(
        host.command_environment(),
        vibe_core::tools::shell::CommandEnvironment::PowerShell
    );
    let manager = ProcessManager::new();
    let workspace = tempfile::tempdir().unwrap();
    let shell = context(workspace.path(), host, None);
    let resolved = shell
        .host
        .command_shell(None, &BTreeMap::new())
        .expect("PowerShell resolves");
    assert_eq!(
        (resolved.executable, resolved.arguments),
        (
            powershell,
            vec![
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-Command".to_owned()
            ]
        )
    );
    let started = structured(manager.start(
        "process-powershell".to_owned(),
        &json!({"command": "Write-Output hi"}),
        workspace.path(),
        None,
        &shell,
    ));
    assert_eq!(started["status"], "running");
    let all = settled_output(&manager, "process-powershell");
    assert_eq!(
        (all["status"].as_str(), all["exitCode"].as_i64()),
        (Some("completed"), Some(0))
    );
    assert!(
        all["output"].as_str().unwrap_or_default().contains("hi"),
        "{all}"
    );
}

/// US-017: stopping a process ends the job it left running in the
/// background, within two seconds.
#[cfg(windows)]
#[test]
fn a_stopped_windows_process_leaves_no_descendant() {
    let host = HostShells::detect();
    if host.git_bash.is_none() {
        eprintln!("skipping: no Git Bash on this host");
        return;
    }
    let alive = |pid: u32| {
        let listed = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .expect("tasklist runs");
        String::from_utf8_lossy(&listed.stdout).contains(&format!("\"{pid}\""))
    };
    let manager = ProcessManager::new();
    let workspace = tempfile::tempdir().unwrap();
    let shell = context(workspace.path(), host, None);
    let _ = manager.start(
        "process-tree".to_owned(),
        &json!({"command": "sleep 300 & cat /proc/$!/winpid; sleep 300"}),
        workspace.path(),
        None,
        &shell,
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let descendant = loop {
        let page = structured(manager.output(&json!({"processId": "process-tree"})));
        if let Some(pid) = page["output"]
            .as_str()
            .unwrap_or_default()
            .lines()
            .find_map(|line| line.trim().parse::<u32>().ok())
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "no background job: {page}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(alive(descendant), "the background job never ran");
    let stopped = structured(manager.stop(&json!({"processId": "process-tree"})));
    assert_eq!(stopped["status"], "stopped", "{stopped}");
    let deadline = Instant::now() + Duration::from_secs(2);
    while alive(descendant) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(descendant), "the background job survived the stop");
}
