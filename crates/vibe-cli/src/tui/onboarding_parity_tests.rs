//! Differential oracle for the onboarding screen graph.
//!
//! `scripts/parity/onboarding.py` drives the reference's real `OnboardingApp`
//! through Textual's headless pilot with the sign-in service, the credential
//! persistence and the config orchestrator replaced by recorders, and records
//! observations rather than output: the installed screen set, the ordered
//! transitions taken, the focus target per screen, the validation class per
//! input state, the effects persisted and the terminating value. Textual and
//! ratatui cannot render the same cells, so no rendered cell, SVG or styled
//! text appears in the corpus; the precedent is
//! `tasks/prd-chat-input-observable-parity.md`.
//!
//! The corpus is committed and replayed unconditionally: it carries screen
//! names, transition edges, class names, effect records and terminating
//! values, and every reference-authored message only as a length plus a
//! SHA-256, which is what `NOTICE` allows. Only the live recapture probe
//! skips, and it names the pin and the way back when it does.
//!
//! The oracle precedes the implementation: this build's setup flow is a chat
//! transcript walked by `SetupFlow` in `crates/vibe-cli/src/tui/setup.rs`,
//! with no screen graph to compare. The ledger below is therefore the
//! measured backlog of EP-055, one prefix entry per screen group naming the
//! story that closes it, and the stale check fails each entry the moment its
//! group conforms, which is what turns a landed screen into a retired ledger
//! row rather than a silent skip.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;

use vibe_core::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};

const CORPUS_RELATIVE: &str = "crates/vibe-cli/tests/onboarding/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/onboarding.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The scenario floor this replay commits to, so a regeneration that captured
/// almost nothing fails instead of reporting a clean but empty run.
const MINIMUM_SCENARIOS: usize = 50;
/// The reference installs seven screens for a browser-capable provider and
/// three for any other, which is the gating the corpus must exhibit.
const FULL_SCREEN_SET: usize = 7;
const MANUAL_SCREEN_SET: usize = 3;

/// Cases where this port answers something other than the reference, each
/// with the reason. A key ending in `*` covers every case it prefixes and
/// goes stale only when none of them diverges, so landing one EP-055 story
/// retires exactly its own entries. A case that conforms while listed here
/// fails the replay as a stale entry, and a case that diverges without an
/// entry fails naming the family, the case and the observed and expected
/// values.
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "screenGraph/graph-*",
        "PENDING US-191: the port's setup is a chat-transcript SetupFlow \
         (crates/vibe-cli/src/tui/setup.rs), so no screen set is installed and no gating \
         predicate exists to compare",
    ),
    (
        "screenGraph/cancel-*",
        "PENDING US-191: cancellation semantics belong to the screen graph the port does not \
         install yet",
    ),
    (
        "screenGraph/welcome-*",
        "PENDING US-192: no welcome screen exists, so the typing gate cannot be compared",
    ),
    (
        "screenGraph/theme-*",
        "PENDING US-192: no theme-selection screen exists, so the live preview and the \
         wrap-around navigation cannot be compared",
    ),
    (
        "screenGraph/auth-method-*",
        "PENDING US-193: no authentication-method screen exists in the port",
    ),
    (
        "screenGraph/target-*",
        "PENDING US-193: no sign-in-target screen exists, so the armed overwrite confirmation \
         cannot be compared",
    ),
    (
        "screenGraph/custom-domain-*",
        "PENDING US-193: no custom-domain screen exists, so the validation classes and the \
         derived URLs cannot be compared",
    ),
    (
        "screenGraph/browser-sign-in-*",
        "PENDING US-194: no browser sign-in screen exists, so the step progression, retry, \
         manual fallback and cancellation cannot be compared",
    ),
    (
        "screenGraph/api-key-*",
        "PENDING US-195: the port collects the key through the chat composer rather than a \
         masked screen input",
    ),
    (
        "terminating/*",
        "PENDING US-191: the port has no five-value terminating vocabulary; SetupFlow \
         completes or aborts inside the TUI process",
    ),
    (
        "domainValidation/*",
        "PENDING US-193: no custom-domain validator, private-cloud heuristic or URL \
         derivation exists in the port",
    ),
    (
        "constants/*",
        "PENDING US-192 (theme fades and gradient table) and US-194 (step names, success \
         delay and URL-help delay): none of these surfaces exists in the port yet",
    ),
];

