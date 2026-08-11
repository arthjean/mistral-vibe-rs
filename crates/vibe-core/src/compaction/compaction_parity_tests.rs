//! Differential oracle for the pure half of compaction.
//!
//! `scripts/parity/compaction.py` drives the reference's own compaction
//! functions over scripted inputs and records what they answer. This module
//! replays that corpus against the port's, family by family.
//!
//! The corpus is committed and replayed unconditionally: it carries values the
//! scenarios supplied, tag names, counts and digests, and no reference-authored
//! sentence, which is what `NOTICE` allows. Only the live recapture probe skips,
//! and it names the pin and the way back when it does.
//!
//! The envelope is split rather than compared whole, for the same reason. Its
//! two prose runs are the reference's own sentences, so the corpus records each
//! one's length and SHA-256 and this port writes its own prose in their place, a
//! divergence the ledger names. Everything between them, which is every byte the
//! envelope's readers depend on, commits verbatim and is compared byte for byte.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::events::ModelMessage;
use crate::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};
use crate::text::hex_encode;

use super::context::{
    COMPACT_USER_MESSAGE_MAX_TOKENS, collect_prior_user_messages, drop_oldest_round,
    extract_summary, parse_previous_user_messages, render_compaction_context,
};
use super::manager::{
    CompactionPlan, CompactionPromptResolution, CompactionPrompts, PLACEHOLDER_SUMMARY, compact,
};
use super::manager_tests::{ScriptedAnswer, ScriptedProvider};
use super::tokens::{approx_token_count, truncate_middle_to_tokens};
use crate::provider::{ToolChoice, Usage};

const CORPUS_RELATIVE: &str = "crates/vibe-core/tests/compaction/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/compaction.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 3;
/// The scenario floor this epic commits to, so a regeneration that captured
/// almost nothing fails instead of reporting a clean but empty run.
const MINIMUM_SCENARIOS: usize = 60;

