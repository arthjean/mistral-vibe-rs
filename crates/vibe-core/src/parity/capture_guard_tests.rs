//! The guards that keep a capture from writing a corpus it should not.
//!
//! A driver pointed at this port's binary records the port, so it may never
//! write a path the manifest lists: without `--output` it would fall back to
//! the committed corpus, and the replay would then compare the port with
//! itself. A reference capture pointed off the pin records a commit no replay
//! accepts, so only `repin.py capture --commit` may start one. Both guards run
//! before a driver captures anything, so these tests need no reference
//! checkout: they run every driver that has a port flag, as the scan below
//! finds them, and read what it refused.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::REFERENCE_COMMIT;
use super::manifest::load;
use super::replay::repository;

/// The flags through which a driver drives this port, as
/// `provenance.PORT_FLAGS` lists them.
const PORT_FLAGS: [&str; 6] = [
    "--server",
    "--agent",
    "--binary",
    "--live-binary",
    "--vibe",
    "--vibe-acp",
];

/// Every capture script, and the first port flag it declares.
fn port_drivers() -> Vec<(PathBuf, &'static str)> {
    let root = repository();
    let mut scripts: Vec<PathBuf> = std::fs::read_dir(root.join("scripts/parity"))
        .expect("scripts/parity is readable")
        .flatten()
        .map(|entry| entry.path())
        .collect();
    scripts.extend(
        std::fs::read_dir(root.join("crates/vibe-cli/tests/runtime-parity"))
            .expect("the runtime oracles are readable")
            .flatten()
            .map(|entry| entry.path()),
    );
    scripts.sort();
    scripts
        .into_iter()
        .filter(|path| path.extension().is_some_and(|extension| extension == "py"))
        .filter_map(|path| {
            let source = std::fs::read_to_string(&path).ok()?;
            let flag = PORT_FLAGS.into_iter().find(|flag| {
                source.contains(&format!("add_argument(\"{flag}\""))
                    || source.contains(&format!("add_argument(\n        \"{flag}\""))
            })?;
            Some((path, flag))
        })
        .collect()
}

fn run(script: &Path, arguments: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new("python3");
    command
        .arg(script)
        .args(arguments)
        .current_dir(repository())
        .env_remove("FORCE_COLOR")
        .env_remove("VIBE_PARITY_REPIN_COMMIT");
    for (key, value) in environment {
        command.env(key, value);
    }
    command.output().expect("python3 runs the driver")
}

/// Runs `arguments` against every port driver side by side, and returns
/// each driver with what it answered.
fn every_port_driver(
    arguments: impl Fn(&str) -> Vec<String> + Sync,
) -> Vec<(PathBuf, &'static str, Output)> {
    let drivers = port_drivers();
    assert!(
        drivers.len() >= 17,
        "the scan found {} port drivers; it no longer reads their argument declarations",
        drivers.len()
    );
    std::thread::scope(|scope| {
        let handles: Vec<_> = drivers
            .iter()
            .map(|(script, flag)| {
                let arguments = &arguments;
                scope.spawn(move || {
                    let owned = arguments(flag);
                    let borrowed: Vec<&str> = owned.iter().map(String::as_str).collect();
                    (script.clone(), *flag, run(script, &borrowed, &[]))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a driver thread completes"))
            .collect()
    })
}

#[test]
fn a_port_mode_driver_without_an_output_refuses_before_capturing() {
    let port = repository().join("target/no-such-port-binary");
    let port = port.to_string_lossy().into_owned();
    for (script, flag, output) in every_port_driver(|flag| vec![flag.to_owned(), port.clone()]) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success()
                && stderr.contains("port-mode capture needs `--output` outside the manifest paths")
                && stderr.contains(flag),
            "{} {flag} without --output must refuse before capturing: {stderr}",
            script.display()
        );
    }
}

#[test]
fn a_port_mode_driver_refuses_an_output_the_manifest_lists() {
    let manifest = load();
    let listed = &manifest
        .entries
        .iter()
        .find(|entry| entry.corpus.ends_with("review-parity/corpus.json"))
        .expect("the review corpus is listed")
        .corpus;
    let before = std::fs::read(repository().join(listed)).expect("the corpus is readable");
    let port = repository().join("target/no-such-port-binary");
    let port = port.to_string_lossy().into_owned();
    for (script, flag, output) in every_port_driver(|flag| {
        vec![
            flag.to_owned(),
            port.clone(),
            "--output".to_owned(),
            listed.clone(),
        ]
    }) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success()
                && stderr.contains("would overwrite the committed corpus")
                && stderr.contains(listed.as_str()),
            "{} {flag} --output {listed} must refuse naming the corpus: {stderr}",
            script.display()
        );
    }
    assert_eq!(
        std::fs::read(repository().join(listed)).expect("the corpus is readable"),
        before,
        "a refused capture wrote the corpus"
    );
}

#[test]
fn an_off_pin_reference_capture_runs_only_through_repin() {
    let root = repository();
    let missing = root.join("target/no-such-reference");
    let missing = missing.to_string_lossy().into_owned();
    let cases: [(&str, &[&str]); 3] = [
        ("scripts/parity/tool_config.py", &["--allow-unpinned"]),
        ("scripts/parity/review.py", &["--expected-commit", ""]),
        (
            "scripts/parity/review.py",
            &[
                "--expected-commit",
                "0123456789abcdef0123456789abcdef01234567",
            ],
        ),
    ];
    for (script, flags) in cases {
        let mut arguments = flags.to_vec();
        arguments.extend(["--reference", missing.as_str()]);
        let output = run(&root.join(script), &arguments, &[]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success()
                && stderr.contains("repin.py capture <corpus> --commit <sha>")
                && stderr.contains(REFERENCE_COMMIT),
            "{script} {flags:?} outside repin.py must refuse: {stderr}"
        );
    }

    // Through `repin.py capture --commit` the guard lets the capture through
    // to the checkout, which is missing here: the refusal is about the
    // checkout, not the commit.
    let sanctioned = "0123456789abcdef0123456789abcdef01234567";
    let output = run(
        &root.join("scripts/parity/tool_config.py"),
        &["--allow-unpinned", "--reference", missing.as_str()],
        &[("VIBE_PARITY_REPIN_COMMIT", sanctioned)],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("repin.py capture") && stderr.contains("no reference checkout"),
        "a capture repin.py started reaches the checkout check: {stderr}"
    );
}