// --------------------------------------------------------------------------
// The corpus
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    constants: Constants,
    screen_graph: Vec<GraphScenario>,
    terminating: Vec<TerminatingCase>,
    domain_validation: Vec<DomainCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Constants {
    sign_in_steps: u32,
    sign_in_step_names: Vec<String>,
    success_exit_delay_seconds: f64,
    sign_in_url_help_delay_seconds: f64,
    theme_visible_neighbors: u32,
    theme_fade_classes: Vec<String>,
    gradient_color_count: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GraphScenario {
    case: String,
    supports_browser_sign_in: bool,
    installed_screens: Vec<String>,
    entry_screen: String,
    transitions: Vec<Transition>,
    #[expect(dead_code, reason = "EP-055 compares the focus and state markers")]
    focus: BTreeMap<String, Value>,
    #[expect(dead_code, reason = "EP-055 compares the persisted effects")]
    effects: Value,
    result: Option<String>,
    #[expect(
        dead_code,
        reason = "US-192 compares the theme carried out of the flow"
    )]
    selected_theme: String,
    #[expect(dead_code, reason = "US-193 compares the applied browser-auth URLs")]
    provider_browser_auth: Value,
    #[serde(default)]
    #[expect(dead_code, reason = "US-192 compares the theme list order")]
    themes: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Transition {
    #[expect(dead_code, reason = "EP-055 compares the driving event per edge")]
    event: String,
    from: String,
    to: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TerminatingCase {
    case: String,
    value: Option<String>,
    exit_code: Option<i64>,
    theme_persisted: bool,
    #[expect(dead_code, reason = "US-192 compares the persisted theme write")]
    theme_writes: Vec<Value>,
    messages: Vec<Digested>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DomainCase {
    case: String,
    #[expect(dead_code, reason = "US-193's validator replays the input")]
    input: String,
    valid: bool,
    #[serde(default)]
    private_cloud_warning: Option<bool>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-193 compares the derived base URL")]
    derived_base_url: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-193 compares the derived API URL")]
    derived_api_base_url: Option<String>,
}

/// A reference-authored message by length and SHA-256 only; EP-055 holds this
/// port's own sentences permanently unequal to it.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Digested {
    length: usize,
    digest: String,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

fn corpus() -> Corpus {
    let path = repo_root().join(CORPUS_RELATIVE);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()));
    let corpus: Corpus = serde_json::from_str(&raw).expect("the onboarding corpus parses");
    assert_eq!(
        corpus.schema_version, CORPUS_SCHEMA_VERSION,
        "the corpus layout moved; regenerate it with {CAPTURE_SCRIPT}"
    );
    assert_eq!(
        corpus.reference.commit, REFERENCE_COMMIT,
        "the corpus was captured from an unpinned reference"
    );
    corpus
}

// --------------------------------------------------------------------------
// The ledger
// --------------------------------------------------------------------------

/// Whether a ledger entry covers a divergence key: an exact match, or a
/// `prefix*` entry the key starts with.
fn covers(entry: &str, key: &str) -> bool {
    entry
        .strip_suffix('*')
        .map_or(entry == key, |prefix| key.starts_with(prefix))
}

/// Records one comparison, so a family reports a count and a divergence names
/// itself instead of stopping at the first one.
#[derive(Default)]
struct Report {
    conformant: usize,
    total: usize,
    divergences: Vec<String>,
    observed: Vec<String>,
}

impl Report {
    fn check<T: PartialEq + std::fmt::Debug>(
        &mut self,
        family: &str,
        case: &str,
        field: &str,
        expected: &T,
        actual: &T,
    ) {
        self.total += 1;
        if expected == actual {
            self.conformant += 1;
            return;
        }
        self.observed.push(format!("{family}/{case}"));
        self.divergences.push(format!(
            "{family}/{case}: {field} diverges: reference {expected:?}, port {actual:?}"
        ));
    }

    /// Records a case the port cannot answer yet as a divergence, so the
    /// pending group stays visible in the counts and its ledger entry goes
    /// stale the moment a real comparator lands without one.
    fn pending(&mut self, family: &str, case: &str, expected: String, story: &str) {
        self.check(
            family,
            case,
            "answer",
            &expected,
            &format!("no port counterpart until {story}"),
        );
    }
}

