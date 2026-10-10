//! Replays the committed hooks corpus against this port's app server.
//!
//! `scripts/parity/hooks.py` captured the corpus from the pinned reference's
//! own `vibe-app-server`: every scenario serves it over stdio in a fresh home,
//! behind a scripted stand-in for the chat-completions API, writes the
//! `hooks.toml` files it declares, and drives turns whose tool calls the hooks
//! guard. Each hook runs a recorder that keeps what it read on stdin and
//! answers as the scenario scripted. The corpus holds the hook notices each
//! turn raised among the transcript entries, every invocation a hook read, the
//! conversation the model was sent, the tool calls the session persisted, the
//! hooks that outlived their wait, and the count and issues `runtime/read`,
//! `diagnostics/list` and `config/read` report as the files change. Driving
//! the `vibe-app-server-stdio-fixture` binary through the same script with
//! `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their server authored to a length and a
//! SHA-256. Unlike the session corpus, two digests are compared exactly: the
//! notices, denials and retry messages a hook run produces are the contract
//! this part measures, and this port writes them as the reference does.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, or a
//! `FIELDS` entry for a field that differs wherever it appears, and every
//! entry has to still reproduce, so row 18 of `docs/parity.md` is a reading
//! of the summary this file prints.

#![cfg(feature = "test-fixtures")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use vibe_core::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("hooks-parity/corpus.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE_SCRIPT: &str = "scripts/parity/hooks.py";

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 39;

/// How many scenarios the capture script runs side by side. Each owns its
/// directories, backend and server processes.
const JOBS: &str = "4";

/// A difference this port keeps in one scenario, and the row of
/// `docs/parity.md` that answers for it.
struct Divergence {
    scenario: &'static str,
    /// Every difference whose JSON pointer starts with this prefix is covered.
    pointer: &'static str,
    row: &'static str,
    reason: &'static str,
}

