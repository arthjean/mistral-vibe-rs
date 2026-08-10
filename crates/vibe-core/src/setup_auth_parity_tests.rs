//! Differential oracle for the setup authentication surface.
//!
//! `scripts/parity/setup_auth.py` drives the reference's auth-state
//! assessment, credential persistence, keyring migration, sign-in service and
//! HTTP gateway over scripted inputs, with no network, no browser and no OS
//! credential store, and records what each one answers. This module replays
//! that corpus, family by family, against what the port answers today.
//!
//! The corpus is committed and replayed unconditionally: it carries
//! scenario-supplied values, vocabulary, call orders and verdicts, and every
//! reference-authored error sentence only as a length plus a SHA-256, which is
//! what `NOTICE` allows. Only the live recapture probe skips, and it names the
//! pin and the way back when it does.
//!
//! The oracle precedes the implementation, so the ledger below is the measured
//! defect inventory rather than a residue: a family the port cannot answer yet
//! is one `family/*` entry naming the stories that implement it, and the stale
//! check retires each entry the moment its family conforms. The `constants`
//! block is the one place the port already publishes an answer: the two
//! browser-auth defaults and the default key variable are read out of the
//! configuration registry through `default_document`, which is exactly the
//! surface `config/fields/read` serves to clients.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;

use crate::config::registry::default_document;
use crate::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};

const CORPUS_RELATIVE: &str = "crates/vibe-core/tests/setup-auth/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/setup_auth.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The scenario floor this replay commits to, so a regeneration that captured
/// almost nothing fails instead of reporting a clean but empty run.
const MINIMUM_SCENARIOS: usize = 140;
/// The reference publishes eleven sign-in error codes; a corpus recording any
/// other count was captured from something else.
const ERROR_CODE_COUNT: usize = 11;
/// The URL-validation floor the setup-parity PRD commits to.
const MINIMUM_URL_CASES: usize = 29;