/// Fails on any divergence the ledger does not name, and on any ledger entry
/// whose divergence no longer reproduces. A `prefix*` entry is stale once no
/// observed divergence starts with its prefix.
fn settle(report: &Report, family: &str) -> usize {
    let unrecorded = report
        .divergences
        .iter()
        .filter(|line| {
            let key = line.split(':').next().unwrap_or_default();
            !DIVERGENCES.iter().any(|(entry, _)| covers(entry, key))
        })
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        unrecorded.is_empty(),
        "{family} diverges from the reference and is unrecorded:\n{}",
        unrecorded.join("\n")
    );
    let family_prefix = format!("{family}/");
    let stale = DIVERGENCES
        .iter()
        .map(|(entry, _)| *entry)
        .filter(|entry| entry.starts_with(&family_prefix))
        .filter(|entry| !report.observed.iter().any(|key| covers(entry, key)))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "these {family} entries conform now and their ledger entry is stale: {stale:?}"
    );
    println!(
        "onboarding: {family} {}/{} conform",
        report.conformant, report.total
    );
    report.total
}

// --------------------------------------------------------------------------
// Family runners
// --------------------------------------------------------------------------

/// The EP-055 story that closes one screen-graph scenario, keyed by the case
/// prefix; a scenario the map cannot place fails loudly instead of being
/// silently ledgered.
fn closing_story(case: &str) -> &'static str {
    let table: &[(&str, &str)] = &[
        ("graph-", "US-191"),
        ("cancel-", "US-191"),
        ("welcome-", "US-192"),
        ("theme-", "US-192"),
        ("auth-method-", "US-193"),
        ("target-", "US-193"),
        ("custom-domain-", "US-193"),
        ("browser-sign-in-", "US-194"),
        ("api-key-", "US-195"),
    ];
    table
        .iter()
        .find(|(prefix, _)| case.starts_with(prefix))
        .map(|(_, story)| *story)
        .unwrap_or_else(|| panic!("no EP-055 story is mapped for scenario {case}"))
}

fn run_screen_graph(cases: &[GraphScenario]) -> usize {
    // The gating predicate is asserted on every scenario rather than on one of
    // each kind: a corpus where a single browser-capable run installed six
    // screens would otherwise replay clean everywhere the reference checkout
    // is absent, which is exactly where the committed corpus is the only
    // oracle left.
    for scenario in cases {
        let expected = if scenario.supports_browser_sign_in {
            FULL_SCREEN_SET
        } else {
            MANUAL_SCREEN_SET
        };
        assert_eq!(
            scenario.installed_screens.len(),
            expected,
            "screenGraph/{}: the corpus records {} installed screens where the reference \
             predicate says {expected}",
            scenario.case,
            scenario.installed_screens.len()
        );
    }
    assert!(
        cases
            .iter()
            .any(|scenario| scenario.supports_browser_sign_in),
        "the corpus drives no browser-capable provider"
    );
    let manual = cases
        .iter()
        .find(|scenario| !scenario.supports_browser_sign_in)
        .expect("the corpus drives a provider without browser sign-in");
    assert!(
        manual
            .transitions
            .iter()
            .any(|edge| edge.from == "theme_selection" && edge.to == "api_key"),
        "the corpus records no direct theme-to-key edge for a manual-only provider"
    );
    for scenario in cases {
        assert_eq!(
            scenario.entry_screen, "welcome",
            "screenGraph/{}: the reference enters through the welcome screen",
            scenario.case
        );
    }

    let mut report = Report::default();
    for scenario in cases {
        let expected = format!(
            "{} screens, {} transitions, result {}",
            scenario.installed_screens.len(),
            scenario.transitions.len(),
            scenario.result.as_deref().unwrap_or("cancelled"),
        );
        report.pending(
            "screenGraph",
            &scenario.case,
            expected,
            closing_story(&scenario.case),
        );
    }
    settle(&report, "screenGraph")
}

