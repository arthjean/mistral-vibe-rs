#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The black-box TUI oracle (`tasks/prd-trustworthy-parity-v3.md`, US-020 and
//! US-021): one keystroke script plays against the v3.1.0 reference client
//! and against this port's `vibe`, each on a pseudo-terminal in a world of its
//! own, and the normalized screens are compared
//! (`vibe_e2e::screens`, which also records what US-019 measured).
//!
//! `capture` is the driver `repin.py capture` runs for the three corpora under
//! `tests/tui-blackbox/`: it plays every script against the attested reference
//! environment of the commit the capture records. Each family's replay plays
//! the same scripts against this port, compares every snapshot as JSON
//! pointers under its ledger and its corpus's measured debt, asserts its line
//! of `docs/parity.md` "Measured figures", and, where this machine holds the
//! pinned environment, recaptures the reference and requires the corpus to be
//! what it still answers. A script the reference cannot run hermetically
//! (the external editor, the clipboard) is port-only: the corpus names it
//! under `unavailable` with the reason, and the replay still plays it against
//! the port.

#[path = "tui_blackbox_parity_tests/families.rs"]
mod families;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use vibe_core::parity::replay::{self, Entry, Tally};
use vibe_e2e::screens::{self, Masks, Stalled};
use vibe_e2e::{Command, Setup, World};

use families::{FAMILIES, Family, Scenario};

/// Which client a script plays against.
enum Subject {
    Port,
    /// The `vibe` console script of an attested environment.
    Reference(PathBuf),
}

impl Subject {
    fn command(&self, world: &World) -> Command {
        let program = match self {
            Self::Port => PathBuf::from(env!("CARGO_BIN_EXE_vibe")),
            Self::Reference(root) => launcher(root),
        };
        world.program(program).current_dir(world.work())
    }
}

/// The console script an environment installs.
fn launcher(root: &Path) -> PathBuf {
    let windows = root.join(".venv/Scripts/vibe.exe");
    if windows.is_file() {
        windows
    } else {
        root.join(".venv/bin/vibe")
    }
}

/// The directory the corpora's worlds were created in, the system temporary
/// directory of the Linux capture host and of the CI runner.
const WORLD_PARENT: &str = "/tmp";

/// Refuses a host whose worlds sit elsewhere than the corpora's. Both clients
/// right-align their footer against the working directory, so a parent of
/// another length moves the rows a corpus records, and the replay would read
/// that host input as the port regressing.
fn require_world_parent() {
    let world = World::new(Setup::mistral()).expect("a world starts");
    let parent = world
        .root()
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
        .unwrap_or_default();
    assert!(world.finish().is_empty(), "the probe world is clean");
    assert_eq!(
        parent, WORLD_PARENT,
        "the TUI corpora were captured with worlds under {WORLD_PARENT}, and this host creates \
         them under {parent}: the footer's padding follows the working directory's length, so \
         the screens would move for a reason outside the port. Run with TMPDIR unset, as CI \
         does, or recapture every family on this host"
    );
}

/// Plays one script in a fresh world and returns what its snapshots saw.
fn observe(scenario: &Scenario, subject: &Subject) -> Result<Value, Stalled> {
    let stalled = |message: String| Stalled {
        message,
        snapshot: 0,
        screen: Value::Null,
    };
    // A generic provider sends the port to onboarding (a known defect of the
    // e2e register), so both subjects run on a Mistral backend.
    let world = World::new(Setup::mistral()).map_err(|failure| stalled(failure.0))?;
    (scenario.prepare)(&world).map_err(|failure| stalled(failure.0))?;
    let observed = screens::play(
        &world,
        subject.command(&world),
        &scenario.script,
        &Masks::default(),
    );
    let verdict = world.finish();
    let observed = observed?;
    if verdict.is_empty() {
        Ok(observed)
    } else {
        Err(stalled(format!(
            "{}: the world reports {}",
            scenario.script.name,
            verdict.join("; ")
        )))
    }
}

/// The script as the corpus records it, with the masks both subjects share.
fn specification(scenario: &Scenario) -> Value {
    let mut described = scenario.script.describe();
    described["masks"] = Masks::default().describe();
    described
}

fn corpus_path(family: &Family) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(family.corpus)
}

/// The corpus as the manifest lists it.
fn listed(family: &Family) -> String {
    format!("crates/vibe-cli/{}", family.corpus)
}

