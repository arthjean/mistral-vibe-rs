use std::path::PathBuf;
use std::sync::Arc;

use libtest_mimic::Arguments;

use super::{Binaries, Ctx, Jail, Scenario, Shared, Subject, Suite, execute};

fn shared(artifacts: Option<PathBuf>) -> Arc<Shared> {
    Arc::new(Shared {
        binaries: Binaries::default(),
        artifacts,
        jail: Jail::Unjailed,
    })
}

fn body(f: impl Fn(&mut Ctx) -> crate::Outcome + Send + Sync + 'static) -> super::Body {
    Box::new(f)
}

#[cfg(unix)]
#[test]
fn a_failed_trial_leaves_its_artifacts_with_credentials_redacted() {
    let target = tempfile_dir();
    let roots = Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = Arc::clone(&roots);
    let failing = body(move |ctx| {
        let world = ctx.world()?;
        kept.lock().unwrap().push(world.root().to_path_buf());
        world.write("vibe-home/logs/vibe.log", "a log line")?;
        world
            .program("sh")
            .args(["-c", "echo to-stdout; echo to-stderr >&2"])
            .env("SECRET_TOKEN", "hunter2")
            .run()?;
        Err(crate::Failure::new("the scenario's own assertion failed"))
    });
    let error = execute(
        "self::failing trial",
        &failing,
        &shared(Some(target.clone())),
    )
    .unwrap_err();
    let message = error.message().unwrap();
    assert!(
        message.contains("the scenario's own assertion failed"),
        "{message}"
    );
    let directory = target.join("self__failing_trial");
    assert!(
        message.contains(&directory.display().to_string()),
        "{message}"
    );
    for file in [
        "failure.txt",
        "journal.json",
        "sentinel.log",
        "config.toml",
        "tree.txt",
        "logs/vibe.log",
    ] {
        assert!(directory.join(file).is_file(), "{file} missing");
    }
    let read = |name: &str| std::fs::read_to_string(directory.join(name)).unwrap();
    assert_eq!(read("child-0-sh.stdout"), "to-stdout\n");
    assert_eq!(read("child-0-sh.stderr"), "to-stderr\n");
    let env = read("child-0-sh.env");
    assert!(env.contains("SECRET_TOKEN=<redacted>"), "{env}");
    assert!(env.contains("E2E_PROVIDER_KEY=<redacted>"), "{env}");
    assert!(
        !env.contains("hunter2") && !env.contains("e2e-key"),
        "{env}"
    );
    assert!(read("tree.txt").contains("vibe-home/config.toml"));
    let size: u64 = walk(&directory)
        .iter()
        .map(|p| p.metadata().unwrap().len())
        .sum();
    assert!(size <= crate::artifacts::BUDGET as u64);
    assert!(roots.lock().unwrap().iter().all(|root| !root.exists()));
    crate::world::remove_tree(&target);
}

#[test]
fn a_passing_trial_leaves_no_artifact_and_no_world() {
    let target = tempfile_dir();
    let roots = Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = Arc::clone(&roots);
    let passing = body(move |ctx| {
        kept.lock().unwrap().push(ctx.world()?.root().to_path_buf());
        Ok(())
    });
    execute("self::passing", &passing, &shared(Some(target.clone()))).unwrap();
    assert!(!target.join("self__passing").exists());
    assert!(roots.lock().unwrap().iter().all(|root| !root.exists()));
    crate::world::remove_tree(&target);
}

#[test]
fn a_panicking_scenario_still_tears_its_world_down() {
    let roots = Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = Arc::clone(&roots);
    #[allow(clippy::panic, reason = "the test provokes a panic on purpose")]
    let panicking = body(move |ctx| {
        kept.lock().unwrap().push(ctx.world()?.root().to_path_buf());
        panic!("boom");
    });
    let error = execute("self::panicking", &panicking, &shared(None)).unwrap_err();
    assert!(error.message().unwrap().contains("panicked: boom"));
    assert!(roots.lock().unwrap().iter().all(|root| !root.exists()));
}

fn tempfile_dir() -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "vibe-e2e-artifacts-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

