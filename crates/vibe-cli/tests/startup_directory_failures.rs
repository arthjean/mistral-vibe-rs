//! What a launch prints when the directories it was pointed at are not there.
//!
//! The reference reports all three of these on standard output and exits 1,
//! and it spells the two paths differently on purpose: `--workdir` is printed
//! resolved and `--add-dir` exactly as it was typed
//! (`vibe/cli/entrypoint.py:279-325`). Which stream carried a line only exists
//! at the process boundary, so this drives the `vibe` binary and reads back
//! what it wrote. The worktree half of the same contract is measured by
//! `worktree_lifecycle.rs`.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "an integration failure must terminate with the output the launch produced"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A `--workdir` that is not a directory names the flag and the path once it
/// was made absolute, not the spelling that was typed.
#[test]
fn a_missing_workdir_is_reported_resolved() {
    let root = tempfile::tempdir().expect("fixture root");
    let workspace = canonical(root.path());
    let output = launch(root.path(), &workspace, &["--workdir", "there/../missing"]);

    let stdout = report(&output);
    let expected = workspace.join("missing");
    assert_eq!(output.status.code(), Some(1), "stdout: {stdout}");
    assert!(
        stdout.contains(&format!(
            "Error: --workdir does not exist or is not a directory: {}",
            expected.display()
        )),
        "the resolved path must be named, stdout: {stdout}"
    );
    assert_no_operating_system_error(&output);
}

/// `--workdir` expands a leading tilde before it asks the filesystem, so the
/// path it reports sits under the home rather than under a directory called
/// `~`.
#[test]
fn a_tilde_workdir_expands_before_the_existence_check() {
    let root = tempfile::tempdir().expect("fixture root");
    let output = launch(root.path(), root.path(), &["--workdir", "~/missing"]);

    let stdout = report(&output);
    let home = canonical(&root.path().join("home"));
    assert_eq!(output.status.code(), Some(1), "stdout: {stdout}");
    assert!(
        stdout.contains(&home.join("missing").display().to_string()),
        "the tilde must expand to the home, stdout: {stdout}"
    );
    assert!(
        !stdout.contains("~/missing"),
        "the unexpanded spelling must not be reported, stdout: {stdout}"
    );
}

/// An `--add-dir` that is not a directory is reported with the argument as it
/// was typed, which is the half of the pair the reference does not resolve.
#[test]
fn a_missing_add_dir_is_reported_as_it_was_typed() {
    let root = tempfile::tempdir().expect("fixture root");
    let workspace = canonical(root.path());
    let output = launch(root.path(), &workspace, &["--add-dir", "./missing"]);

    let stdout = report(&output);
    assert_eq!(output.status.code(), Some(1), "stdout: {stdout}");
    assert!(
        stdout.contains("Error: --add-dir path does not exist or is not a directory: ./missing"),
        "the typed argument must be named, stdout: {stdout}"
    );
    assert!(
        !stdout.contains(&workspace.join("missing").display().to_string()),
        "the resolved path must not be reported, stdout: {stdout}"
    );
    assert_no_operating_system_error(&output);
}

/// Several `--add-dir` values are checked in the order they were given, so the
/// failure names the one that is wrong rather than reporting them together.
#[test]
fn the_first_bad_add_dir_is_the_one_reported() {
    let root = tempfile::tempdir().expect("fixture root");
    let workspace = canonical(root.path());
    fs::create_dir_all(workspace.join("present")).expect("present directory");
    let output = launch(
        root.path(),
        &workspace,
        &["--add-dir", "present", "--add-dir", "second-missing"],
    );

    let stdout = report(&output);
    assert_eq!(output.status.code(), Some(1), "stdout: {stdout}");
    assert!(
        stdout
            .contains("Error: --add-dir path does not exist or is not a directory: second-missing"),
        "the second value must be the one named, stdout: {stdout}"
    );
    assert!(
        !stdout.contains("present"),
        "the value that resolved must not be named, stdout: {stdout}"
    );
}

/// A working directory deleted underneath the shell is explained in two lines,
/// the second of which offers the flag that works around it.
#[test]
fn a_deleted_working_directory_explains_itself_and_offers_workdir() {
    if !Path::new("/bin/sh").exists() {
        eprintln!("skipping: no POSIX shell to leave a process in a deleted directory");
        return;
    }
    let root = tempfile::tempdir().expect("fixture root");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("home directory");
    let gone = canonical(root.path()).join("gone");
    fs::create_dir_all(&gone).expect("doomed directory");

    // A process only holds a deleted working directory when the directory went
    // away after it moved there, so the shell moves in, removes it, and hands
    // the launch its own working directory.
    let output = Command::new("/bin/sh")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("VIBE_HOME", home.join(".vibe"))
        .env("TERM", "dumb")
        .arg("-c")
        .arg(format!(
            "cd '{gone}' && rmdir '{gone}' && exec '{binary}' --prompt hi",
            gone = gone.display(),
            binary = env!("CARGO_BIN_EXE_vibe"),
        ))
        .stdin(Stdio::null())
        .output()
        .expect("the shell launched vibe");

    let stdout = report(&output);
    assert_eq!(output.status.code(), Some(1), "stdout: {stdout}");
    let mut lines = stdout
        .lines()
        .skip_while(|line| !line.starts_with("Error: Current working directory"));
    let first = lines.next().unwrap_or_else(|| {
        panic!("the deletion must be stated first, stdout: {stdout}");
    });
    assert_eq!(
        first, "Error: Current working directory no longer exists.",
        "stdout: {stdout}"
    );
    let second = lines
        .next()
        .unwrap_or_else(|| panic!("the way out must follow, stdout: {stdout}"));
    assert!(
        second.contains("--workdir"),
        "the second line must offer --workdir, stdout: {stdout}"
    );
    assert_no_operating_system_error(&output);
}

