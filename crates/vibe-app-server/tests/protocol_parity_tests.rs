//! Replays the committed protocol corpus against this port's app server.
//!
//! `scripts/parity/app_server_protocol.py` captured the corpus from the pinned
//! reference's own `vibe-app-server`: every scenario serves it over stdio in a
//! fresh home, behind a scripted stand-in for the provider and console APIs,
//! and records the shape of every answer, notification and server request the
//! app-server protocol publishes, from a probe per declared method to the
//! flows that chain them (turns, callbacks, the queue, manual shells,
//! compaction, the skills registry, the connector catalog, git checkouts).
//! Driving the `vibe-app-server-stdio-fixture` binary through the same script
//! with `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! The script records shapes rather than values, and reduces the prose a
//! server authored to a length and a SHA-256. This port writes its own prose
//! on purpose (`NOTICE`), so two digests count as equal wherever both sides
//! hold one, and everything else is compared exactly.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 17 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use vibe_core::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("protocol-parity/corpus.json");

const CAPTURE_SCRIPT: &str = "scripts/parity/app_server_protocol.py";

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 300;

/// How many scenarios the capture script runs side by side. Each owns its
/// directories, backend and server processes.
const JOBS: &str = "4";

/// A pointer this port answers differently in some scenarios, and why.
struct Divergence {
    /// The scenario it applies to, `*` for every one, or `<prefix>/*` for every
    /// scenario one segment below the prefix, which names both probes of one
    /// method.
    scenario: &'static str,
    /// Every difference whose JSON pointer ends with this suffix is covered; an
    /// empty suffix covers every difference in the scenario.
    suffix: &'static str,
    reason: &'static str,
}

impl Divergence {
    fn covers(&self, scenario: &str, pointer: &str) -> bool {
        let scenario_matches = self.scenario == "*"
            || self.scenario == scenario
            || self.scenario.strip_suffix("/*").is_some_and(|prefix| {
                scenario
                    .rsplit_once('/')
                    .is_some_and(|(parent, _)| parent == prefix)
            });
        scenario_matches && pointer.ends_with(self.suffix)
    }
}

