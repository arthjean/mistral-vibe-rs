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
    Band, Capture, Class, Divergence, OwnProse, Replay, assert_ledger, entries, scenarios,
};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("hooks-parity/corpus.json");

/// The corpus as `scripts/parity/manifest.json` lists it.
const LISTED: &str = "crates/vibe-app-server/tests/hooks-parity/corpus.json";

const CAPTURE: Capture = Capture {
    script: "scripts/parity/hooks.py",
    family: &[],
    // How many scenarios the capture script runs side by side. Each owns its
    // directories, backend and server processes.
    arguments: &["--jobs", "4"],
};

/// The accepted divergences the entries below stand on.
const SORTED_ARGUMENTS: &str = "A rewritten tool call lists its arguments with keys sorted";
const INVALID_ARGUMENTS: &str = "The label an argument of the wrong class is answered with";
const TOOL_RESULTS: &str = "Authored result and error text in six tools";
const LOOP_SENTENCES: &str = "The conversation loop's own sentences";

const LEDGER: &[Divergence] = &[
    Divergence {
        scenarios: &["pre_tool/rewrite-declaration-order"],
        pointer: "/conversations/1/1/toolCalls/0/arguments",
        within: &[],
        count: 1,
        row: "4",
        class: Class::Accepted(SORTED_ARGUMENTS),
        reason: "a rewritten call's arguments carry the same fields and values, with keys sorted \
                 where the reference's `model_dump` lists them in declaration order: this \
                 workspace builds `serde_json` without `preserve_order`, so no tool schema here \
                 keeps its declaration order to dump by",
    },
    Divergence {
        scenarios: &["pre_tool/rewrite-declaration-order"],
        pointer: "/steps/1/persisted/0/1/toolCalls/0/arguments",
        within: &[],
        count: 1,
        row: "4",
        class: Class::Accepted(SORTED_ARGUMENTS),
        reason: "the same key order, as persisted",
    },
    Divergence {
        scenarios: &["post_tool/after-a-rewrite"],
        pointer: "/conversations/1/1/toolCalls/0/arguments",
        within: &[],
        count: 1,
        row: "4",
        class: Class::Accepted(SORTED_ARGUMENTS),
        reason: "the same key order, in the call a `post_tool` hook then observes",
    },
    Divergence {
        scenarios: &["post_tool/after-a-rewrite"],
        pointer: "/steps/1/persisted/0/1/toolCalls/0/arguments",
        within: &[],
        count: 1,
        row: "4",
        class: Class::Accepted(SORTED_ARGUMENTS),
        reason: "the same key order, as persisted",
    },
];

/// Every text this port words its own way in these scenarios.
const PROSE: &[OwnProse] = &[
    OwnProse {
        scenarios: &["pre_tool/rewrite-invalid"],
        pointer: "/conversations/1/2/content",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "4",
        accepted: INVALID_ARGUMENTS,
        reason: "a rewrite that fails validation is denied naming the violations in this port's \
                 argument-rejection sentence, the one every call with invalid arguments reads, \
                 where the reference quotes Pydantic's `ValidationError`",
    },
    OwnProse {
        scenarios: &["pre_tool/rewrite-invalid"],
        pointer: "/steps/0/turn/7/params/patch/0/value/display/message",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "4",
        accepted: INVALID_ARGUMENTS,
        reason: "the same rejection sentence, as the skipped call's header",
    },
    OwnProse {
        scenarios: &["pre_tool/rewrite-invalid"],
        pointer: "/steps/0/turn/7/params/patch/0/value/reason",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "4",
        accepted: INVALID_ARGUMENTS,
        reason: "the same rejection sentence, as the skipped call's reason",
    },
    OwnProse {
        scenarios: &["pre_tool/rewrite-invalid"],
        pointer: "/steps/1/persisted/0/2/content",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "4",
        accepted: INVALID_ARGUMENTS,
        reason: "the same rejection sentence, as persisted",
    },
    OwnProse {
        scenarios: &["post_tool/tool-failure"],
        pointer: "/conversations/1/2/content",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "`read_file` reports a missing file in this port's own sentence (`NOTICE`), which \
                 the model reads under the reference's `<tool_error>read_file failed: ` frame",
    },
    OwnProse {
        scenarios: &["post_tool/tool-failure"],
        pointer: "/stdin/observe/0/tool_error",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "the same sentence, as the post-tool hook's `tool_error`",
    },
    OwnProse {
        scenarios: &["post_tool/tool-failure"],
        pointer: "/stdin/observe/0/tool_output_text",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "the same sentence, as the post-tool hook's `tool_output_text`",
    },
    OwnProse {
        scenarios: &["post_tool/tool-failure"],
        pointer: "/steps/0/turn/4/params/patch/0/value/display/message",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "the same sentence, as the failed call's header",
    },
    OwnProse {
        scenarios: &["post_tool/tool-failure"],
        pointer: "/steps/0/turn/4/params/patch/0/value/error/message",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "the same sentence, as the failed call's error",
    },
    OwnProse {
        scenarios: &["post_tool/tool-failure"],
        pointer: "/steps/1/persisted/0/2/content",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "3",
        accepted: TOOL_RESULTS,
        reason: "the same sentence, as persisted",
    },
    OwnProse {
        scenarios: &["cancel/interrupted-tool"],
        pointer: "/stdin/observe/0/tool_error",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "34",
        accepted: LOOP_SENTENCES,
        reason: "a call the interrupt cut short reads this port's interruption sentence \
                 (`INTERRUPTED_TOOL_RESULT`), which the post-tool hooks it still runs are shown",
    },
    OwnProse {
        scenarios: &["cancel/interrupted-tool"],
        pointer: "/stdin/observe/0/tool_output_text",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "34",
        accepted: LOOP_SENTENCES,
        reason: "the same sentence, as the post-tool hook's `tool_output_text`",
    },
    OwnProse {
        scenarios: &["cancel/interrupted-tool"],
        pointer: "/steps/1/persisted/0/2/content",
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "34",
        accepted: LOOP_SENTENCES,
        reason: "the same sentence, as persisted",
    },
];

#[test]
fn the_port_runs_every_hook_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    Replay {
        heading: "hooks parity:",
        corpus: CORPUS,
        capture: &CAPTURE,
        port: &[
            "--server",
            env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture"),
        ],
        output: &Path::new(env!("CARGO_TARGET_TMPDIR")).join("hooks-parity-port.json"),
    }
    .run(entries(PROSE).chain(entries(LEDGER)));
}

#[test]
fn every_ledger_entry_names_another_scorecard_row() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(
        LISTED,
        &scenarios(&corpus),
        entries(PROSE).chain(entries(LEDGER)),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    CAPTURE.probe("hooks");
}