/// Every startup failure the corpus recorded, replayed against the binary.
///
/// The corpus stores the exit code and which stream carried the report, and
/// that pair only exists at a process boundary, so these cases are replayed
/// here rather than in the in-process replay that reads the rest of the corpus
/// (`src/cli_surface_parity_tests.rs`). The sentences are not compared: they
/// are the reference's own, so the corpus never carried them, and the tests
/// above measure the ones this port publishes instead.
#[test]
fn every_recorded_startup_failure_replays_at_the_process_boundary() {
    let corpus: serde_json::Value =
        serde_json::from_str(include_str!("runtime-parity/cli-surface.json"))
            .expect("the corpus parses");
    let cases: Vec<&serde_json::Value> = corpus["cases"]
        .as_array()
        .expect("the corpus lists its cases")
        .iter()
        .filter(|case| case["parser"] == "startup")
        .collect();
    assert_eq!(
        cases.len(),
        4,
        "the corpus no longer carries the four startup vectors"
    );
    for case in cases {
        let id = case["case"].as_str().expect("a case carries an id");
        let argv: Vec<&str> = case["argv"]
            .as_array()
            .expect("a case carries its argv")
            .iter()
            .map(|word| word.as_str().expect("an argv word is a string"))
            .collect();
        let Some(output) = replay_startup(id, &argv) else {
            eprintln!("skipping {id}: no POSIX shell to leave a process in a deleted directory");
            continue;
        };
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert_eq!(
            output.status.code().map(i64::from),
            case["exit"].as_i64(),
            "{id} exited differently, stdout: {stdout}, stderr: {stderr}"
        );
        assert_eq!(
            !stdout.is_empty(),
            case["streams"]["stdout"],
            "{id} disagrees on standard output, stdout: {stdout}"
        );
        assert_eq!(
            !stderr.is_empty(),
            case["streams"]["stderr"],
            "{id} disagrees on standard error, stderr: {stderr}"
        );
    }
}

/// One recorded vector, launched from the directory its argv was captured in.
///
/// Three of them name something that has to be missing beside something that
/// has to be there, so the fixture holds the one name they expect to resolve.
/// The fourth is about the launch directory itself, which no argument can
/// express, so a shell moves in, removes it, and hands the launch what is left.
/// That route needs a shell, and returns `None` where there is none.
fn replay_startup(case: &str, argv: &[&str]) -> Option<Output> {
    let root = tempfile::tempdir().expect("fixture root");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("home directory");
    let workspace = canonical(root.path()).join("workspace");
    fs::create_dir_all(&workspace).expect("workspace directory");
    let mut command = if case == "startup-working-directory-deleted" {
        if !Path::new("/bin/sh").exists() {
            return None;
        }
        let words = argv
            .iter()
            .map(|word| format!(" '{word}'"))
            .collect::<String>();
        let mut shell = Command::new("/bin/sh");
        shell.arg("-c").arg(format!(
            "cd '{workspace}' && rmdir '{workspace}' && exec '{binary}'{words}",
            workspace = workspace.display(),
            binary = env!("CARGO_BIN_EXE_vibe"),
        ));
        shell
    } else {
        fs::create_dir_all(workspace.join("present")).expect("the present directory");
        let mut launch = Command::new(env!("CARGO_BIN_EXE_vibe"));
        launch.current_dir(&workspace).args(argv);
        launch
    };
    Some(
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("VIBE_HOME", home.join(".vibe"))
            .env("TERM", "dumb")
            .stdin(Stdio::null())
            .output()
            .expect("vibe launched"),
    )
}

/// The standard output a launch produced, asserting that nothing about the
/// failure also reached standard error.
fn report(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !stderr.contains("Error: "),
        "the failure belongs on standard output, stderr: {stderr}"
    );
    stdout
}

/// None of these failures may fall back on the operating system's own wording,
/// which names an error number a reader cannot act on.
fn assert_no_operating_system_error(output: &Output) {
    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !both.contains("os error") && !both.contains("startup I/O failed"),
        "a generic I/O sentence reached the user: {both}"
    );
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("the fixture path resolves")
}

/// One programmatic launch, with the environment reduced to what startup reads
/// and no provider credential in reach.
fn launch(home_root: &Path, workspace: &Path, arguments: &[&str]) -> Output {
    let home = home_root.join("home");
    fs::create_dir_all(&home).expect("home directory");
    Command::new(env!("CARGO_BIN_EXE_vibe"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("VIBE_HOME", home.join(".vibe"))
        .env("TERM", "dumb")
        .current_dir(workspace)
        .args(arguments)
        .args(["--prompt", "hi"])
        .stdin(Stdio::null())
        .output()
        .expect("vibe launched")
}