/// Captures every family against the attested reference environment of the
/// commit `repin.py capture` names, or of the pin, and writes the corpora in
/// place.
#[test]
#[ignore = "the capture driver: `python3 scripts/parity/repin.py capture tui-blackbox/startup.json`"]
fn capture() {
    let commit = std::env::var("VIBE_PARITY_REPIN_COMMIT")
        .ok()
        .filter(|commit| !commit.is_empty())
        .unwrap_or_else(|| vibe_core::parity::REFERENCE_COMMIT.to_owned());
    let root = vibe_core::parity::live::environment_at(&commit)
        .unwrap_or_else(|refusal| panic!("{}", refusal.message("the TUI capture")));
    let attestation: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join(vibe_core::parity::live::ATTESTATION)).unwrap(),
    )
    .unwrap();
    require_world_parent();
    let subject = Subject::Reference(root);
    for family in FAMILIES {
        let mut recorded = Vec::new();
        let mut unavailable = Vec::new();
        for scenario in (family.scenarios)() {
            if let Some(reason) = scenario.port_only {
                unavailable.push(json!({"name": scenario.script.name, "reason": reason}));
                continue;
            }
            let observed = observe(&scenario, &subject).unwrap_or_else(|stalled| {
                panic!(
                    "{}\nlast reference screen:\n{}",
                    stalled.message,
                    screens::render(&stalled.screen)
                )
            });
            recorded.push(json!({
                "name": scenario.script.name,
                "heading": scenario.heading,
                "scenario": specification(&scenario),
                "observed": observed,
            }));
        }
        let corpus = json!({
            "schemaVersion": 1,
            "reference": {"commit": commit, "version": attestation["version"]},
            "platform": std::env::consts::OS,
            "note": "Normalized screens of the reference's terminal client, one script per \
                     scenario (crates/vibe-cli/tests/tui_blackbox_parity_tests.rs). A row \
                     keeps its text only when every letter on it was typed, seeded or answered \
                     by the scenario; any other row is its length and SHA-256. Masked cells \
                     and the world's directories are named in each scenario's masks.",
            "unavailable": unavailable,
            "scenarios": recorded,
        });
        let text = format!("{}\n", serde_json::to_string_pretty(&corpus).unwrap());
        std::fs::create_dir_all(corpus_path(family).parent().unwrap()).unwrap();
        std::fs::write(corpus_path(family), text).unwrap();
    }
}

/// The replay of one family: every recorded scenario against the port, the
/// port-only ones played for completion, the reference recaptured where the
/// pinned environment is here.
fn replay(family: &Family) {
    require_world_parent();
    let text = std::fs::read_to_string(corpus_path(family)).expect("the corpus is readable");
    let corpus: Value = serde_json::from_str(&text).expect("the corpus parses");
    let recorded = replay::scenarios(&corpus);
    let scenarios = (family.scenarios)();
    let recordable: Vec<&Scenario> = scenarios.iter().filter(|s| s.port_only.is_none()).collect();
    assert_eq!(
        recorded.keys().map(String::as_str).collect::<Vec<_>>(),
        {
            let mut names: Vec<&str> = recordable.iter().map(|s| s.script.name).collect();
            names.sort_unstable();
            names
        },
        "the {} family declares other scripts than its corpus records; recapture it with \
         `python3 scripts/parity/repin.py capture {}`",
        family.name,
        listed(family)
    );
    let unavailable: Vec<(&str, &str)> = corpus["unavailable"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| Some((entry["name"].as_str()?, entry["reason"].as_str()?)))
        .collect();
    let port_only: Vec<(&str, &str)> = scenarios
        .iter()
        .filter_map(|s| Some((s.script.name, s.port_only?)))
        .collect();
    assert_eq!(
        unavailable, port_only,
        "the corpus names other port-only scripts"
    );

    let mut headings: Vec<&str> = recordable.iter().map(|s| s.heading).collect();
    headings.dedup();
    let ledger: Vec<&dyn Entry> = Vec::new();
    for heading in headings {
        let mut tally = Tally::new(&text, ledger.iter().copied());
        for scenario in recordable.iter().filter(|s| s.heading == heading) {
            let name = scenario.script.name;
            let reference = recorded[name];
            assert_eq!(
                reference["scenario"],
                specification(scenario),
                "the script {name} changed since the corpus was recorded; recapture it with \
                 `python3 scripts/parity/repin.py capture {}`",
                listed(family)
            );
            let port = observe(scenario, &Subject::Port).unwrap_or_else(|stalled| {
                panic!("{}", stall_report(&stalled, &reference["observed"]))
            });
            let found = replay::differences(&reference["observed"], &port);
            tally.record(name, &reference["observed"], &port, found);
        }
        tally.conclude(heading);
    }
    for scenario in scenarios.iter().filter(|s| s.port_only.is_some()) {
        if let Err(stalled) = observe(scenario, &Subject::Port) {
            panic!(
                "the port-only script {} did not complete: {}\n{}",
                scenario.script.name,
                stalled.message,
                screens::render(&stalled.screen)
            );
        }
    }
    if let Some(root) = replay::pinned_reference(&format!("tui {} capture", family.name)) {
        let subject = Subject::Reference(root);
        for scenario in &recordable {
            let name = scenario.script.name;
            let answered = observe(scenario, &subject).unwrap_or_else(|stalled| {
                panic!("{}", stall_report(&stalled, &recorded[name]["observed"]))
            });
            let moved: Vec<String> = replay::differences(&recorded[name]["observed"], &answered)
                .into_iter()
                .map(|pointer| {
                    format!(
                        "{pointer}: recorded {}, now {}",
                        recorded[name]["observed"]
                            .pointer(&pointer)
                            .unwrap_or(&Value::Null),
                        answered.pointer(&pointer).unwrap_or(&Value::Null)
                    )
                })
                .collect();
            assert!(
                moved.is_empty(),
                "the pinned reference no longer renders {name} as the corpus records; recapture \
                 it with `python3 scripts/parity/repin.py capture {}`:\n  {}",
                listed(family),
                moved.join("\n  ")
            );
        }
    }
}

