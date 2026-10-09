//! Replays the committed agent loop corpus against this port's app server.
//!
//! `scripts/parity/agent_loop.py` captured the corpus from the pinned
//! reference's own `vibe-app-server`: every scenario serves it over stdio in a
//! fresh home, behind a scripted stand-in for the chat-completions API, and
//! drives the turns row 34 of `docs/parity.md` is about. `persist` records the
//! lines a turn appends to `messages.jsonl`, key order included, the
//! statistics `meta.json` keeps and what a reload shows; `stats` the cached
//! tokens and the session cost; `interrupt` a turn cut short while a tool
//! runs, before it runs and while it waits on its approval; `approval` the
//! session and permanent grants; `steer` and `inject` the context a running
//! or idle session takes; `plan` a plan reviewed and edited by hand; `titles`
//! the background title, its cadence and the snapshots that announce it;
//! `relocate` a session moved to another worktree; `children` a delegated
//! subagent's saved session. Driving the `vibe-app-server-stdio-fixture`
//! binary through the same script with `--server` yields observations
//! normalized the same way, and the replay compares the two scenario by
//! scenario. It needs no reference checkout; only the live probe at the end,
//! which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256, and two digests are compared exactly. What differs is the prose
//! `NOTICE` keeps this port from reproducing: tool errors and displays, the
//! plan review, the sentences the loop answers an interrupted call and a
//! reviewed plan with, the plan-mode reminder and the checkpoints a reload
//! closes on.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, or a
//! `FIELDS` entry for a field that differs wherever it appears, and every
//! entry has to still reproduce, so row 34 of `docs/parity.md` is a reading
//! of the summary this file prints.

#![cfg(feature = "test-fixtures")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use vibe_core::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("agent-loop-parity/corpus.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE_SCRIPT: &str = "scripts/parity/agent_loop.py";

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 29;

/// How many scenarios the capture script runs side by side. Each owns its
/// directories, backend and server processes.
const JOBS: &str = "4";

/// The accepted divergence the row 34 entries below stand on: a difference
/// the measured row keeps on purpose has to be decided in the scorecard.
const OWN_PROSE: &str = "The conversation loop's own sentences";

/// A difference this port keeps in the scenarios named, and the row of
/// `docs/parity.md` that answers for it.
struct Divergence {
    scenarios: &'static [&'static str],
    /// Every difference whose JSON pointer starts with this prefix is covered.
    pointer: &'static str,
    row: &'static str,
    reason: &'static str,
}

const PLAN_REVIEWS: &[&str] = &["plan/edited-during-review", "plan/unchanged-during-review"];

const MODEL_PROBE: &str = "since v2.26.0 a root session opened with titles on first asks the \
     provider whether it serves a fast model, with a one-token completion that offers no tools \
     (`vibe/app_server/_runtime.py:1956`, `vibe/core/llm/model_probe.py:189-211` at 376f6a3), \
     so the reference sends one more utility request than this port, which sends no probe. \
     The stand-in answers the probe apart, so every title still reads the reply its scenario \
     scripted";