/// Cases where this port answers something other than the reference, each with
/// the reason. A case that conforms while listed here fails the replay as a
/// stale entry, and a case that diverges without an entry fails naming the
/// family, the scenario and the field.
///
/// The two entries are the envelope's prose runs, which `NOTICE` forbids
/// shipping: this port writes its own sentences covering the same three
/// directives, and the structure between them is held to the reference byte for
/// byte. Every other captured answer is reproduced, so any further divergence is
/// a regression rather than a known gap.
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "envelopeProse/preamble",
        "LICENSING: original prose covering the same continuation directives",
    ),
    (
        "envelopeProse/summaryLead",
        "LICENSING: original prose introducing the summary block",
    ),
    (
        "placeholderSummary/placeholder",
        "LICENSING: original wording for the summary a failed summarization falls back to",
    ),
    (
        "managerCallType/callType",
        "The reference labels the compaction request with the telemetry call type its request \
         census reads back; no model call here is routed through a census yet, so this port marks \
         the same request through the provider metadata instead",
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
    token_counts: Vec<TokenCount>,
    truncations: Vec<Truncation>,
    summary_extractions: Vec<SummaryExtraction>,
    round_drops: Vec<RoundDrop>,
    /// The two prose runs the reference's envelope carries, recorded once
    /// because the capture proves they are the same across every render.
    envelope_prose: EnvelopeProse,
    envelope_renders: Vec<EnvelopeRender>,
    envelope_parses: Vec<EnvelopeParse>,
    message_selections: Vec<MessageSelection>,
    /// The placeholder summary the reference substitutes when both calls
    /// failed, recorded by digest for the same reason the envelope's prose is.
    placeholder_summary: Digested,
    /// The manager's decision tree: what each scripted model answer makes the
    /// summarizer do, and what the conversation looks like afterward.
    manager_scenarios: Vec<ManagerScenario>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagerScenario {
    case: String,
    strict: bool,
    /// The transcript the scenario compacts.
    conversation: Vec<MessageEntry>,
    /// The scripted model answers, one per call the manager makes.
    answers: Vec<ScriptedStep>,
    extra_instructions: String,
    calls: Vec<ManagerCall>,
    /// The reasons the failure telemetry record carried, in order.
    failures: Vec<String>,
    /// What the stats hold afterward: zero once the transcript was replaced.
    context_tokens: u64,
    messages_after: Vec<ManagerMessage>,
    /// `returned`, `raised` or `overflowed`.
    outcome: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    summary: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScriptedStep {
    text: String,
    tool_calls: usize,
    overflow: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagerCall {
    /// The telemetry label the reference passes; a recorded divergence.
    call_type: String,
    messages: usize,
    model: String,
    overflow: bool,
    system_messages: usize,
    /// The model's thinking setting as the reference stringifies it.
    thinking: String,
    tool_choice: Option<String>,
    /// The number of tools passed, or `None` where the reference passes no tool
    /// list at all.
    tools: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagerMessage {
    role: String,
    content: String,
    injected: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Constants {
    compact_user_message_max_tokens: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TokenCount {
    case: String,
    text: String,
    count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Truncation {
    case: String,
    text: String,
    max_tokens: i64,
    result: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SummaryExtraction {
    case: String,
    text: String,
    summary: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RoundDrop {
    case: String,
    messages: Vec<MessageEntry>,
    result: Option<Vec<RoleAndContent>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleAndContent {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EnvelopeProse {
    preamble: Digested,
    summary_lead: Digested,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EnvelopeRender {
    case: String,
    messages: Vec<MessageEntry>,
    summary: String,
    /// The envelope with both prose runs removed: the preserved-messages block
    /// and the summary block, concatenated, which is every byte a reader of the
    /// envelope depends on.
    structure: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Digested {
    length: usize,
    digest: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EnvelopeParse {
    case: String,
    /// The text to parse, when the scenario wrote it.
    #[serde(default)]
    content: Option<String>,
    /// The render case whose output to parse, for a round trip. The replay
    /// renders it again rather than reading the preamble out of the corpus.
    #[serde(default)]
    render: Option<String>,
    parsed: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MessageSelection {
    case: String,
    messages: Vec<MessageEntry>,
    summary_prefix: String,
    /// The budget the scenario set, or [`None`] for the module's own default.
    max_tokens: Option<i64>,
    selected: Vec<Selected>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selected {
    content: String,
    injected: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageEntry {
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    injected: bool,
    /// Set instead of `content` when the message carries a rendered envelope.
    #[serde(default)]
    render: Option<String>,
    #[serde(default)]
    tool_call_id: Option<String>,
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
    let corpus: Corpus = serde_json::from_str(&raw).expect("the compaction corpus parses");
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

/// The divergence ledger as a lookup, keyed `family/case`.
fn ledger() -> BTreeMap<String, String> {
    DIVERGENCES
        .iter()
        .map(|(case, reason)| ((*case).to_owned(), (*reason).to_owned()))
        .collect()
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
}

/// Fails on any divergence the ledger does not name, and on any ledger entry
/// whose divergence no longer reproduces.
fn settle(report: &Report, family: &str) -> usize {
    let recorded = ledger();
    let unrecorded = report
        .divergences
        .iter()
        .filter(|line| {
            let key = line.split(':').next().unwrap_or_default();
            !recorded.contains_key(key)
        })
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        unrecorded.is_empty(),
        "{family} diverges from the reference and is unrecorded:\n{}",
        unrecorded.join("\n")
    );
    let stale = recorded
        .keys()
        .filter(|key| key.starts_with(&format!("{family}/")) && !report.observed.contains(key))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "these {family} entries conform now and their ledger entry is stale: {stale:?}"
    );
    println!(
        "compaction: {family} {}/{} conform",
        report.conformant, report.total
    );
    report.total
}

fn digest_of(value: &str) -> Digested {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    Digested {
        length: value.chars().count(),
        digest: hex_encode(&hasher.finalize()),
    }
}

const MESSAGES_OPEN: &str = "<previous_user_messages>";
const MESSAGES_CLOSE: &str = "</previous_user_messages>";
const SUMMARY_BLOCK_OPEN: &str = "<compaction_summary>";
/// What stands where the prose was, matching the two markers the capture script
/// writes. Both sides mask their own sentences, so what is compared is every
/// byte the envelope's readers depend on and nothing either side authored.
const PREAMBLE_MARK: &str = "{preamble}";
const SUMMARY_LEAD_MARK: &str = "{summaryLead}";
/// What stands where the placeholder summary was, for the same reason.
const PLACEHOLDER_MARK: &str = "{placeholderSummary}";

/// Where the envelope's three structural anchors sit, or [`None`] when `text` is
/// not an envelope. The reserved tags are escaped inside every preserved
/// message, so a text carrying all three in order is one and the split is
/// unambiguous.
fn anchors(text: &str) -> Option<(usize, usize, usize)> {
    let open_at = text.find(MESSAGES_OPEN)?;
    let close_at = text[open_at..].find(MESSAGES_CLOSE)? + open_at + MESSAGES_CLOSE.len();
    let summary_at = text[close_at..].find(SUMMARY_BLOCK_OPEN)? + close_at;
    Some((open_at, close_at, summary_at))
}

/// `text` with the two prose runs an envelope carries replaced by markers, the
/// way `mask_envelope` in the capture script replaces them. Anything that is not
/// an envelope comes back untouched.
fn mask_envelope(text: &str) -> String {
    let Some((open_at, close_at, summary_at)) = anchors(text) else {
        return text.to_owned();
    };
    format!(
        "{PREAMBLE_MARK}{}{SUMMARY_LEAD_MARK}{}",
        &text[open_at..close_at],
        &text[summary_at..]
    )
}

/// The two prose runs `text` carries, each by length and SHA-256.
fn envelope_prose(text: &str) -> (Digested, Digested) {
    let (open_at, close_at, summary_at) =
        anchors(text).expect("a rendered envelope carries its structural anchors");
    (
        digest_of(&text[..open_at]),
        digest_of(&text[close_at..summary_at]),
    )
}

/// The message a scenario entry describes, with a rendered envelope resolved
/// through the renders this replay produced itself.
fn build(entry: &MessageEntry, envelopes: &BTreeMap<String, String>) -> ModelMessage {
    let content = match (&entry.content, &entry.render) {
        (Some(content), _) => content.clone(),
        (None, Some(case)) => envelopes
            .get(case)
            .unwrap_or_else(|| panic!("the corpus names an unknown render case `{case}`"))
            .clone(),
        (None, None) => String::new(),
    };
    match entry.role.as_str() {
        "system" => ModelMessage::System { content },
        "user" => {
            if entry.injected {
                ModelMessage::injected_user(content)
            } else {
                ModelMessage::user(content)
            }
        }
        "assistant" => ModelMessage::Assistant {
            content,
            reasoning: None,
            reasoning_signature: None,
            reasoning_state: Vec::new(),
            tool_calls: Vec::new(),
        },
        "tool" => ModelMessage::Tool {
            call_id: entry.tool_call_id.clone().unwrap_or_default(),
            content,
            is_error: false,
        },
        other => panic!("the corpus names an unknown role `{other}`"),
    }
}

fn role_of(message: &ModelMessage) -> &'static str {
    match message {
        ModelMessage::System { .. } => "system",
        ModelMessage::User { .. } => "user",
        ModelMessage::Assistant { .. } => "assistant",
        ModelMessage::Tool { .. } => "tool",
    }
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

#[tokio::test]
async fn the_committed_corpus_replays_every_family_the_reference_answered() {
    let corpus = corpus();
    assert_eq!(
        corpus.constants.compact_user_message_max_tokens, COMPACT_USER_MESSAGE_MAX_TOKENS,
        "the reference moved its preservation budget"
    );

    let mut scenarios = 0;

    let mut report = Report::default();
    for case in &corpus.token_counts {
        report.check(
            "tokenCounts",
            &case.case,
            "count",
            &case.count,
            &approx_token_count(&case.text),
        );
    }
    scenarios += settle(&report, "tokenCounts");

    let mut report = Report::default();
    for case in &corpus.truncations {
        report.check(
            "truncations",
            &case.case,
            "result",
            &case.result,
            &truncate_middle_to_tokens(&case.text, case.max_tokens),
        );
    }
    scenarios += settle(&report, "truncations");

    let mut report = Report::default();
    for case in &corpus.summary_extractions {
        report.check(
            "summaryExtractions",
            &case.case,
            "summary",
            &case.summary,
            &extract_summary(&case.text),
        );
    }
    scenarios += settle(&report, "summaryExtractions");

    let mut report = Report::default();
    for case in &corpus.round_drops {
        let messages = case
            .messages
            .iter()
            .map(|entry| build(entry, &BTreeMap::new()))
            .collect::<Vec<_>>();
        let dropped = drop_oldest_round(&messages).map(|kept| {
            kept.iter()
                .map(|message| (role_of(message).to_owned(), message.content().to_owned()))
                .collect::<Vec<_>>()
        });
        let expected = case.result.as_ref().map(|kept| {
            kept.iter()
                .map(|item| (item.role.clone(), item.content.clone()))
                .collect::<Vec<_>>()
        });
        report.check("roundDrops", &case.case, "result", &expected, &dropped);
    }
    scenarios += settle(&report, "roundDrops");

    // The renders are replayed first: the parse round trips and the selection
    // scenarios that carry a prior envelope read the text this port produced,
    // never a preamble stored in the corpus.
    let mut envelopes: BTreeMap<String, String> = BTreeMap::new();
    let mut report = Report::default();
    let mut prose = Report::default();
    for case in &corpus.envelope_renders {
        let messages = case
            .messages
            .iter()
            .map(|entry| build(entry, &BTreeMap::new()))
            .collect::<Vec<_>>();
        let rendered = render_compaction_context(&messages, &case.summary);
        report.check(
            "envelopeRenders",
            &case.case,
            "structure",
            &case.structure,
            &mask_envelope(&rendered),
        );
        // The prose is constant across renders, which the capture proves before
        // it records it, so it is compared once rather than once per case: a
        // ledger entry per case would say the same thing eight times.
        if prose.total == 0 {
            let (preamble, summary_lead) = envelope_prose(&rendered);
            prose.check(
                "envelopeProse",
                "preamble",
                "digest",
                &corpus.envelope_prose.preamble,
                &preamble,
            );
            prose.check(
                "envelopeProse",
                "summaryLead",
                "digest",
                &corpus.envelope_prose.summary_lead,
                &summary_lead,
            );
        }
        envelopes.insert(case.case.clone(), rendered);
    }
    scenarios += settle(&report, "envelopeRenders");
    // The prose is a recorded divergence rather than a conformance scenario, so
    // it is settled against the ledger without joining the count.
    settle(&prose, "envelopeProse");

    let mut report = Report::default();
    for case in &corpus.envelope_parses {
        let content = match (&case.content, &case.render) {
            (Some(content), _) => content.clone(),
            (None, Some(render)) => envelopes
                .get(render)
                .unwrap_or_else(|| panic!("the corpus names an unknown render case `{render}`"))
                .clone(),
            (None, None) => panic!("`{}` carries neither content nor a render", case.case),
        };
        report.check(
            "envelopeParses",
            &case.case,
            "parsed",
            &case.parsed,
            &parse_previous_user_messages(&content),
        );
    }
    scenarios += settle(&report, "envelopeParses");

    let mut report = Report::default();
    for case in &corpus.message_selections {
        let messages = case
            .messages
            .iter()
            .map(|entry| build(entry, &envelopes))
            .collect::<Vec<_>>();
        let selected = collect_prior_user_messages(
            &messages,
            &case.summary_prefix,
            case.max_tokens.unwrap_or(COMPACT_USER_MESSAGE_MAX_TOKENS),
        );
        // A selection returns an envelope verbatim when the transcript carries
        // one that is not injected and is therefore a real user turn, so the
        // prose is masked out here the same way the renders mask it.
        let actual = selected
            .iter()
            .map(|message| (mask_envelope(message.content()), message.is_injected()))
            .collect::<Vec<_>>();
        let expected = case
            .selected
            .iter()
            .map(|item| (item.content.clone(), item.injected))
            .collect::<Vec<_>>();
        report.check(
            "messageSelections",
            &case.case,
            "selected",
            &expected,
            &actual,
        );
    }
    scenarios += settle(&report, "messageSelections");

    scenarios += replay_manager(&corpus).await;

    println!(
        "compaction: {scenarios} scenarios across 8 families conform at {}",
        &corpus.reference.commit[..12],
    );
    assert!(
        scenarios >= MINIMUM_SCENARIOS,
        "the corpus replays {scenarios} scenarios, below the {MINIMUM_SCENARIOS} this epic \
         commits to; regenerate it with {CAPTURE_SCRIPT}"
    );
}

/// The eighth family: the manager's decision tree, replayed by driving this
/// port's summarizer with the same scripted answers the capture drove the
/// reference's with.
///
/// Each scenario is compared on five axes: the sequence of calls, what each one
/// carried, how the compaction ended, which failure reasons were reported, and
/// the transcript left behind. Two axes cannot be compared as recorded and are
/// named in the ledger instead: the reference's telemetry call type, which this
/// port marks through the provider metadata, and the placeholder summary, which
/// is reference-authored prose this port writes its own wording for.
async fn replay_manager(corpus: &Corpus) -> usize {
    let mut report = Report::default();
    let mut call_type = Report::default();
    let mut placeholder = Report::default();

    for scenario in &corpus.manager_scenarios {
        let messages: Vec<ModelMessage> = scenario
            .conversation
            .iter()
            .map(|entry| build(entry, &BTreeMap::new()))
            .collect();
        let provider = ScriptedProvider::new(scenario.answers.iter().map(|step| ScriptedAnswer {
            text: step.text.clone(),
            tool_calls: step.tool_calls,
            overflow: step.overflow,
            usage: Usage::default(),
        }));
        // The reference's own scenario config: no tools, `auto` as the tool
        // choice, the active model as the compaction model, and its thinking
        // setting on the primary call.
        let reference_model = scenario
            .calls
            .first()
            .map(|call| call.model.clone())
            .unwrap_or_default();
        let plan = CompactionPlan {
            prompts: CompactionPromptResolution::Resolved(CompactionPrompts {
                request: "compaction request".to_owned(),
                fallback_system: "fallback system".to_owned(),
                summary_prefix: "Another language model".to_owned(),
            }),
            model: Some(reference_model.clone()),
            thinking: scenario
                .calls
                .first()
                .is_some_and(|call| call.thinking != "off"),
            tools: Vec::new(),
            tool_choice: Some(ToolChoice::Auto),
            strict: scenario.strict,
            ..CompactionPlan::default()
        };
        let outcome = compact(&provider, &plan, &messages, &scenario.extra_instructions).await;

        let calls = provider.calls();
        report.check(
            "managerScenarios",
            &scenario.case,
            "calls",
            &scenario.calls.len(),
            &calls.len(),
        );
        for (index, (expected, actual)) in scenario.calls.iter().zip(&calls).enumerate() {
            let field = format!("call {index}");
            // `tools` records `None` where the reference passes no list at all,
            // which this port cannot express: an absent list and an empty one
            // are the same request here, and the tool choice is what actually
            // distinguishes the two calls.
            report.check(
                "managerScenarios",
                &scenario.case,
                &field,
                &(
                    expected.messages,
                    expected.system_messages,
                    expected.tools.unwrap_or_default(),
                    expected.tool_choice.clone(),
                    expected.thinking != "off",
                    expected.model.clone(),
                    expected.overflow,
                ),
                &(
                    actual.messages,
                    actual.system_messages,
                    actual.tools,
                    actual.tool_choice.as_ref().map(tool_choice_label),
                    actual.thinking,
                    actual.model.clone().unwrap_or_default(),
                    actual.overflow,
                ),
            );
            if call_type.total == 0 {
                call_type.check(
                    "managerCallType",
                    "callType",
                    "label",
                    &expected.call_type,
                    &"session_compaction".to_owned(),
                );
            }
        }

        let (observed_outcome, observed_reason, observed_failures, replaced) = match &outcome {
            Ok(compacted) => (
                "returned",
                None,
                compacted
                    .failure
                    .map(|reason| reason.label().to_owned())
                    .into_iter()
                    .collect::<Vec<_>>(),
                true,
            ),
            Err(failure) => match failure.reason {
                Some(reason) => (
                    "raised",
                    Some(reason.label().to_owned()),
                    vec![reason.label().to_owned()],
                    false,
                ),
                None => ("overflowed", None, Vec::new(), false),
            },
        };
        report.check(
            "managerScenarios",
            &scenario.case,
            "outcome",
            &scenario.outcome,
            &observed_outcome.to_owned(),
        );
        report.check(
            "managerScenarios",
            &scenario.case,
            "reason",
            &scenario.reason,
            &observed_reason,
        );
        report.check(
            "managerScenarios",
            &scenario.case,
            "failures",
            &scenario.failures,
            &observed_failures,
        );
        // The reference zeroes the context size inside the manager; this port
        // does it in the turn ledger, which resets exactly when the transcript
        // was replaced. What is compared is that decision, not where it lives.
        report.check(
            "managerScenarios",
            &scenario.case,
            "contextTokens",
            &scenario.context_tokens,
            &if replaced { 0 } else { scenario.context_tokens },
        );

        // The summary, and the transcript left behind: the live conversation
        // when the compaction failed, the replacement when it succeeded.
        let observed_summary = outcome
            .as_ref()
            .ok()
            .map(|compacted| mask_placeholder(&compacted.summary));
        report.check(
            "managerScenarios",
            &scenario.case,
            "summary",
            &scenario.summary,
            &observed_summary,
        );
        let observed_messages: Vec<(String, String, bool)> = outcome
            .as_ref()
            .map_or_else(|_| messages.clone(), |compacted| compacted.messages.clone())
            .iter()
            .map(|message| {
                (
                    role_of(message).to_owned(),
                    mask_placeholder(&mask_envelope(message.content())),
                    message.is_injected(),
                )
            })
            .collect();
        let expected_messages: Vec<(String, String, bool)> = scenario
            .messages_after
            .iter()
            .map(|entry| (entry.role.clone(), entry.content.clone(), entry.injected))
            .collect();
        report.check(
            "managerScenarios",
            &scenario.case,
            "messagesAfter",
            &expected_messages,
            &observed_messages,
        );

        if placeholder.total == 0 && scenario.summary.as_deref() == Some(PLACEHOLDER_MARK) {
            placeholder.check(
                "placeholderSummary",
                "placeholder",
                "digest",
                &corpus.placeholder_summary,
                &digest_of(PLACEHOLDER_SUMMARY),
            );
        }
    }

    // Both recorded divergences are settled against the ledger without joining
    // the conformance count, the way the envelope's prose already is.
    settle(&call_type, "managerCallType");
    settle(&placeholder, "placeholderSummary");
    settle(&report, "managerScenarios")
}

/// The label the corpus records for a tool choice, which is the string the
/// reference passes.
fn tool_choice_label(choice: &ToolChoice) -> String {
    match choice {
        ToolChoice::Auto => "auto".to_owned(),
        ToolChoice::None => "none".to_owned(),
        ToolChoice::Required => "required".to_owned(),
        ToolChoice::Tool { name } => name.clone(),
    }
}

/// `text` with this port's placeholder summary replaced by the marker the
/// capture writes in place of the reference's.
fn mask_placeholder(text: &str) -> String {
    text.replace(PLACEHOLDER_SUMMARY, PLACEHOLDER_MARK)
}

/// The corpus is only an oracle for as long as it still describes the pinned
/// reference. This probe recaptures it where the checkout is present and on the
/// pin, and skips everywhere else naming the pin and the way back.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "compaction") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let script = repository.join(CAPTURE_SCRIPT);
    let recaptured = repository.join("target/compaction-recapture.json");
    let output = Command::new("python3")
        .arg(&script)
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(&recaptured)
        .arg("--corpus")
        .arg(repository.join("target/compaction-corpus.json"))
        .current_dir(&repository)
        .output()
        .expect("the compaction capture script runs");
    assert!(
        output.status.success(),
        "the compaction capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fresh = fs::read_to_string(repository.join("target/compaction-corpus.json"))
        .expect("the recaptured corpus is readable");
    let committed =
        fs::read_to_string(repository.join(CORPUS_RELATIVE)).expect("the corpus is readable");
    let fresh: Value = serde_json::from_str(&fresh).expect("the recaptured corpus parses");
    let committed: Value = serde_json::from_str(&committed).expect("the corpus parses");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate it \
         with `{CAPTURE_SCRIPT} --corpus`"
    );
}
