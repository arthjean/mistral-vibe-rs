//! Replays the committed ACP corpus against this port's `vibe-acp`.
//!
//! `scripts/parity/acp.py` captured the corpus from the pinned reference's own
//! `vibe-acp`: every scenario starts the agent over stdio in a fresh home,
//! behind a scripted stand-in for the chat-completions API, and records what
//! the agent writes. Driving this crate's binary through the same script with
//! `--agent` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Both sides reduce a string their agent authored to a length and a SHA-256.
//! This port writes its own prose on purpose (`NOTICE`), so two digests that
//! differ are a difference unless a prose ledger below names their pointer,
//! counts it and finds the port's length within its band; everything else is
//! compared exactly.
//!
//! The corpus also records how the entry point answers argument vectors that
//! exit before it serves anything, which is compared with no ledger at all.
//!
//! The script captures each harness backend into its own corpus: `corpus.json`
//! runs both sides on the legacy backend through the escape hatch, and
//! `unified-corpus.json` on the Unified Harness every session opens on by
//! default. Both replay against the same ledger.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce on each backend, so rows 8 and 36 of
//! `docs/parity.md` are readings of the summaries this file prints.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, Class, Divergence, OwnProse, Tally, assert_ledger, differences, entries,
    pinned_reference, scenarios,
};

/// The corpora, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("acp-parity/corpus.json");
const UNIFIED_CORPUS: &str = include_str!("acp-parity/unified-corpus.json");

/// The corpora as `scripts/parity/manifest.json` lists them.
const LISTED: &str = "crates/vibe-acp/tests/acp-parity/corpus.json";
const UNIFIED_LISTED: &str = "crates/vibe-acp/tests/acp-parity/unified-corpus.json";

/// The accepted divergences the editor's prose stands on.
const AUTH_LABELS: &str = "The ACP authentication method labels";
const SERVER_PROSE: &str = "Server refusals and errors are this port's own prose";
const HELP_LINES: &str = "The help document's authored lines";
const OPTION_NAMES: &str = "A path scope's refusal and option names are this port's own";
const OWN_SENTENCES: &str = "Programmatic mode's own sentences";
const EDITOR_PROSE: &str = "The editor adapter's own prose";

/// A request whose parameters fail validation is refused with the field and
/// the cause in a few words, where the reference relays pydantic's whole
/// report, the offending input and a documentation link among it: a twelfth
/// to a quarter of its length.
const VALIDATION_BAND: Band = Band {
    low: 0.05,
    high: 2.0,
};

/// The checkpoint a loaded or forked session's history closes on is a full
/// sentence where the reference writes a 15-character marker, 2.4 times as
/// long.
const CHECKPOINT_BAND: Band = Band {
    low: 0.5,
    high: 2.5,
};

/// A session whose MCP servers cannot be reached says so with each
/// transport's own error quoted in full, up to 2.7 times the reference's
/// notice.
const MCP_FAILURE_BAND: Band = Band {
    low: 0.5,
    high: 3.0,
};

/// An own-words entry of the editor replay, which row 8 answers for.
const fn own(
    scenarios: &'static [&'static str],
    pointer: &'static str,
    count: usize,
    band: Band,
    accepted: &'static str,
    reason: &'static str,
) -> OwnProse {
    OwnProse {
        scenarios,
        pointer,
        count,
        band,
        directives: None,
        row: "8",
        accepted,
        reason,
    }
}

/// The refusal of one request the capture sends without the field it needs.
const fn refusal(scenario: &'static [&'static str], pointer: &'static str) -> OwnProse {
    own(
        scenario,
        pointer,
        1,
        VALIDATION_BAND,
        SERVER_PROSE,
        "a request that fails validation is refused naming the field and the cause, without \
         the validator's whole report",
    )
}