/// Cases where this port answers something other than the reference, each with
/// the reason. A case that conforms while listed here fails the replay as a
/// stale entry, and a case that diverges without an entry fails naming the
/// family, the case and the observed and expected values.
///
/// A `family/*` entry covers every case of its family and goes stale only when
/// the whole family conforms: the oracle ships before the implementation, so
/// these entries are the measured backlog of the setup-parity PRD, each naming
/// the stories that retire it.
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "authState/*",
        "PENDING US-183: the port decides credential presence inline and binary in \
         crates/vibe-cli/src/tui/mod.rs and publishes no six-state provenance assessment to \
         compare",
    ),
    (
        "persistence/*",
        "PENDING US-184 and US-185: the port stores under the service name `mistral-vibe-rs`, \
         has no legacy read or migration, collapses every keyring failure into one error \
         instead of falling back to the global dotenv, and ships no removal path",
    ),
    (
        "signInProtocol/*",
        "PENDING US-187 and US-189: no sign-in gateway, no polling state machine and no \
         event vocabulary exist anywhere in crates/",
    ),
    (
        "urlValidation/*",
        "PENDING US-188: the port issues no sign-in requests, so no origin or path-prefix \
         validation exists to answer these verdicts",
    ),
    (
        "errorTaxonomy/*",
        "PENDING US-187: the port has no sign-in error type, so none of the eleven codes can \
         be produced, and the message digests stay uncompared until this port writes its own \
         sentences",
    ),
    (
        "constants/*",
        "PENDING US-184 (keyring service names live in the CLI adapter as `mistral-vibe-rs`), \
         US-187 (endpoint paths, challenge method, HTTP vocabulary and PKCE), US-188 (default \
         port table) and US-189 (poll cadence and status vocabulary): vibe-core publishes no \
         auth module to answer them; the three registry-backed defaults below are compared \
         for real and conform",
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
    auth_state: Vec<AuthStateCase>,
    persistence: Vec<PersistenceCase>,
    sign_in_protocol: Vec<ProtocolCase>,
    url_validation: Vec<UrlValidationCase>,
    error_taxonomy: Vec<ErrorCode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Constants {
    keyring_service: String,
    legacy_keyring_services: Vec<String>,
    default_env_key: String,
    browser_auth_base_url: String,
    browser_auth_api_base_url: String,
    poll_interval_seconds: f64,
    max_consecutive_poll_failures: u32,
    statuses: Vec<String>,
    http_gone_status: u16,
    default_ports: BTreeMap<String, u16>,
    code_challenge_method: String,
    sign_in_path: String,
    exchange_path_template: String,
    pkce: Pkce,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Pkce {
    scripted_verifier: String,
    scripted_challenge: String,
    generated_length: usize,
    generated_charset_is_unreserved: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AuthStateCase {
    case: String,
    #[expect(dead_code, reason = "US-183's assessment replays the scenario inputs")]
    env_key_kind: String,
    #[expect(dead_code, reason = "US-183's assessment replays the scenario inputs")]
    had_value_before_dotenv_load: bool,
    #[expect(dead_code, reason = "US-183's assessment replays the scenario inputs")]
    process_env: Option<String>,
    #[expect(dead_code, reason = "US-183's assessment replays the scenario inputs")]
    keyring: Option<String>,
    #[expect(dead_code, reason = "US-183's assessment replays the scenario inputs")]
    dotenv: String,
    /// The six-state verdict, absent when the reference raised instead.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    can_use_active_provider: Option<bool>,
    #[serde(default)]
    sign_out_available: Option<bool>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-183 compares the reported key variable")]
    reported_env_key: Option<String>,
    #[serde(default)]
    raised: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistenceCase {
    case: String,
    op: String,
    #[expect(dead_code, reason = "US-184 and US-185 replay the scenario inputs")]
    env_key: String,
    #[serde(default)]
    #[expect(dead_code, reason = "US-185 compares the telemetry flag")]
    custom_domain: Option<bool>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-185 compares the save-error shape")]
    outcome_detail_present: Option<bool>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-185 compares the process environment effect")]
    process_env_value: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-185 compares the dotenv effect")]
    dotenv_value: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-184 compares the stored services")]
    keyring_stored: Option<BTreeMap<String, String>>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-184 compares the ordered primitive calls")]
    keyring_calls: Option<Vec<String>>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-185 compares the telemetry events")]
    telemetry: Option<Vec<Value>>,
    #[serde(default)]
    raised: Option<String>,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProtocolCase {
    case: String,
    layer: String,
    #[serde(default)]
    #[expect(dead_code, reason = "US-187 and US-189 replay the scripted inputs")]
    op: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-187 replays the configured bases")]
    browser_base: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-187 replays the configured bases")]
    api_base: Option<String>,
    #[expect(dead_code, reason = "US-187 and US-189 replay the scripted inputs")]
    script: Value,
    #[serde(default)]
    #[expect(dead_code, reason = "US-189 compares the ordered event sequence")]
    events: Option<Vec<Value>>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-189 compares the gateway call order")]
    gateway_calls: Option<Vec<String>>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-189 compares the poll count")]
    poll_count: Option<u32>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-189 compares the sleep clamping")]
    sleeps: Option<Vec<f64>>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-190 compares the opened URLs")]
    browser_opened: Option<Vec<String>>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-187 compares the derived challenge")]
    challenge: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-187 compares the issued requests")]
    requests: Option<Vec<Value>>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-187 compares the parsed creation payload")]
    process_id: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-187 compares the parsed creation payload")]
    sign_in_url: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-187 compares the parsed creation payload")]
    poll_url: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-187 compares the normalized expiry")]
    expires_at: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-187 compares the parsed poll payload")]
    exchange_token: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-187 compares the parsed poll payload")]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UrlValidationCase {
    case: String,
    #[expect(dead_code, reason = "US-188's validator replays the inputs")]
    value: String,
    #[expect(dead_code, reason = "US-188's validator replays the inputs")]
    base: String,
    verdict: String,
    #[serde(default)]
    #[expect(
        dead_code,
        reason = "US-188 asserts the value passes through unchanged"
    )]
    returned_unchanged: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ErrorCode {
    name: String,
    value: String,
    messages: Vec<Digested>,
}

/// A reference-authored sentence by length and SHA-256 only; US-187 holds this
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
    let corpus: Corpus = serde_json::from_str(&raw).expect("the setup-auth corpus parses");
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

/// The divergence ledger as a lookup, keyed `family/case` or `family/*`.
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

    /// Records a case the port cannot answer yet as a divergence, so the
    /// pending family stays visible in the counts and its `family/*` ledger
    /// entry goes stale the moment a real comparator lands without one.
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
/// whose divergence no longer reproduces. A `family/*` entry is stale once its
/// family diverges nowhere.
fn settle(report: &Report, family: &str) -> usize {
    let recorded = ledger();
    let wildcard = format!("{family}/*");
    let unrecorded = report
        .divergences
        .iter()
        .filter(|line| {
            let key = line.split(':').next().unwrap_or_default();
            !recorded.contains_key(key) && !recorded.contains_key(&wildcard)
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
        .filter(|key| {
            if **key == wildcard {
                report.total > 0 && report.divergences.is_empty()
            } else {
                key.starts_with(&format!("{family}/")) && !report.observed.contains(key)
            }
        })
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "these {family} entries conform now and their ledger entry is stale: {stale:?}"
    );
    println!(
        "setup-auth: {family} {}/{} conform",
        report.conformant, report.total
    );
    report.total
}

// --------------------------------------------------------------------------
// Family runners
// --------------------------------------------------------------------------

/// The provider defaults the port actually publishes, read from the registry
/// exactly as `config/fields/read` serves them.
fn published_mistral_provider() -> toml::Table {
    let document = default_document();
    let providers = document
        .get("providers")
        .and_then(|value| value.as_array())
        .expect("the registry publishes a providers array");
    providers
        .iter()
        .filter_map(|value| value.as_table())
        .find(|table| table.get("name").and_then(|name| name.as_str()) == Some("mistral"))
        .expect("the registry publishes the mistral provider")
        .clone()
}

fn run_constants(corpus: &Constants) -> usize {
    let mut report = Report::default();
    let provider = published_mistral_provider();
    let string_field = |name: &str| {
        provider
            .get(name)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned()
    };
    report.check(
        "constants",
        "browserAuthBaseUrl",
        "registry default",
        &corpus.browser_auth_base_url,
        &string_field("browser_auth_base_url"),
    );
    report.check(
        "constants",
        "browserAuthApiBaseUrl",
        "registry default",
        &corpus.browser_auth_api_base_url,
        &string_field("browser_auth_api_base_url"),
    );
    report.check(
        "constants",
        "defaultEnvKey",
        "registry default",
        &corpus.default_env_key,
        &string_field("api_key_env_var"),
    );
    report.pending(
        "constants",
        "keyringService",
        corpus.keyring_service.clone(),
        "US-184",
    );
    report.pending(
        "constants",
        "legacyKeyringServices",
        format!("{:?}", corpus.legacy_keyring_services),
        "US-184",
    );
    report.pending(
        "constants",
        "pollIntervalSeconds",
        format!("{}", corpus.poll_interval_seconds),
        "US-189",
    );
    report.pending(
        "constants",
        "maxConsecutivePollFailures",
        format!("{}", corpus.max_consecutive_poll_failures),
        "US-189",
    );
    report.pending(
        "constants",
        "statuses",
        format!("{:?}", corpus.statuses),
        "US-189",
    );
    report.pending(
        "constants",
        "httpGoneStatus",
        format!("{}", corpus.http_gone_status),
        "US-187",
    );
    report.pending(
        "constants",
        "defaultPorts",
        format!("{:?}", corpus.default_ports),
        "US-188",
    );
    report.pending(
        "constants",
        "codeChallengeMethod",
        corpus.code_challenge_method.clone(),
        "US-187",
    );
    report.pending(
        "constants",
        "signInPath",
        corpus.sign_in_path.clone(),
        "US-187",
    );
    report.pending(
        "constants",
        "exchangePathTemplate",
        corpus.exchange_path_template.clone(),
        "US-187",
    );
    report.pending(
        "constants",
        "pkce",
        format!(
            "challenge {} for verifier {} (generated length {}, unreserved {})",
            corpus.pkce.scripted_challenge,
            corpus.pkce.scripted_verifier,
            corpus.pkce.generated_length,
            corpus.pkce.generated_charset_is_unreserved,
        ),
        "US-187",
    );
    settle(&report, "constants")
}

fn run_auth_state(cases: &[AuthStateCase]) -> usize {
    let kinds = cases
        .iter()
        .filter_map(|case| case.kind.clone())
        .collect::<BTreeSet<_>>();
    for expected in [
        "auth_not_required",
        "os_keyring",
        "process_env",
        "signed_out",
        "unsupported_provider",
        "vibe_home_env_file",
    ] {
        assert!(
            kinds.contains(expected),
            "the corpus reaches no {expected} verdict; regenerate it with {CAPTURE_SCRIPT}"
        );
    }
    let mut report = Report::default();
    for case in cases {
        let expected = match (&case.kind, &case.raised) {
            (Some(kind), _) => format!(
                "{kind} canUse={} signOut={}",
                case.can_use_active_provider.unwrap_or_default(),
                case.sign_out_available.unwrap_or_default(),
            ),
            (None, Some(raised)) => format!("raised {raised}"),
            (None, None) => panic!(
                "authState/{} records neither a verdict nor a raise",
                case.case
            ),
        };
        report.pending("authState", &case.case, expected, "US-183");
    }
    settle(&report, "authState")
}

fn run_persistence(cases: &[PersistenceCase]) -> usize {
    let mut report = Report::default();
    for case in cases {
        let expected = match case.op.as_str() {
            "persist" => format!(
                "outcome {}",
                case.outcome.as_deref().unwrap_or("<unrecorded>")
            ),
            "remove" => format!("raised {}", case.raised.as_deref().unwrap_or("nothing")),
            "keyringRead" => format!("value {}", case.value.as_deref().unwrap_or("none")),
            other => panic!("persistence/{} records unknown op {other}", case.case),
        };
        let story = if case.op == "keyringRead" {
            "US-184"
        } else {
            "US-185"
        };
        report.pending("persistence", &case.case, expected, story);
    }
    settle(&report, "persistence")
}

fn run_sign_in_protocol(cases: &[ProtocolCase]) -> usize {
    let mut report = Report::default();
    for case in cases {
        assert!(
            matches!(case.layer.as_str(), "service" | "gateway"),
            "signInProtocol/{} records unknown layer {}",
            case.case,
            case.layer
        );
        let expected = if let Some(code) = &case.error_code {
            format!("error {code}")
        } else if let Some(key) = &case.api_key {
            format!("api key {key}")
        } else if let Some(status) = &case.status {
            format!("poll status {status}")
        } else {
            "parsed payload".to_owned()
        };
        let story = if case.layer == "service" {
            "US-189"
        } else {
            "US-187"
        };
        report.pending("signInProtocol", &case.case, expected, story);
    }
    settle(&report, "signInProtocol")
}

fn run_url_validation(cases: &[UrlValidationCase]) -> usize {
    assert!(
        cases.len() >= MINIMUM_URL_CASES,
        "the corpus records {} URL validation cases, below the {MINIMUM_URL_CASES} the PRD \
         commits to; regenerate it with {CAPTURE_SCRIPT}",
        cases.len()
    );
    let mut report = Report::default();
    for case in cases {
        assert!(
            matches!(case.verdict.as_str(), "accepted" | "rejected"),
            "urlValidation/{} records unknown verdict {}",
            case.case,
            case.verdict
        );
        report.pending(
            "urlValidation",
            &case.case,
            format!("verdict {}", case.verdict),
            "US-188",
        );
    }
    settle(&report, "urlValidation")
}

fn run_error_taxonomy(cases: &[ErrorCode]) -> usize {
    assert_eq!(
        cases.len(),
        ERROR_CODE_COUNT,
        "the corpus records {} error codes where the reference declares {ERROR_CODE_COUNT}",
        cases.len()
    );
    for code in cases {
        assert!(
            !code.messages.is_empty(),
            "errorTaxonomy/{} carries no message digest; regenerate with {CAPTURE_SCRIPT}",
            code.value
        );
    }
    let mut report = Report::default();
    for code in cases {
        report.pending(
            "errorTaxonomy",
            &code.value,
            format!("{} = {}", code.name, code.value),
            "US-187",
        );
    }
    settle(&report, "errorTaxonomy")
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

#[test]
fn the_committed_corpus_replays_against_this_port() {
    let corpus = corpus();
    println!("setup-auth: divergence ledger");
    for (case, reason) in DIVERGENCES {
        println!("  {case}: {reason}");
    }
    let mut scenarios = 0;
    scenarios += run_constants(&corpus.constants);
    scenarios += run_auth_state(&corpus.auth_state);
    scenarios += run_persistence(&corpus.persistence);
    scenarios += run_sign_in_protocol(&corpus.sign_in_protocol);
    scenarios += run_url_validation(&corpus.url_validation);
    scenarios += run_error_taxonomy(&corpus.error_taxonomy);
    println!(
        "setup-auth: {scenarios} scenarios across 5 families plus the constants block \
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
    if let Some(reason) = off_pin_reason(&root, "setup-auth") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let script = repository.join(CAPTURE_SCRIPT);
    let recaptured = repository.join("target/setup-auth-corpus.json");
    let output = Command::new("python3")
        .arg(&script)
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/setup-auth-full.json"))
        .arg("--corpus")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the setup-auth capture script runs");
    assert!(
        output.status.success(),
        "the setup-auth capture failed: {}",
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
