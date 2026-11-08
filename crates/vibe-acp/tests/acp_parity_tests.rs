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
//! From v3.0.0 the reference runs one backend, the Unified Harness, so one
//! corpus records it; the legacy backend's corpus went with the escape hatch.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 8 of `docs/parity.md` is a
//! reading of the summaries this file prints.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use serde_json::Value;
use vibe_core::parity::replay::{
    Band, Capture, Class, Divergence, OwnProse, PatternDivergence, Tally, assert_ledger,
    differences, entries, pinned_reference, scenarios,
};

/// The corpora, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("acp-parity/corpus.json");

/// The corpora as `scripts/parity/manifest.json` lists them.
const LISTED: &str = "crates/vibe-acp/tests/acp-parity/corpus.json";

/// The accepted divergences the editor's prose stands on.
const AUTH_LABELS: &str = "The ACP authentication method labels";
const SERVER_PROSE: &str = "Server refusals and errors are this port's own prose";
const HELP_LINES: &str = "The help document's authored lines";
const OPTION_NAMES: &str = "A path scope's refusal and option names are this port's own";
const EDITOR_PROSE: &str = "The editor adapter's own prose";

/// A request whose parameters fail validation is refused with the field and
/// the cause in a few words, where the reference relays pydantic's whole
/// report, the offending input and a documentation link among it: a twelfth
/// to a quarter of its length.
const VALIDATION_BAND: Band = Band {
    low: 0.05,
    high: 2.0,
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
        73,
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

/// The texts this port words its own way on the one backend v3.1.0 runs.
const PROSE: &[OwnProse] = &[
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
        &["*"],
        "/*/stream/*/message/params/update/content/*/content/text",
        4,
        Band::DEFAULT,
        EDITOR_PROSE,
        "the text of a tool call the adapter writes itself: a compaction, a review or a \
         rewind, a dismissed or rejected command",
    ),
    own(
        &["*"],
        "/*/stream/*/message/params/options/*/name",
        4,
        Band::DEFAULT,
        OPTION_NAMES,
        "the name of each option a permission request offers",
    ),
];

/// The help text `-h` and `--help` print, which this port words its own way
/// under the same usage line, options and order.
/// No own-words entry holds the help of `vibe-acp` at v3.1.0: its length
/// left the band, so the vectors' differences are the gaps below.
const ARGV_PROSE: &[OwnProse] = &[];

/// Why the entries below are gaps: what the v3.1.0 re-pin revealed, kept
/// against the row this corpus measures until a follow-on closes it.
const ARGV_V3_REASON: &str = "v3.1.0 parses the editor binary's arguments otherwise, with the backend flags removed, which this port does not reproduce yet";

/// Differences an own-words entry covers but whose length v3.1.0 moved out of
/// its band, one by one; chained first, so they are decided before it.
const ARGV_V3_REFUSED: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "argv/10",
        pointer: "/stdout",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: ARGV_V3_REASON,
    },
    PatternDivergence {
        scenario: "argv/11",
        pointer: "/stdout",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: ARGV_V3_REASON,
    },
    PatternDivergence {
        scenario: "argv/3",
        pointer: "/stdout",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: ARGV_V3_REASON,
    },
    PatternDivergence {
        scenario: "argv/4",
        pointer: "/stdout",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: ARGV_V3_REASON,
    },
    PatternDivergence {
        scenario: "argv/5",
        pointer: "/stdout",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: ARGV_V3_REASON,
    },
    PatternDivergence {
        scenario: "argv/6",
        pointer: "/stdout",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: ARGV_V3_REASON,
    },
];

/// Differences no entry above covers, by pointer with its indices
/// generalized, most specific first; chained last.
const ARGV_V3_GAPS: &[PatternDivergence] = &[PatternDivergence {
    scenario: "*",
    pointer: "/stderr",
    count: 12,
    row: "8",
    class: Class::Gap,
    reason: ARGV_V3_REASON,
}];

const CAPTURE: Capture = Capture {
    script: "scripts/parity/acp.py",
    family: &[],
    arguments: JOBS,
};

/// How many scenarios the capture script runs side by side. Each owns its
/// directories, backend and agent process.
const JOBS: &[&str] = &["--jobs", "6"];
/// Why the entries below are gaps: what the v3.1.0 re-pin revealed, kept
/// against the row this corpus measures until a follow-on closes it.
const V3_REASON: &str = "v3.1.0 answers the editor protocol otherwise on the Unified Harness the reference now runs on alone, which this port does not reproduce yet";