/// What the v2.26.0 re-pin opened (`376f6a3`): fields the reference's public
/// models gained, and the methods this port neither declares nor routes, which
/// the per-method probes reach.
const LEDGER: &[Divergence] = &[
    Divergence {
        scenario: "*",
        suffix: "/maxContextLength",
        reason: "since v2.26.0 every model a configuration view lists states its context window \
                 (`vibe/app_server/config.py:25`, projected by `vibe/app_server/_projection.py:162` \
                 at `376f6a3`); this port's model view does not carry the field",
    },
    Divergence {
        scenario: "*",
        suffix: "/thinkingLevels",
        reason: "since v2.26.0 every model a configuration view lists names the thinking levels \
                 it offers (`vibe/app_server/config.py:28`, projected by \
                 `vibe/app_server/_projection.py:164` at `376f6a3`); this port's model view does \
                 not carry the field",
    },
    Divergence {
        scenario: "*",
        suffix: "/imageDelivery",
        reason: "since v2.26.0 every model a configuration view lists states how file-backed \
                 images reach it (`vibe/app_server/config.py:33`, projected by \
                 `vibe/app_server/_projection.py:151-163` at `376f6a3`); this port's model view \
                 does not carry the field",
    },
    Divergence {
        scenario: "*",
        suffix: "/config/enableSystemTrustStore",
        reason: "since v2.26.0 the configuration view publishes the outbound TLS trust setting \
                 (`vibe/app_server/config.py:99`, projected by \
                 `vibe/app_server/_projection.py:111` at `376f6a3`); this port's view does not \
                 carry the field",
    },
    Divergence {
        scenario: "*",
        suffix: "/inputEntryId",
        reason: "since v2.26.0 a public turn and every public history entry name the user entry \
                 the turn answers (`vibe/app_server/models.py:1236` and `949` at `376f6a3`); this \
                 port's turns and entries do not carry the field",
    },
    Divergence {
        scenario: "*",
        suffix: "/session/archivedAt",
        reason: "since v2.26.0 a public session records when it was archived \
                 (`vibe/app_server/models.py:1185` at `376f6a3`); this port's session does not \
                 carry the field",
    },
    Divergence {
        scenario: "*",
        suffix: "/session/isUnseen",
        reason: "since v2.26.0 a public session says whether it holds activity its user has not \
                 seen (`vibe/app_server/models.py:1186` at `376f6a3`); this port's session does \
                 not carry the field",
    },
    Divergence {
        scenario: "*",
        suffix: "/session/worktree",
        reason: "since v2.26.0 a public session names the managed worktree it runs in \
                 (`vibe/app_server/models.py:1191`, shaped by `PublicSessionWorktree` at \
                 `vibe/app_server/models.py:1157` at `376f6a3`); this port's session does not \
                 carry the field",
    },
    Divergence {
        scenario: "*",
        suffix: "/state/backgroundProcesses",
        reason: "since v2.26.0 the public session state lists the session's background processes \
                 (`vibe/app_server/models.py:1263` at `376f6a3`); this port's state does not carry \
                 the list",
    },
    Divergence {
        scenario: "*",
        suffix: "/detail/pathScopeChoices",
        reason: "since v2.26.0 an approval callback lists the path grant scopes it offers \
                 (`vibe/app_server/models.py:322`, filled by `vibe/app_server/_turns.py:863` at \
                 `376f6a3`); this port's callback does not carry the field",
    },
    Divergence {
        scenario: "*",
        suffix: "/decision/pathScope",
        reason: "since v2.26.0 the approval decision an effect records holds the path grant scope \
                 it chose (`vibe/app_server/models.py:312` at `376f6a3`), null for a plain \
                 approval; this port's decision does not carry the field",
    },
    Divergence {
        scenario: "*",
        suffix: "/output/fileExisted",
        reason: "since v2.26.0 a file write's output says whether it replaced an existing file \
                 (`vibe/app_server/_effect_models.py:158` at `376f6a3`); this port's output does \
                 not carry the field",
    },
    Divergence {
        scenario: "*",
        suffix: "/output/previousContent",
        reason: "since v2.26.0 a file write's output carries the text it replaced \
                 (`vibe/app_server/_effect_models.py:159` at `376f6a3`); this port's output does \
                 not carry the field",
    },
    Divergence {
        scenario: "probe/providerAuth/read/*",
        suffix: "/error/code",
        reason: "v2.26.0 declares `providerAuth/read` (`vibe/app_server/protocol.py:158` at \
                 `376f6a3`): with no session the reference asks for one first \
                 (`vibe/app_server/server.py:1333`), and the legacy backend refuses it as not \
                 implemented (`vibe/app_server/_handler.py:317`); this port does not declare it \
                 and answers that the method is unknown",
    },
    Divergence {
        scenario: "probe/session/backgroundProcess/output/bare",
        suffix: "/error/code",
        reason: "v2.26.0 declares `session/backgroundProcess/output` \
                 (`vibe/app_server/protocol.py:176` at `376f6a3`), so with no session the \
                 reference asks for one first (`vibe/app_server/server.py:1333`); this port does \
                 not declare it and answers that the method is unknown",
    },
    Divergence {
        scenario: "probe/session/backgroundProcess/stop/bare",
        suffix: "/error/code",
        reason: "v2.26.0 declares `session/backgroundProcess/stop` \
                 (`vibe/app_server/protocol.py:177` at `376f6a3`), so with no session the \
                 reference asks for one first (`vibe/app_server/server.py:1333`); this port does \
                 not declare it and answers that the method is unknown",
    },
    Divergence {
        scenario: "probe/setup/status/*",
        suffix: "",
        reason: "v2.26.0 answers `setup/status` without a session, with the onboarding seed, \
                 and refuses a `sessionId` (`vibe/app_server/protocol.py:1807`, \
                 `vibe/app_server/_setup.py:63` at `376f6a3`); this port does not declare it and \
                 answers that the method is unknown",
    },
    Divergence {
        scenario: "probe/setup/store-credential/*",
        suffix: "",
        reason: "v2.26.0 validates `setup/store-credential` against its parameters \
                 (`vibe/app_server/protocol.py:1831`, `vibe/app_server/_setup.py:66` at \
                 `376f6a3`); this port does not declare it and answers that the method is \
                 unknown",
    },
    Divergence {
        scenario: "probe/setup/submit-choices/*",
        suffix: "",
        reason: "v2.26.0 answers `setup/submit-choices` without a session and refuses a \
                 `sessionId` (`vibe/app_server/protocol.py:1853`, \
                 `vibe/app_server/_setup.py:69` at `376f6a3`); this port does not declare it and \
                 answers that the method is unknown",
    },
    Divergence {
        scenario: "probe/workspace/git/worktrees/reap/*",
        suffix: "",
        reason: "v2.26.0 validates `workspace/git/worktrees/reap` against its parameters \
                 (`vibe/app_server/protocol.py:1712`, `vibe/app_server/_host.py:516` at \
                 `376f6a3`); this port does not declare it and answers that the method is \
                 unknown",
    },
    Divergence {
        scenario: "probe/workspace/git/worktrees/reap/cancel/*",
        suffix: "",
        reason: "v2.26.0 validates `workspace/git/worktrees/reap/cancel` against its parameters \
                 (`vibe/app_server/protocol.py:1731`, `vibe/app_server/_host.py:524` at \
                 `376f6a3`); this port does not declare it and answers that the method is \
                 unknown",
    },
];

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root resolves")
}