fn run_terminating(cases: &[TerminatingCase]) -> usize {
    let values = cases
        .iter()
        .map(|case| {
            case.value
                .clone()
                .unwrap_or_else(|| "<cancelled>".to_owned())
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        values.len(),
        5,
        "the corpus records {} terminating values where the reference returns 5",
        values.len()
    );
    let mut report = Report::default();
    for case in cases {
        assert!(
            !case.messages.is_empty() || case.value.is_none() && case.exit_code == Some(0),
            "terminating/{}: every exit path but silent success prints a message",
            case.case
        );
        let expected = format!(
            "exit {:?}, theme persisted {}",
            case.exit_code, case.theme_persisted
        );
        report.pending("terminating", &case.case, expected, "US-191");
    }
    settle(&report, "terminating")
}

fn run_domain_validation(cases: &[DomainCase]) -> usize {
    assert!(
        cases.iter().any(|case| case.valid),
        "the corpus accepts no domain at all; regenerate it with {CAPTURE_SCRIPT}"
    );
    assert!(
        cases.iter().any(|case| !case.valid),
        "the corpus rejects no domain at all; regenerate it with {CAPTURE_SCRIPT}"
    );
    assert!(
        cases
            .iter()
            .any(|case| case.private_cloud_warning == Some(true)),
        "the corpus exercises no private-cloud warning; regenerate it with {CAPTURE_SCRIPT}"
    );
    let mut report = Report::default();
    for case in cases {
        let expected = format!(
            "valid {}, warning {:?}",
            case.valid, case.private_cloud_warning
        );
        report.pending("domainValidation", &case.case, expected, "US-193");
    }
    settle(&report, "domainValidation")
}

fn run_constants(constants: &Constants) -> usize {
    let mut report = Report::default();
    report.pending(
        "constants",
        "signInSteps",
        format!(
            "{} steps {:?}",
            constants.sign_in_steps, constants.sign_in_step_names
        ),
        "US-194",
    );
    report.pending(
        "constants",
        "delays",
        format!(
            "success exit {}s, URL help {}s",
            constants.success_exit_delay_seconds, constants.sign_in_url_help_delay_seconds
        ),
        "US-194",
    );
    report.pending(
        "constants",
        "themePresentation",
        format!(
            "{} neighbors, fades {:?}, {} gradient colors",
            constants.theme_visible_neighbors,
            constants.theme_fade_classes,
            constants.gradient_color_count
        ),
        "US-192",
    );
    settle(&report, "constants")
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

#[test]
fn the_committed_corpus_replays_against_this_port() {
    let corpus = corpus();
    println!("onboarding: divergence ledger");
    for (case, reason) in DIVERGENCES {
        println!("  {case}: {reason}");
    }
    let mut scenarios = 0;
    scenarios += run_constants(&corpus.constants);
    scenarios += run_screen_graph(&corpus.screen_graph);
    scenarios += run_terminating(&corpus.terminating);
    scenarios += run_domain_validation(&corpus.domain_validation);
    println!(
        "onboarding: {scenarios} scenarios across 3 families plus the constants block \
         replayed at {}",
        &corpus.reference.commit[..12],
    );
    assert!(
        scenarios >= MINIMUM_SCENARIOS,
        "the corpus replays {scenarios} scenarios, below the {MINIMUM_SCENARIOS} floor; \
         regenerate it with {CAPTURE_SCRIPT}"
    );
}

/// The corpus is only an oracle for as long as it still describes the pinned
/// reference. This probe recaptures it where the checkout is present and on
/// the pin, and skips everywhere else naming the pin and the way back.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "onboarding") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let script = repository.join(CAPTURE_SCRIPT);
    let recaptured = repository.join("target/onboarding-corpus.json");
    let output = Command::new("python3")
        .arg(&script)
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/onboarding-full.json"))
        .arg("--corpus")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the onboarding capture script runs");
    assert!(
        output.status.success(),
        "the onboarding capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fresh = fs::read_to_string(&recaptured).expect("the recaptured corpus is readable");
    let committed =
        fs::read_to_string(repository.join(CORPUS_RELATIVE)).expect("the corpus is readable");
    let fresh: Value = serde_json::from_str(&fresh).expect("the recaptured corpus parses");
    let committed: Value = serde_json::from_str(&committed).expect("the corpus parses");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate \
         it with `{CAPTURE_SCRIPT}`"
    );
}