/// The texts this port words its own way on both backends, by the same
/// count. A replay takes the first entry that covers a difference, so the
/// entries narrowed to a scenario or a step come before the broad ones.
const COMMON_PROSE: &[OwnProse] = &[
    refusal(&["ext/connectors"], "/6/response/error/message"),
    refusal(&["ext/log-level"], "/6/response/error/message"),
    refusal(&["ext/loops"], "/9/response/error/message"),
    refusal(&["ext/project-links"], "/8/response/error/message"),
    refusal(&["ext/review"], "/6/response/error/message"),
    refusal(&["ext/review"], "/7/response/error/message"),
    refusal(&["ext/review"], "/10/response/error/message"),
    refusal(&["ext/session-delete"], "/6/response/error/message"),
    refusal(&["ext/session-title"], "/4/response/error/message"),
    refusal(&["ext/session-title"], "/6/response/error/message"),
    refusal(&["ext/trust"], "/6/response/error/message"),
    refusal(&["ext/trust"], "/7/response/error/message"),
    refusal(&["ext/whoami"], "/4/response/error/message"),
    own(
        &["ext/log-level"],
        "/*/response/error/data/details",
        1,
        Band::DEFAULT,
        SERVER_PROSE,
        "the detail a refused log level carries",
    ),
    own(
        &["session/new-mcp-unreachable"],
        "/*/stream/*/message/params/update/content/text",
        1,
        MCP_FAILURE_BAND,
        EDITOR_PROSE,
        "the notice naming the MCP servers a new session could not reach",
    ),
    own(
        &["command/help"],
        "/*/stream/*/message/params/update/content/text",
        1,
        Band::DEFAULT,
        HELP_LINES,
        "`/help` answers with this port's own headings and shortcut lines",
    ),
    own(
        &["*"],
        "/*/stream/*/message/params/update/content/text",
        16,
        Band::DEFAULT,
        EDITOR_PROSE,
        "what a slash command answers with: a compaction, the data retention notice, the \
         lean mode, the log location and the like",
    ),
    own(
        &["*"],
        "/*/response/result/authMethods/*/description",
        84,
        Band::DEFAULT,
        AUTH_LABELS,
        "the description of each authentication method `initialize` advertises",
    ),
    own(
        &["*"],
        "/*/response/result/authMethods/*/name",
        84,
        Band::DEFAULT,
        AUTH_LABELS,
        "the name of each authentication method `initialize` advertises",
    ),
    own(
        &["*"],
        "/*/response/result/configOptions/*/name",
        74,
        Band::DEFAULT,
        EDITOR_PROSE,
        "the name of each configuration option a session publishes",
    ),
    own(
        &["command/lean"],
        "/*/stream/*/message/params/update/configOptions/*/name",
        2,
        Band::DEFAULT,
        EDITOR_PROSE,
        "the same names, as the update a mode change sends",
    ),
    own(
        &["command/compact"],
        "/*/stream/*/message/params/update/title",
        2,
        Band::DEFAULT,
        EDITOR_PROSE,
        "the title of the tool call a compaction is shown as",
    ),
    own(
        &[
            "prompt/bash-approved",
            "prompt/bash-approved-for-session",
            "prompt/bash-dismissed",
            "prompt/bash-rejected",
        ],
        "/*/stream/*/message/params/options/*/_meta/required_permissions/*/label",
        8,
        Band::DEFAULT,
        OPTION_NAMES,
        "the label of the permission a command approval would grant",
    ),
];