const LEDGER: &[Divergence] = &[
    // ---------------------------------------------------------- row 3
    Divergence {
        scenarios: &[
            "persist/tool-failure",
            "persist/reload",
            "persist/invalid-arguments",
            "approval/permanent",
        ],
        pointer: "/requests/1/conversation/2/content",
        row: "3",
        reason: "a failed call answers the model under the reference's error tag and tool name, \
                 with the tool's own sentence: `read_file`'s missing file, `web_search`'s HTTP \
                 failure, and an argument rejection rendered by this port's schema validator \
                 where the reference prints pydantic's report",
    },
    Divergence {
        scenarios: &["persist/tool-failure", "persist/invalid-arguments"],
        pointer: "/steps/1/persisted/0/messages/2/message/content",
        row: "3",
        reason: "the same failure, as the saved tool message",
    },
    Divergence {
        scenarios: &["persist/tool-failure"],
        pointer: "/steps/0/turn/9/params/patch/0/value/",
        row: "3",
        reason: "the same failure, as the settled effect's header and error",
    },
    Divergence {
        scenarios: &["persist/invalid-arguments"],
        pointer: "/steps/0/turn/8/params/patch/0/value/",
        row: "3",
        reason: "the same rejection, as the settled effect's header and error",
    },
    Divergence {
        scenarios: &["persist/reload"],
        pointer: "/steps/0/turn/13/params/patch/0/value/",
        row: "3",
        reason: "the same failure, as the live effect's header and error",
    },
    Divergence {
        scenarios: &["persist/reload"],
        pointer: "/steps/1/response/result/state/history/3/state/",
        row: "3",
        reason: "the same failure, as the effect the resumed session rebuilds from its log",
    },
    Divergence {
        scenarios: &["persist/reload"],
        pointer: "/steps/2/response/result/history/3/state/",
        row: "3",
        reason: "the same failure, as `session/history/get` reads it back",
    },
    Divergence {
        scenarios: &["approval/permanent"],
        pointer: "/steps/0/turn/15/params/patch/0/value/",
        row: "3",
        reason: "the same `web_search` failure, as the settled effect's header and error",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/requests/1/conversation/3/content",
        row: "3",
        reason: "`exit_plan_mode` writes its own sentence for each plan-review outcome, as the \
                 model reads the result",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/0/turn/8/params/entry/detail/display/",
        row: "3",
        reason: "`exit_plan_mode`'s status line is this port's own words",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/0/turn/9/params/patch/0/value/display/",
        row: "3",
        reason: "`exit_plan_mode`'s call header, the same verbs over this port's own summary, \
                 message and status line",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/0/turn/12/params/entry/detail/request/",
        row: "3",
        reason: "the plan review asks its question, describes its four options and names the \
                 plan file in this port's own words; the option labels, which a client answers \
                 with, are the reference's",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/0/turn/15/params/patch/0/value/output/result/answers/0/question",
        row: "3",
        reason: "the same question, as the answered callback repeats it",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/0/turn/18/params/patch/0/value/",
        row: "3",
        reason: "the plan-review outcome, as the settled effect's header and output",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/persisted/0/messages/2/message/tool_calls/0/presentation/display/",
        row: "3",
        reason: "`exit_plan_mode`'s call display, as the saved call records it",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/persisted/0/messages/3/message/",
        row: "3",
        reason: "the plan-review outcome, as the saved tool message, its result and its display",
    },
    // --------------------------------------------------------- row 10
    Divergence {
        scenarios: &["titles/first-answer"],
        pointer: "/requests/2",
        row: "10",
        reason: MODEL_PROBE,
    },
    Divergence {
        scenarios: &["titles/manual-title-wins"],
        pointer: "/requests/1",
        row: "10",
        reason: MODEL_PROBE,
    },
    Divergence {
        scenarios: &["titles/after-compaction"],
        pointer: "/requests/5",
        row: "10",
        reason: MODEL_PROBE,
    },
    Divergence {
        scenarios: &["titles/tool-heavy-turn"],
        pointer: "/requests/11",
        row: "10",
        reason: MODEL_PROBE,
    },
    // --------------------------------------------------------- row 13
    Divergence {
        scenarios: &["persist/denied"],
        pointer: "/steps/0/turn/15/params/patch/0/value/display/message",
        row: "13",
        reason: "a declined call's display is this port's own sentence",
    },
    Divergence {
        scenarios: &["persist/denied"],
        pointer: "/steps/1/persisted/0/messages/2/message/content",
        row: "13",
        reason: "a declined call answers the model under the reference's cancellation tag with \
                 this port's own sentence",
    },
    Divergence {
        scenarios: &["persist/reload"],
        pointer: "/steps/1/response/result/state/history/5/message",
        row: "13",
        reason: "the checkpoint a resumed session closes its history on is this port's own \
                 sentence",
    },
    Divergence {
        scenarios: &["persist/resource-and-display"],
        pointer: "/steps/2/response/result/state/history/2/message",
        row: "13",
        reason: "the same resume checkpoint",
    },
    // --------------------------------------------------------- row 17
    Divergence {
        scenarios: &["relocate/during-turn"],
        pointer: "/steps/0/response/error/message",
        row: "17",
        reason: "the refusal names the running turn, which this port numbers where the \
                 reference mints a UUID",
    },
    // --------------------------------------------------------- row 19
    Divergence {
        scenarios: &["titles/after-compaction"],
        pointer: "/requests/1/conversation/2/content",
        row: "19",
        reason: "the summarization request carries this port's own compaction instructions",
    },
    // --------------------------------------------------------- row 26
    Divergence {
        scenarios: &["titles/after-compaction"],
        pointer: "/steps/3/persisted/0/messages/2/message/content",
        row: "26",
        reason: "the compaction envelope's two prose runs are this port's own",
    },
    // --------------------------------------------------------- row 34
    Divergence {
        scenarios: &["interrupt/during-tool", "interrupt/before-run"],
        pointer: "/requests/1/conversation/2/content",
        row: "34",
        reason: "a call the interrupt cut short, and one left without an answer, reads the \
                 reference's cancellation tag around this port's own sentence",
    },
    Divergence {
        scenarios: &["interrupt/during-tool"],
        pointer: "/steps/1/persisted/0/messages/2/message/content",
        row: "34",
        reason: "the same interrupted answer, as the turn saves it",
    },
    Divergence {
        scenarios: &["interrupt/during-tool"],
        pointer: "/steps/3/persisted/0/messages/2/message/content",
        row: "34",
        reason: "the same interrupted answer, as the next turn leaves it",
    },
    Divergence {
        scenarios: &["interrupt/before-run"],
        pointer: "/steps/4/persisted/0/messages/2/message/content",
        row: "34",
        reason: "the answer the next turn fills a call left unanswered with",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/requests/0/conversation/1/content",
        row: "34",
        reason: "the reminder a plan session injects before its first request is this port's own \
                 prose, naming the same plan file, the same read-only rule and the same two \
                 tools as reference `ReadOnlyAgentMiddleware`",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/requests/1/conversation/1/content",
        row: "34",
        reason: "the same reminder, as the next request resends it",
    },
    Divergence {
        scenarios: PLAN_REVIEWS,
        pointer: "/steps/1/persisted/0/messages/1/message/content",
        row: "34",
        reason: "the same reminder, as the session saves it",
    },
    Divergence {
        scenarios: &["plan/edited-during-review"],
        pointer: "/requests/1/conversation/4/content",
        row: "34",
        reason: "a plan edited by hand during its review reaches the model in a warning of this \
                 port's own words, under the reference's tag and over the same plan text",
    },
    Divergence {
        scenarios: &["plan/edited-during-review"],
        pointer: "/steps/1/persisted/0/messages/4/message/content",
        row: "34",
        reason: "the same warning, as the session saves it",
    },
];