#[cfg(unix)]
fn walk(directory: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

fn suite() -> Suite {
    Suite::new("self").scenarios([
        Scenario::new("plain", |_| Ok(())),
        Scenario::new("defect", |_| Ok(())).known_defect("E2E-D99", "a cause"),
        Scenario::new("port", |_| Ok(())).port_only("a reason"),
        Scenario::new("jail", |_| Ok(())).needs_jail(),
    ])
}

fn quiet(arguments: &[&str]) -> Arguments {
    Arguments::from_iter(["e2e", "--quiet"].iter().chain(arguments))
}

#[test]
fn a_known_defect_is_ignored_for_the_port_and_announced() {
    let (trials, notices) = suite().trials(Subject::Port, None, Jail::Jailed);
    let ignored: Vec<&str> = trials
        .iter()
        .filter(|t| t.has_ignored_flag())
        .map(|t| t.name())
        .collect();
    assert_eq!(ignored, ["defect"]);
    assert_eq!(notices, ["known defect E2E-D99 (a cause): defect"]);
    let conclusion = libtest_mimic::run(&quiet(&[]), trials);
    assert_eq!(
        (conclusion.num_passed, conclusion.num_ignored),
        jailed_counts(3, 1)
    );
}

/// Passed and ignored counts of a jailed run of [`suite`]: Windows skips the
/// scenario that needs the jail by design.
fn jailed_counts(passed: u64, ignored: u64) -> (u64, u64) {
    if cfg!(windows) {
        (passed - 1, ignored + 1)
    } else {
        (passed, ignored)
    }
}

#[test]
fn the_reference_runs_known_defects_and_skips_port_only_scenarios() {
    let (trials, notices) = suite().trials(
        Subject::Reference,
        Some(PathBuf::from("/ref")),
        Jail::Jailed,
    );
    assert!(trials.iter().all(|t| !t.has_ignored_flag()));
    assert!(notices.is_empty());
    let conclusion = libtest_mimic::run(&quiet(&[]), trials);
    assert_eq!(
        (conclusion.num_passed, conclusion.num_ignored),
        jailed_counts(3, 1)
    );
}

#[test]
fn a_windows_ignore_skips_the_trial_on_windows_only() {
    let (trials, _) = Suite::new("self")
        .scenarios([Scenario::new("tagged", |_| Ok(())).windows_ignore("a reason")])
        .trials(Subject::Port, None, Jail::Jailed);
    let conclusion = libtest_mimic::run(&quiet(&[]), trials);
    let expected = if cfg!(windows) { (0, 1) } else { (1, 0) };
    assert_eq!((conclusion.num_passed, conclusion.num_ignored), expected);
}

#[test]
fn a_scenario_that_needs_the_jail_is_skipped_without_it() {
    let (trials, _) = suite().trials(Subject::Port, None, Jail::Unjailed);
    let conclusion = libtest_mimic::run(&quiet(&["--exact", "jail"]), trials);
    assert_eq!((conclusion.num_passed, conclusion.num_ignored), (0, 1));
}

#[test]
fn a_port_run_resolves_the_reference_for_a_scenario_that_needs_it() {
    let needing = |resolve: fn() -> Result<PathBuf, String>| {
        Suite::new("self")
            .reference(resolve)
            .scenarios([Scenario::new("cross", |ctx| {
                ctx.world()?.command_for(Subject::Reference, "vibe")?;
                Ok(())
            })
            .needs_reference()])
            .trials(Subject::Port, None, Jail::Jailed)
            .0
    };
    let conclusion = libtest_mimic::run(&quiet(&[]), needing(|| Ok(PathBuf::from("/ref"))));
    assert_eq!((conclusion.num_passed, conclusion.num_ignored), (1, 0));
    let conclusion = libtest_mimic::run(
        &quiet(&[]),
        needing(|| Err("the checkout is missing".to_owned())),
    );
    assert_eq!((conclusion.num_passed, conclusion.num_ignored), (0, 1));
}

#[test]
fn a_duplicate_trial_name_is_found() {
    assert_eq!(suite().duplicate(), None);
    let doubled = suite().scenarios([Scenario::new("port", |_| Ok(()))]);
    assert_eq!(doubled.duplicate(), Some("port"));
}

#[test]
fn the_subject_variable_selects_the_port_or_the_resolved_reference() {
    let plain = suite();
    assert_eq!(plain.subject(None).unwrap(), (Subject::Port, None));
    assert_eq!(plain.subject(Some("port")).unwrap(), (Subject::Port, None));
    assert!(
        plain
            .subject(Some("reference"))
            .unwrap_err()
            .contains("declares no reference")
    );
    assert!(
        plain
            .subject(Some("python"))
            .unwrap_err()
            .contains("names no subject")
    );
    let missing = suite().reference(|| Err("no reference checkout at /nowhere".to_owned()));
    let error = missing.subject(Some("reference")).unwrap_err();
    assert!(
        error.contains("no reference checkout at /nowhere"),
        "{error}"
    );
    let pinned = suite().reference(|| Ok(PathBuf::from("/pinned")));
    assert_eq!(
        pinned.subject(Some("reference")).unwrap(),
        (Subject::Reference, Some(PathBuf::from("/pinned")))
    );
}

#[test]
fn a_reference_command_resolves_inside_the_checkout_virtual_environment() {
    let binaries = Binaries {
        subject: Subject::Reference,
        port: [("vibe".to_owned(), PathBuf::from("/port/vibe"))].into(),
        reference: Some(PathBuf::from("/pinned")),
    };
    assert_eq!(
        binaries.resolve(Subject::Reference, "vibe-acp").unwrap(),
        PathBuf::from("/pinned/.venv/bin/vibe-acp")
    );
    assert_eq!(
        binaries.resolve(Subject::Port, "vibe").unwrap(),
        PathBuf::from("/port/vibe")
    );
    assert!(binaries.resolve(Subject::Port, "vibe-acp").is_err());
}