/// A stalled script: the subject's last normalized screen, the screen the
/// corpus records at that point, and the first cell where they differ.
fn stall_report(stalled: &Stalled, recorded: &Value) -> String {
    let expected = &recorded["snapshots"][stalled.snapshot]["screen"];
    let first = screens::first_difference(expected, &stalled.screen)
        .unwrap_or_else(|| "the two screens are the same".to_owned());
    format!(
        "{}\nthis subject's last screen:\n{}\nthe corpus records at snapshot {}:\n{}\nfirst \
         difference: {first}",
        stalled.message,
        screens::render(&stalled.screen),
        stalled.snapshot,
        screens::render(expected),
    )
}

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: the TUI corpora hold Linux screens, and a platform-specific corpus is declared per platform (US-019)"
)]
fn the_startup_family_replays_against_the_port() {
    replay(&families::STARTUP);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: the TUI corpora hold Linux screens, and a platform-specific corpus is declared per platform (US-019)"
)]
fn the_composer_family_replays_against_the_port() {
    replay(&families::COMPOSER);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: the TUI corpora hold Linux screens, and a platform-specific corpus is declared per platform (US-019)"
)]
fn the_commands_family_replays_against_the_port() {
    replay(&families::COMMANDS);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: the TUI corpora hold Linux screens, and a platform-specific corpus is declared per platform (US-019)"
)]
fn the_paths_family_replays_against_the_port() {
    replay(&families::PATHS);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "windows by design: the TUI corpora hold Linux screens, and a platform-specific corpus is declared per platform (US-019)"
)]
fn the_transcript_family_replays_against_the_port() {
    replay(&families::TRANSCRIPT);
}

/// US-019's determinism measurement: the startup and composer scripts five
/// times against the reference, masked and unmasked, listing what varies.
#[test]
#[ignore = "US-019 measurement; needs the pinned environment"]
fn the_reference_renders_each_script_identically_five_times() {
    let root = vibe_core::parity::live::pinned_environment()
        .unwrap_or_else(|refusal| panic!("{}", refusal.message("the determinism probe")));
    let subject = Subject::Reference(root);
    let mut report = Vec::new();
    for family in [
        &families::STARTUP,
        &families::COMPOSER,
        &families::COMMANDS,
        &families::PATHS,
    ] {
        for scenario in (family.scenarios)()
            .iter()
            .filter(|s| s.port_only.is_none())
        {
            let started = std::time::Instant::now();
            let runs: Vec<Value> = (0..5)
                .map(|_| observe(scenario, &subject).unwrap_or_else(|s| panic!("{}", s.message)))
                .collect();
            let varying: Vec<String> = runs[1..]
                .iter()
                .flat_map(|run| {
                    replay::differences(&runs[0], run)
                        .into_iter()
                        .map(|pointer| {
                            format!(
                                "{pointer} {} / {}",
                                runs[0].pointer(&pointer).unwrap_or(&Value::Null),
                                run.pointer(&pointer).unwrap_or(&Value::Null)
                            )
                        })
                })
                .collect();
            report.push(format!(
                "{}: {} varying pointers over 5 runs ({:.1} s): {:?}",
                scenario.script.name,
                varying.len(),
                started.elapsed().as_secs_f64(),
                varying.iter().take(8).collect::<Vec<_>>()
            ));
        }
    }
    println!("{}", report.join("\n"));
    assert!(
        report.iter().all(|line| line.contains(": 0 varying")),
        "{}",
        report.join("\n")
    );
}