/// A field this port answers differently wherever it appears, in any scenario,
/// and the row of `docs/parity.md` that answers for it.
struct FieldDivergence {
    /// Every difference whose JSON pointer ends with this suffix is covered.
    suffix: &'static str,
    row: &'static str,
    reason: &'static str,
}

const SESSION_ARCHIVE: &str = "v2.26.0 publishes each session's `archivedAt` and `isUnseen` \
     (`vibe/app_server/models.py:1185-1186` at 376f6a3), the state `session/archive` and \
     `session/markAsSeen` keep; this port routes neither method and omits both fields, which \
     read null and false in every scenario";

const PATH_SCOPE: &str = "v2.26.0 approvals carry a path grant scope: `pathScopeChoices` on the \
     request and `pathScope` on the decision (`vibe/app_server/models.py:312,322` at 376f6a3), \
     empty or null for every call these scenarios approve; this port's payloads carry neither";

const FIELDS: &[FieldDivergence] = &[
    FieldDivergence {
        suffix: "/inputEntryId",
        row: "17",
        reason: "v2.26.0 gives every public history entry and turn an `inputEntryId` \
                 (`vibe/app_server/models.py:949,1236` at 376f6a3), null on the legacy \
                 backend; this port's entries and turns do not carry the field",
    },
    FieldDivergence {
        suffix: "/archivedAt",
        row: "17",
        reason: SESSION_ARCHIVE,
    },
    FieldDivergence {
        suffix: "/isUnseen",
        row: "17",
        reason: SESSION_ARCHIVE,
    },
    FieldDivergence {
        suffix: "/worktree",
        row: "17",
        reason: "v2.26.0 publishes the managed worktree a session runs in \
                 (`vibe/app_server/models.py:1191` at 376f6a3), null outside one; this port \
                 omits the field",
    },
    FieldDivergence {
        suffix: "/backgroundProcesses",
        row: "17",
        reason: "v2.26.0 session state lists the Unified Harness's background processes \
                 (`vibe/app_server/models.py:1263` at 376f6a3), empty on the legacy backend; \
                 this port omits the field",
    },
    FieldDivergence {
        suffix: "/agentName",
        row: "17",
        reason: "v2.26.0 names the agent a subagent call runs on its detail \
                 (`vibe/app_server/_effect_models.py:295` at 376f6a3), null until the call is \
                 known; this port omits the field",
    },
    FieldDivergence {
        suffix: "/pathScopeChoices",
        row: "17",
        reason: PATH_SCOPE,
    },
    FieldDivergence {
        suffix: "/pathScope",
        row: "17",
        reason: PATH_SCOPE,
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

/// Every JSON pointer at which `port` departs from `reference`.
fn differences(reference: &Value, port: &Value, pointer: &str, found: &mut Vec<String>) {
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
        .expect("python3 runs the agent loop capture script")
}

#[test]
fn the_port_runs_every_loop_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    assert!(
        recorded.len() >= SCENARIO_FLOOR,
        "the corpus holds {} scenarios, below the floor of {SCENARIO_FLOOR}",
        recorded.len()
    );

    let output_path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("agent-loop-parity-port.json");
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
    // One count per entry and scenario it names, so an entry that stopped
    // reproducing in one of its scenarios is reported for that one.
    let mut reproduced = BTreeMap::<(usize, &str), usize>::new();
    let mut fields = vec![0_usize; FIELDS.len()];
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
            match LEDGER.iter().position(|entry| {
                entry.scenarios.contains(&name.as_str()) && pointer.starts_with(entry.pointer)
            }) {
                Some(index) => {
                    let scenario = LEDGER[index]
                        .scenarios
                        .iter()
                        .find(|scenario| **scenario == name.as_str())
                        .expect("the entry names the scenario it matched");
                    *reproduced.entry((index, scenario)).or_default() += 1;
                }
                None => match FIELDS
                    .iter()
                    .position(|field| pointer.ends_with(field.suffix))
                {
                    Some(index) => fields[index] += 1,
                    None => unexplained.push(format!("{name} {pointer}")),
                },
            }
        }
    }
    let mut stale = Vec::new();
    for (index, entry) in LEDGER.iter().enumerate() {
        for scenario in entry.scenarios {
            if !reproduced.contains_key(&(index, *scenario)) {
                stale.push(format!("{scenario} {}", entry.pointer));
            }
        }
    }
    for (field, count) in FIELDS.iter().zip(&fields) {
        if *count == 0 {
            stale.push(field.suffix.to_owned());
        }
    }
    println!(
        "agent loop parity: {conformant}/{} scenarios conformant, {} ledgered differences \
         across {} entries",
        recorded.len(),
        reproduced.values().chain(&fields).sum::<usize>(),
        LEDGER.len() + FIELDS.len()
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

#[test]
fn every_ledger_entry_names_a_scorecard_row_that_answers_for_it() {
    let rows: BTreeSet<&str> = SCORECARD
        .lines()
        .filter_map(|line| line.strip_prefix("| "))
        .filter_map(|line| line.split_once(" |"))
        .map(|(number, _)| number.trim())
        .filter(|number| number.parse::<u32>().is_ok())
        .collect();
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    for entry in LEDGER {
        assert!(
            rows.contains(entry.row),
            "the ledger entry for {} names row {}, which docs/parity.md does not carry",
            entry.pointer,
            entry.row
        );
        // Row 34 is what this corpus measures, so a difference it keeps is a
        // gap unless the scorecard decided to keep it.
        assert!(
            entry.row != "34" || SCORECARD.contains(&format!("| {OWN_PROSE} |")),
            "the ledger entry for {} keeps a row 34 difference that no accepted divergence \
             decides",
            entry.pointer
        );
        assert!(
            !entry.scenarios.is_empty()
                && entry
                    .scenarios
                    .iter()
                    .all(|scenario| recorded.contains_key(*scenario))
                && entry.pointer.starts_with('/')
                && !entry.reason.is_empty(),
            "the ledger entry for {} needs recorded scenarios, a pointer prefix and a reason",
            entry.pointer
        );
    }
    for field in FIELDS {
        assert!(
            rows.contains(field.row) && field.row != "34",
            "the field entry for {} names row {}, which is not another row docs/parity.md \
             carries",
            field.suffix,
            field.row
        );
        assert!(
            field.suffix.starts_with('/') && !field.reason.is_empty(),
            "the field entry for {} needs a pointer suffix and a reason",
            field.suffix
        );
    }
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "agent loop") {
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
