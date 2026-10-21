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

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Capture, Entry, FieldDivergence, Prose, Replay, entries, scenarios, scorecard_rows,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("hooks-parity/corpus.json");

/// The scorecard the ledger's `row` values point into.
const SCORECARD: &str = include_str!("../../../docs/parity.md");

const CAPTURE: Capture = Capture {
    script: "scripts/parity/hooks.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 39;

/// A difference this port keeps in one scenario, and the row of
/// `docs/parity.md` that answers for it.
struct Divergence {
    scenario: &'static str,
    /// Every difference whose JSON pointer starts with this prefix is covered.
    pointer: &'static str,
    row: &'static str,
    reason: &'static str,
}

impl Entry for Divergence {
    fn covers(&self, scenario: &str, pointer: &str) -> bool {
        self.scenario == scenario && pointer.starts_with(self.pointer)
    }

    fn label(&self) -> String {
        self.pointer.to_owned()
    }

    fn scenarios(&self) -> &[&str] {
        std::slice::from_ref(&self.scenario)
    }
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

const FIELDS: &[FieldDivergence] = &[];

#[test]
fn the_port_runs_every_hook_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "hooks parity:",
        corpus: CORPUS,
        floor: SCENARIO_FLOOR,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("hooks-parity-port.json"),
        prose: Prose::Compared,
    }
    .run(entries(LEDGER).chain(entries(FIELDS)));
}

#[test]
fn every_ledger_entry_names_another_scorecard_row() {
    let rows = scorecard_rows(SCORECARD);
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
    FieldDivergence::assert_ledger(FIELDS, SCORECARD, "18");
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("hooks");
}