/// Differences an own-words entry covers but whose length v3.1.0 moved out of
/// its band, one by one; chained first, so they are decided before it.
const V3_REFUSED: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "command/retry",
        pointer: "/4/stream/1/message/params/update/content/text",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "session/fork",
        pointer: "/3/stream/1/message/params/update/content/text",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "session/fork-at-message",
        pointer: "/3/stream/1/message/params/update/content/text",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "session/fork-at-message",
        pointer: "/6/stream/0/message/params/update/title",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "session/fork-at-message",
        pointer: "/6/stream/4/message/params/update/content/text",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "session/load",
        pointer: "/5/stream/0/message/params/update/title",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

/// Differences no entry above covers, by pointer with its indices
/// generalized, most specific first; chained last.
const V3_GAPS: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/error",
        count: 7,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result",
        count: 7,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/apiKey",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/budget",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/customDomainLocked",
        count: 3,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/error",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/planOffer/url",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/usage",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/detached/*",
        count: 2,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/detached/*/params/update/size",
        count: 229,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/configOptions/*/currentValue",
        count: 2,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/sessions/*/title",
        count: 2,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stream/*",
        count: 40,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stream/*/message/params/update/_meta/tool_name",
        count: 12,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stream/*/message/params/update/rawInput/changes",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stream/*/message/params/update/rawInput/newString",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stream/*/message/params/update/rawInput/oldString",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stream/*/message/params/update/rawInput/replaceAll",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stream/*/message/params/update/rawOutput/previousContent",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stream/*/message/params/update/title",
        count: 2,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/configOptions/*/options/*",
        count: 290,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/configOptions/*/options/*/name",
        count: 73,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/response/result/configOptions/*/options/*/value",
        count: 73,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stream/*/message/params/update/rawOutput/occurrences/*/newStartLine",
        count: 1,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stream/*/message/params/update/configOptions/*/options/*",
        count: 8,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stream/*/message/params/update/configOptions/*/options/*/name",
        count: 2,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/*/stream/*/message/params/update/configOptions/*/options/*/value",
        count: 2,
        row: "8",
        class: Class::Gap,
        reason: V3_REASON,
    },
];

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
        count: 301,
        row: "30",
        class: Class::Gap,
        reason: "`_config/schema` relays the app server's `config/schema`, whose content is \
                 the configuration model",
    },
];

#[test]
fn the_port_answers_every_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    replay(CORPUS, &CAPTURE);
}

fn replay(corpus: &str, capture: &Capture) {
    // The manifest entry holds the floors of both the scenarios and the
    // argument vectors, so a recapture that lost either fails here.
    let corpus_text = corpus;
    let mut tally = Tally::new(
        corpus,
        entries(V3_REFUSED)
            .chain(entries(COMMON_PROSE))
            .chain(entries(PROSE))
            .chain(entries(LEDGER))
            .chain(entries(V3_GAPS)),
    );
    let corpus: Value = serde_json::from_str(corpus).expect("the corpus parses");
    let recorded = scenarios(&corpus);

    let replayed = capture.port(
        &["--agent", env!("CARGO_BIN_EXE_vibe-acp")],
        &Path::new(env!("CARGO_TARGET_TMPDIR")).join("acp-parity-port.json"),
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
    let mut vectors = Tally::new(
        corpus_text,
        entries(ARGV_V3_REFUSED)
            .chain(entries(ARGV_PROSE))
            .chain(entries(ARGV_V3_GAPS)),
    );
    for (index, (reference, port)) in argv.iter().zip(replayed_argv).enumerate() {
        let found = differences(reference, port);
        vectors.record(&format!("argv/{index}"), reference, port, found);
    }
    vectors.conclude("acp argv parity:");

    tally.compare(capture, &recorded, &scenarios(&replayed), differences);
    tally.conclude("acp parity:");
}

#[test]
fn every_ledger_entry_names_a_scorecard_row_and_a_recorded_scenario() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    assert_ledger(
        LISTED,
        &scenarios(&corpus),
        entries(V3_REFUSED)
            .chain(entries(COMMON_PROSE))
            .chain(entries(PROSE))
            .chain(entries(LEDGER))
            .chain(entries(V3_GAPS))
            .chain(entries(ARGV_V3_REFUSED))
            .chain(entries(ARGV_PROSE))
            .chain(entries(ARGV_V3_GAPS)),
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which
/// is what keeps a missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    if let Some(root) = pinned_reference("ACP") {
        CAPTURE.check(&root, &[]);
    }
}