fn scenarios(corpus: &Value) -> BTreeMap<String, &Value> {
    corpus["scenarios"]
        .as_array()
        .expect("the corpus holds a scenario list")
        .iter()
        .map(|entry| {
            (
                entry["name"]
                    .as_str()
                    .expect("every scenario is named")
                    .to_owned(),
                entry,
            )
        })
        .collect()
}

fn is_prose(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == 2 && object.contains_key("prose") && object.contains_key("sha256")
    })
}

/// Every JSON pointer at which `port` departs from `reference`.
fn differences(reference: &Value, port: &Value, pointer: &str, found: &mut Vec<String>) {
    if is_prose(reference) && is_prose(port) {
        return;
    }
    match (reference, port) {
        (Value::Object(left), Value::Object(right)) => {
            let keys: BTreeSet<&String> = left.keys().chain(right.keys()).collect();
            for key in keys {
                let child = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => differences(left, right, &child, found),
                    _ => found.push(child),
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            for index in 0..left.len().max(right.len()) {
                let child = format!("{pointer}/{index}");
                match (left.get(index), right.get(index)) {
                    (Some(left), Some(right)) => differences(left, right, &child, found),
                    _ => found.push(child),
                }
            }
        }
        _ if reference != port => found.push(pointer.to_owned()),
        _ => {}
    }
}

fn capture(arguments: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new("python3")
        .arg(repository().join(CAPTURE_SCRIPT))
        .args(arguments)
        .args(["--jobs", JOBS])
        .current_dir(repository())
        .env_remove("FORCE_COLOR")
        .output()
        .expect("python3 runs the protocol capture script")
}

#[test]
fn the_port_answers_every_protocol_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    assert!(
        recorded.len() >= SCENARIO_FLOOR,
        "the corpus holds {} scenarios, below the floor of {SCENARIO_FLOOR}",
        recorded.len()
    );

    let output_path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("protocol-parity-port.json");
    let output = capture(&[
        "--server".as_ref(),
        env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture").as_ref(),
        "--output".as_ref(),
        output_path.as_os_str(),
    ]);
    assert!(
        output.status.success(),
        "the port capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replayed: Value = serde_json::from_str(
        &std::fs::read_to_string(&output_path).expect("the port capture is readable"),
    )
    .expect("the port capture parses");
    let replayed = scenarios(&replayed);
    assert_eq!(
        recorded.keys().collect::<Vec<_>>(),
        replayed.keys().collect::<Vec<_>>(),
        "the capture script declares other scenarios than the corpus records; recapture the \
         reference with `{CAPTURE_SCRIPT}`"
    );

    let mut unexplained = Vec::new();
    let mut reproduced = vec![0_usize; LEDGER.len()];
    let mut conformant = 0;
    for (name, reference) in &recorded {
        let port = replayed[name];
        assert_eq!(
            reference["scenario"], port["scenario"],
            "the capture script changed scenario {name} since the corpus was recorded; \
             recapture the reference with `{CAPTURE_SCRIPT}`"
        );
        let mut found = Vec::new();
        differences(&reference["observed"], &port["observed"], "", &mut found);
        if found.is_empty() {
            conformant += 1;
        }
        for pointer in found {
            match LEDGER.iter().position(|entry| entry.covers(name, &pointer)) {
                Some(index) => reproduced[index] += 1,
                None => unexplained.push(format!("{name} {pointer}")),
            }
        }
    }
    let stale: Vec<&str> = LEDGER
        .iter()
        .zip(&reproduced)
        .filter(|(_, count)| **count == 0)
        .map(|(entry, _)| entry.reason)
        .collect();
    println!(
        "protocol parity: {conformant}/{} scenarios conformant, {} ledgered differences across \
         {} entries",
        recorded.len(),
        reproduced.iter().sum::<usize>(),
        LEDGER.len()
    );
    assert!(
        unexplained.is_empty(),
        "the port departs from the corpus where no ledger entry says it may:\n{}",
        unexplained.join("\n")
    );
    assert!(
        stale.is_empty(),
        "these ledger entries no longer reproduce and should be removed: {stale:?}"
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "protocol") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let output = capture(&["--check".as_ref(), "--reference".as_ref(), root.as_os_str()]);
    assert!(
        output.status.success(),
        "the pinned reference no longer answers what the corpus records; regenerate it with \
         `{CAPTURE_SCRIPT}`: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