/// US-019: the reference's composer is ready within ten seconds on a
/// 120 by 40 terminal.
#[test]
#[ignore = "US-019 measurement; needs the pinned environment"]
fn the_reference_shows_its_first_screen_within_ten_seconds() {
    let root = vibe_core::parity::live::pinned_environment()
        .unwrap_or_else(|refusal| panic!("{}", refusal.message("the first-screen probe")));
    let subject = Subject::Reference(root);
    let mut elapsed = Vec::new();
    for _ in 0..3 {
        let world = World::new(Setup::default()).unwrap();
        let started = std::time::Instant::now();
        let mut tui = subject.command(&world).spawn_tui(120, 40).unwrap();
        tui.wait_text("e2e-model", std::time::Duration::from_secs(10))
            .unwrap();
        tui.wait_text(families::READY, std::time::Duration::from_secs(10))
            .unwrap();
        elapsed.push(started.elapsed().as_secs_f64());
        tui.kill();
        assert_eq!(world.finish(), Vec::<String>::new());
    }
    println!("composer ready after {elapsed:.2?} s");
    assert!(elapsed.iter().all(|seconds| *seconds < 10.0), "{elapsed:?}");
}

/// The client binary the reference wheel bundles beside its Python package.
fn bundled_client(root: &Path) -> PathBuf {
    let name = if cfg!(windows) {
        "vibe-rs.exe"
    } else {
        "vibe-rs"
    };
    let environment = root.join(".venv");
    let windows = environment.join("Lib/site-packages/vibe/_bin").join(name);
    if windows.is_file() {
        return windows;
    }
    std::fs::read_dir(environment.join("lib"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path().join("site-packages/vibe/_bin").join(name))
        .find(|path| path.is_file())
        .expect("the environment bundles the client")
}

/// US-019's fallback oracle: the client started with `VIBE_APP_SERVER_CMD`
/// naming the relay, through one turn, records the app-server methods it
/// issues.
#[test]
#[ignore = "US-019 measurement; needs the pinned environment"]
fn the_app_server_method_sequence_is_recorded_through_the_relay() {
    let root = vibe_core::parity::live::pinned_environment()
        .unwrap_or_else(|refusal| panic!("{}", refusal.message("the relay probe")));
    let world = World::new(Setup::default()).unwrap();
    world
        .platform()
        .utility(vibe_e2e::Reply::text("Relayed session"));
    world
        .platform()
        .reply(vibe_e2e::Reply::text("Relayed answer."));
    let log = world.tmp().join("methods.jsonl");
    let server = launcher(&root).with_file_name(if cfg!(windows) {
        "vibe-app-server.exe"
    } else {
        "vibe-app-server"
    });
    let python = vibe_core::parity::pinned_interpreter(&root).expect("the environment's python");
    let quote = |path: &Path| format!("'{}'", path.display());
    let relay =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/parity/app_server_tap.py");
    let command = [
        quote(&python),
        quote(&relay),
        "--log".to_owned(),
        quote(&log),
        "--".to_owned(),
        quote(&server),
    ]
    .join(" ");
    let mut tui = world
        .program(bundled_client(&root))
        .current_dir(world.work())
        .env("VIBE_APP_SERVER_CMD", command)
        .spawn_tui(120, 40)
        .unwrap();
    let step = std::time::Duration::from_secs(20);
    tui.wait_text(families::READY, step).unwrap();
    tui.type_text("relay a turn").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1200));
    tui.send(&[vibe_e2e::Key::Enter]).unwrap();
    if let Err(failure) = tui.wait_text("Relayed answer.", step) {
        let logs: String = walk(&world.vibe_home())
            .into_iter()
            .filter(|path| path.extension().is_some_and(|e| e == "log"))
            .map(|path| std::fs::read_to_string(path).unwrap_or_default())
            .collect();
        panic!(
            "{}\nrelay log:\n{}\nclient logs:\n{}",
            failure.0.lines().take(8).collect::<Vec<_>>().join("\n"),
            std::fs::read_to_string(&log).unwrap_or_default(),
            logs.lines().rev().take(40).collect::<Vec<_>>().join("\n")
        );
    }
    tui.kill();
    let methods: Vec<Value> = std::fs::read_to_string(&log)
        .expect("the relay wrote its log")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let requests: Vec<&str> = methods
        .iter()
        .filter(|entry| entry["from"] == "client" && entry["kind"] == "request")
        .filter_map(|entry| entry["method"].as_str())
        .collect();
    let notifications: std::collections::BTreeSet<&str> = methods
        .iter()
        .filter(|entry| entry["from"] == "server" && entry["kind"] == "notification")
        .filter_map(|entry| entry["method"].as_str())
        .collect();
    println!("client requests: {requests:?}\nserver notifications: {notifications:?}");
    assert_eq!(requests.first(), Some(&"initialize"), "{requests:?}");
    assert!(requests.len() > 2, "{requests:?}");
    drop(tui);
    assert_eq!(world.finish(), Vec::<String>::new());
}

/// Every file below `root`.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}