/// The texts whose count differs between the backends: the legacy one here,
/// the unified one below.
const PROSE: &[OwnProse] = &[
    own(
        &["session/fork-at-message", "session/load"],
        "/*/stream/*/message/params/update/content/*/content/text",
        2,
        CHECKPOINT_BAND,
        OWN_SENTENCES,
        "the checkpoint a loaded or forked session's history closes on",
    ),
    own(
        &["session/fork-at-message", "session/load"],
        "/*/stream/*/message/params/update/title",
        2,
        CHECKPOINT_BAND,
        OWN_SENTENCES,
        "the same checkpoint, as the title of the call it is shown as",
    ),
    own(
        &["*"],
        "/*/response/error/message",
        42,
        Band::DEFAULT,
        SERVER_PROSE,
        "the message of every other refusal the agent answers with",
    ),
    own(
        &["*"],
        "/*/detached/*/params/update/availableCommands/*/description",
        864,
        Band::DEFAULT,
        EDITOR_PROSE,
        "the description of each slash command the session advertises",
    ),
    own(
        &["*"],
        "/*/detached/*/params/update/availableCommands/*/input/hint",
        290,
        Band::DEFAULT,
        EDITOR_PROSE,
        "the hint of each slash command that takes an argument",
    ),
    own(
        &["*"],
        "/*/stream/*/message/params/update/content/*/content/text",
        6,
        Band::DEFAULT,
        EDITOR_PROSE,
        "the text of a tool call the adapter writes itself: a compaction, a review or a \
         rewind, a dismissed or rejected command",
    ),
    own(
        &["command/skill"],
        "/*/stream/*/message/params/update/rawOutput/content",
        1,
        Band::DEFAULT,
        EDITOR_PROSE,
        "the sentence a skill's content reaches the model inside",
    ),
    own(
        &["ext/review", "ext/rewind"],
        "/*/stream/*/message/params/update/rawOutput/message",
        2,
        Band::DEFAULT,
        EDITOR_PROSE,
        "what a review or a rewind reports when it settles",
    ),
    own(
        &["prompt/bash-rejected"],
        "/*/stream/*/message/params/update/rawOutput",
        1,
        Band::DEFAULT,
        EDITOR_PROSE,
        "what a rejected command reports",
    ),
    own(
        &[
            "prompt/question-declined",
            "prompt/question-elicited",
            "prompt/question-invalid-answer",
        ],
        "/*/stream/*/message/params/message",
        3,
        Band::DEFAULT,
        EDITOR_PROSE,
        "the prompt a question the model asks is put to the editor with",
    ),
    own(
        &["*"],
        "/*/stream/*/message/params/options/*/name",
        7,
        Band::DEFAULT,
        OPTION_NAMES,
        "the name of each option a permission request offers",
    ),
];
const UNIFIED_PROSE: &[OwnProse] = &[
    own(
        &["*"],
        "/*/response/error/message",
        36,
        Band::DEFAULT,
        SERVER_PROSE,
        "the message of every other refusal the agent answers with",
    ),
    own(
        &["*"],
        "/*/detached/*/params/update/availableCommands/*/description",
        1022,
        Band::DEFAULT,
        EDITOR_PROSE,
        "the description of each slash command the session advertises",
    ),
    own(
        &["*"],
        "/*/detached/*/params/update/availableCommands/*/input/hint",
        440,
        Band::DEFAULT,
        EDITOR_PROSE,
        "the hint of each slash command that takes an argument",
    ),
    own(
        &["command/compact", "command/skill"],
        "/*/stream/*/message/params/update/content/*/content/text",
        3,
        Band::DEFAULT,
        EDITOR_PROSE,
        "the text of a tool call the adapter writes itself: a compaction and the skill it \
         loads",
    ),
    own(
        &[
            "prompt/bash-approved",
            "prompt/bash-approved-for-session",
            "prompt/bash-dismissed",
            "prompt/bash-rejected",
        ],
        "/*/stream/*/message/params/options/*/name",
        4,
        Band::DEFAULT,
        OPTION_NAMES,
        "the name of each option a permission request offers",
    ),
];

/// The help text `-h` and `--help` print, which this port words its own way
/// under the same usage line, options and order.
const ARGV_PROSE: &[OwnProse] = &[OwnProse {
    scenarios: &["argv/*"],
    pointer: "/stdout",
    count: 6,
    band: Band::DEFAULT,
    directives: None,
    row: "7",
    accepted: "The CLI help's descriptions are this port's own",
    reason: "`vibe-acp --help` describes its options in this port's own sentences",
}];

/// The script's two families, one per backend.
const LEGACY: Capture = Capture {
    script: "scripts/parity/acp.py",
    family: &["--harness", "legacy"],
    arguments: JOBS,
};
const UNIFIED: Capture = Capture {
    script: "scripts/parity/acp.py",
    family: &["--harness", "unified"],
    arguments: JOBS,
};