const LEDGER: &[Divergence] = &[
    Divergence {
        scenario: "pre_tool/rewrite-declaration-order",
        pointer: "/conversations/1/1/toolCalls/0/arguments",
        row: "4",
        reason: "a rewritten call's arguments carry the same fields and values, with keys sorted \
                 where the reference's `model_dump` lists them in declaration order: this \
                 workspace builds `serde_json` without `preserve_order`, so no tool schema here \
                 keeps its declaration order to dump by",
    },
    Divergence {
        scenario: "pre_tool/rewrite-declaration-order",
        pointer: "/steps/1/persisted/0/1/toolCalls/0/arguments",
        row: "4",
        reason: "the same key order, as persisted",
    },
    Divergence {
        scenario: "post_tool/after-a-rewrite",
        pointer: "/conversations/1/1/toolCalls/0/arguments",
        row: "4",
        reason: "the same key order, in the call a `post_tool` hook then observes",
    },
    Divergence {
        scenario: "post_tool/after-a-rewrite",
        pointer: "/steps/1/persisted/0/1/toolCalls/0/arguments",
        row: "4",
        reason: "the same key order, as persisted",
    },
    Divergence {
        scenario: "pre_tool/rewrite-invalid",
        pointer: "/conversations/1/2/content",
        row: "4",
        reason: "a rewrite that fails validation is denied naming the violations in this port's \
                 argument-rejection sentence, the one every call with invalid arguments reads, \
                 where the reference quotes Pydantic's `ValidationError`",
    },
    Divergence {
        scenario: "pre_tool/rewrite-invalid",
        pointer: "/steps/0/turn/7/params/patch/0/value/display/message",
        row: "4",
        reason: "the same rejection sentence, as the skipped call's header",
    },
    Divergence {
        scenario: "pre_tool/rewrite-invalid",
        pointer: "/steps/0/turn/7/params/patch/0/value/reason",
        row: "4",
        reason: "the same rejection sentence, as the skipped call's reason",
    },
    Divergence {
        scenario: "pre_tool/rewrite-invalid",
        pointer: "/steps/1/persisted/0/2/content",
        row: "4",
        reason: "the same rejection sentence, as persisted",
    },
    Divergence {
        scenario: "post_tool/tool-failure",
        pointer: "/conversations/1/2/content",
        row: "3",
        reason: "`read_file` reports a missing file in this port's own sentence (`NOTICE`), which \
                 the model reads under the reference's `<tool_error>read_file failed: ` frame",
    },
    Divergence {
        scenario: "post_tool/tool-failure",
        pointer: "/stdin/observe/0/tool_error",
        row: "3",
        reason: "the same sentence, as the post-tool hook's `tool_error`",
    },
    Divergence {
        scenario: "post_tool/tool-failure",
        pointer: "/stdin/observe/0/tool_output_text",
        row: "3",
        reason: "the same sentence, as the post-tool hook's `tool_output_text`",
    },
    Divergence {
        scenario: "post_tool/tool-failure",
        pointer: "/steps/0/turn/4/params/patch/0/value/display/message",
        row: "3",
        reason: "the same sentence, as the failed call's header",
    },
    Divergence {
        scenario: "post_tool/tool-failure",
        pointer: "/steps/0/turn/4/params/patch/0/value/error/message",
        row: "3",
        reason: "the same sentence, as the failed call's error",
    },
    Divergence {
        scenario: "post_tool/tool-failure",
        pointer: "/steps/1/persisted/0/2/content",
        row: "3",
        reason: "the same sentence, as persisted",
    },
    Divergence {
        scenario: "cancel/interrupted-tool",
        pointer: "/stdin/observe/0/tool_error",
        row: "34",
        reason: "a call the interrupt cut short reads this port's interruption sentence \
                 (`INTERRUPTED_TOOL_RESULT`), which the post-tool hooks it still runs are shown",
    },
    Divergence {
        scenario: "cancel/interrupted-tool",
        pointer: "/stdin/observe/0/tool_output_text",
        row: "34",
        reason: "the same sentence, as the post-tool hook's `tool_output_text`",
    },
    Divergence {
        scenario: "cancel/interrupted-tool",
        pointer: "/steps/1/persisted/0/2/content",
        row: "34",
        reason: "the same sentence, as persisted",
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

const FIELDS: &[FieldDivergence] = &[];

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
        .expect("python3 runs the hooks capture script")
}

#[test]
fn the_port_runs_every_hook_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    assert!(
        recorded.len() >= SCENARIO_FLOOR,
        "the corpus holds {} scenarios, below the floor of {SCENARIO_FLOOR}",
        recorded.len()
    );

    let output_path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hooks-parity-port.json");
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
            if let Some(index) = LEDGER
                .iter()
                .position(|entry| entry.scenario == name && pointer.starts_with(entry.pointer))
            {
                reproduced[index] += 1;
            } else if let Some(index) = FIELDS
                .iter()
                .position(|field| pointer.ends_with(field.suffix))
            {
                fields[index] += 1;
            } else {
                unexplained.push(format!("{name} {pointer}"));
            }
        }
    }
    let stale: Vec<String> = LEDGER
        .iter()
        .zip(&reproduced)
        .filter(|(_, count)| **count == 0)
        .map(|(entry, _)| format!("{} {}", entry.scenario, entry.pointer))
        .chain(
            FIELDS
                .iter()
                .zip(&fields)
                .filter(|(_, count)| **count == 0)
                .map(|(field, _)| field.suffix.to_owned()),
        )
        .collect();
    println!(
        "hooks parity: {conformant}/{} scenarios conformant, {} ledgered differences across \
         {} entries",
        recorded.len(),
        reproduced.iter().chain(&fields).sum::<usize>(),
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
fn every_ledger_entry_names_another_scorecard_row() {
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
            "the ledger entry for {} {} names row {}, which docs/parity.md does not carry",
            entry.scenario,
            entry.pointer,
            entry.row
        );
        assert_ne!(
            entry.row, "18",
            "row 18 is what this corpus measures, so a difference it keeps is a gap, not a \
             ledger entry: {} {}",
            entry.scenario, entry.pointer
        );
        assert!(
            recorded.contains_key(entry.scenario)
                && entry.pointer.starts_with('/')
                && !entry.reason.is_empty(),
            "the ledger entry for {} {} needs a recorded scenario, a pointer prefix and a reason",
            entry.scenario,
            entry.pointer
        );
    }
    for field in FIELDS {
        assert!(
            rows.contains(field.row) && field.row != "18",
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
    if let Some(reason) = off_pin_reason(&root, "hooks") {
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