/// How many scenarios the capture script runs side by side. Each owns its
/// directories, backend and agent process.
const JOBS: &[&str] = &["--jobs", "6"];

const LEDGER: &[Divergence] = &[
    Divergence {
        scenarios: &["initialize/terminal-auth"],
        pointer: "/0/response/result/authMethods/1",
        within: &[],
        count: 6,
        row: "8",
        class: Class::Accepted("The ACP terminal sign-in relaunches this executable"),
        reason: "the reference runs under a Python interpreter, so its terminal sign-in \
                 relaunches `python3` with the script as the first argument; a native \
                 executable announces itself with `--setup`, which is the reference's own \
                 branch for one",
    },
    Divergence {
        scenarios: &["ext/config-schema"],
        pointer: "/1/response/result",
        within: &[],
        count: 265,
        row: "30",
        class: Class::Gap,
        reason: "`_config/schema` relays the app server's `config/schema`, whose content is \
                 the configuration model",
    },
];

#[test]
fn the_port_answers_every_legacy_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    replay(CORPUS, &LEGACY, "legacy", PROSE);
}

#[test]
fn the_port_answers_every_unified_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    replay(UNIFIED_CORPUS, &UNIFIED, "unified", UNIFIED_PROSE);
}

fn replay(corpus: &str, capture: &Capture, harness: &str, prose: &[OwnProse]) {
    // The manifest entry holds the floors of both the scenarios and the
    // argument vectors, so a recapture that lost either fails here.
    let corpus_text = corpus;
    let mut tally = Tally::new(
        corpus,
        entries(COMMON_PROSE)
            .chain(entries(prose))
            .chain(entries(LEDGER)),
    );
    let corpus: Value = serde_json::from_str(corpus).expect("the corpus parses");
    let recorded = scenarios(&corpus);

    let replayed = capture.port(
        &["--agent", env!("CARGO_BIN_EXE_vibe-acp")],
        &Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("acp-parity-port-{harness}.json")),
    );
    // The command line answers before any scenario runs: every vector exits
    // the way the reference's argparse does, and only the help text, which
    // this port words its own way, may differ.
    let argv = corpus["argv"]
        .as_array()
        .expect("the corpus records the command line");
    let replayed_argv = replayed["argv"]
        .as_array()
        .expect("the port capture records the command line");
    assert_eq!(
        argv.len(),
        replayed_argv.len(),
        "the capture script declares other argument vectors than the corpus records"
    );
    let mut vectors = Tally::new(corpus_text, entries(ARGV_PROSE));
    for (index, (reference, port)) in argv.iter().zip(replayed_argv).enumerate() {
        let found = differences(reference, port);
        vectors.record(&format!("argv/{index}"), reference, port, found);
    }
    vectors.conclude(&format!("acp argv parity ({harness}):"));

    tally.compare(capture, &recorded, &scenarios(&replayed), differences);
    tally.conclude(&format!("acp parity ({harness}):"));
}

#[test]
fn every_ledger_entry_names_a_scorecard_row_and_a_recorded_scenario() {
    for (listed, corpus, prose) in [
        (LISTED, CORPUS, PROSE),
        (UNIFIED_LISTED, UNIFIED_CORPUS, UNIFIED_PROSE),
    ] {
        let corpus: Value = serde_json::from_str(corpus).expect("the corpus parses");
        assert_ledger(
            listed,
            &scenarios(&corpus),
            entries(COMMON_PROSE)
                .chain(entries(prose))
                .chain(entries(LEDGER))
                .chain(entries(ARGV_PROSE)),
        );
    }
}

/// Recaptures the pinned reference on each backend and asserts the committed
/// corpora are still what it answers. The replay above runs regardless, which
/// is what keeps a missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    if let Some(root) = pinned_reference("ACP") {
        LEGACY.check(&root, &[]);
        UNIFIED.check(&root, &[]);
    }
}
