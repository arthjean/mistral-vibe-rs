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
//! The replay drives this port's [`super::onboarding::model::OnboardingModel`]
//! through the same scripts: each corpus scenario maps to a drive that feeds
//! the same key presses, scripted sign-in feeds and persistence outcomes, and
//! every observation is compared field by field. The reference-authored
//! messages and every sentence its screens draw are committed as a length
//! plus a SHA-256, which is what `NOTICE` allows, and the replay holds this
//! port's own sentences permanently unequal to them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use toml::Table;
use toml::Value as TomlValue;

use vibe_core::auth::{
    DEFAULT_BROWSER_AUTH_API_BASE_URL, DEFAULT_BROWSER_AUTH_BASE_URL, PersistOutcome,
    SignInErrorCode, SignInStatus,
};
use vibe_core::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};

use super::onboarding::context::{
    self, OnboardingContext, is_likely_mistral_private_cloud_domain, is_valid_custom_domain,
    resolve_browser_auth_urls,
};
use super::onboarding::exit_plan;
use super::onboarding::model::{
    GRADIENT_COLORS, KeyPress, ModelEffect, ModelEvent, OnboardingModel, OnboardingOutcome,
    OnboardingPorts, SIGN_IN_STEP_NAMES, SIGN_IN_URL_HELP_DELAY_SECONDS,
    SUCCESS_EXIT_DELAY_SECONDS, THEME_FADE_CLASSES, THEME_VISIBLE_NEIGHBORS,
};
use super::themes::sorted_theme_names;

const CORPUS_RELATIVE: &str = "crates/vibe-cli/tests/onboarding/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/onboarding.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 2;
/// The scenario floor this replay commits to, so a regeneration that captured
/// almost nothing fails instead of reporting a clean but empty run.
const MINIMUM_SCENARIOS: usize = 50;
/// The reference installs seven screens for a browser-capable provider and
/// three for any other, which is the gating the corpus must exhibit.
const FULL_SCREEN_SET: usize = 7;
const MANUAL_SCREEN_SET: usize = 3;

/// Cases where this port answers something other than the reference, each
/// with the reason. EP-055 landed the screen graph, so the ledger is empty: a
/// case that diverges fails naming the family, the case and the observed and
/// expected values, and a new divergence needs a new named entry here.
const DIVERGENCES: &[(&str, &str)] = &[];

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
    screen_prose: Vec<ProseModule>,
}

/// One reference screen module's authored runs, each by length and SHA-256
/// only. `NOTICE` forbids shipping the sentences themselves, so the replay
/// asserts permanent inequality rather than equality.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProseModule {
    module: String,
    runs: Vec<Digested>,
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
    focus: BTreeMap<String, Value>,
    effects: Effects,
    result: Option<String>,
    selected_theme: String,
    provider_browser_auth: ProviderBrowserAuth,
    #[serde(default)]
    themes: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Transition {
    event: String,
    from: String,
    to: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Effects {
    /// Absent for a provider that installs no sign-in screens.
    #[serde(default)]
    factory_calls: Option<u64>,
    persist_calls: Vec<Value>,
    provider_writes: Vec<Value>,
    service_closes: u64,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderBrowserAuth {
    api_base_url: Option<String>,
    base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TerminatingCase {
    case: String,
    value: Option<String>,
    exit_code: Option<i64>,
    theme_persisted: bool,
    theme_writes: Vec<Value>,
    messages: Vec<Digested>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DomainCase {
    case: String,
    input: String,
    valid: bool,
    #[serde(default)]
    private_cloud_warning: Option<bool>,
    #[serde(default)]
    derived_base_url: Option<String>,
    #[serde(default)]
    derived_api_base_url: Option<String>,
}

/// A reference-authored message by length and SHA-256 only; this port's own
/// sentences are held permanently unequal to it.
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
// The drive harness
// --------------------------------------------------------------------------

/// What the scripted sign-in worker does for one attempt, standing in for the
/// stub service the capture script installs.
#[derive(Debug, Clone, Copy)]
enum Feed {
    /// The attempt starts and stays pending until cancelled.
    Pending,
    /// The attempt runs to completion and returns this key.
    Success(&'static str),
    /// The attempt fails after the waiting status.
    Failure,
}

const ORACLE_SIGN_IN_URL: &str = "https://console.mistral.ai/oracle/sign-in";

/// Records every persistence call the way the capture's recorders do, and
/// answers with the scripted outcome.
struct Recorder {
    factory_calls: u64,
    service_closes: u64,
    open_attempt: bool,
    feeds: VecDeque<Feed>,
    persist_outcome: PersistOutcome,
    persist_calls: Vec<Value>,
    provider_writes: Vec<Value>,
}

impl Recorder {
    fn new(feeds: Vec<Feed>, persist_outcome: PersistOutcome) -> Self {
        Self {
            factory_calls: 0,
            service_closes: 0,
            open_attempt: false,
            feeds: feeds.into(),
            persist_outcome,
            persist_calls: Vec::new(),
            provider_writes: Vec::new(),
        }
    }
}

impl OnboardingPorts for Recorder {
    fn persist_api_key(
        &mut self,
        env_key: &str,
        provider: &Table,
        api_key: &str,
        custom_domain: bool,
    ) -> PersistOutcome {
        self.persist_calls.push(json!({
            "apiKey": api_key,
            "customDomain": custom_domain,
            "envKey": env_key,
            "provider": provider.get("name").and_then(TomlValue::as_str),
        }));
        self.persist_outcome.clone()
    }

    fn persist_provider(&mut self, provider: &Table) -> bool {
        let field = |key: &str| provider.get(key).and_then(TomlValue::as_str);
        self.provider_writes.push(json!({
            "browserAuthApiBaseUrl": field("browser_auth_api_base_url"),
            "browserAuthBaseUrl": field("browser_auth_base_url"),
            "provider": field("name"),
        }));
        true
    }
}

/// One scripted drive: the model, the recorder, and the observations the
/// corpus compares.
struct Drive {
    model: OnboardingModel,
    recorder: Recorder,
    edges: Vec<(String, String, String)>,
    focus: BTreeMap<String, Value>,
    exit_screen: Option<&'static str>,
}

impl Drive {
    fn new(context: OnboardingContext, feeds: Vec<Feed>, persist_outcome: PersistOutcome) -> Self {
        let model = OnboardingModel::new(context);
        let mut drive = Self {
            model,
            recorder: Recorder::new(feeds, persist_outcome),
            edges: Vec::new(),
            focus: BTreeMap::new(),
            exit_screen: None,
        };
        drive.note_visit();
        drive
    }

    fn typing_finished(&mut self) {
        self.dispatch(ModelEvent::WelcomeTypingFinished);
    }

    /// A recorded pilot step: the edge the corpus lists for this press.
    fn press(&mut self, key: KeyPress) {
        let from = self.model.current_screen().name().to_owned();
        self.dispatch(ModelEvent::Key(key));
        let to = if self.model.outcome().is_some() {
            "<exit>".to_owned()
        } else {
            self.model.current_screen().name().to_owned()
        };
        self.edges.push((press_label(key), from, to));
        self.note_visit();
    }

    /// An unrecorded input: selection moves, typed characters, and presses
    /// the capture did not record as transitions.
    fn feed_key(&mut self, key: KeyPress) {
        self.dispatch(ModelEvent::Key(key));
        self.note_visit();
    }

    fn type_text(&mut self, text: &str) {
        for character in text.chars() {
            self.feed_key(KeyPress::Char(character));
        }
    }

    fn clear_domain(&mut self) {
        for _ in 0..self.model.domain_value().chars().count() {
            self.feed_key(KeyPress::Backspace);
        }
    }

    /// The capture's explicit wait on a worker that exits the app: the edge
    /// is recorded from the screen the exit fired on.
    fn wait(&mut self, label: &str) {
        let from = self
            .exit_screen
            .expect("a wait step follows a worker-driven exit");
        self.edges.push((
            format!("wait:{label}"),
            from.to_owned(),
            "<exit>".to_owned(),
        ));
    }

    fn probe(&mut self, key: &str, value: Value) {
        self.focus.insert(key.to_owned(), value);
    }

    fn note_visit(&mut self) {
        if self.model.outcome().is_some() {
            return;
        }
        let screen = self.model.current_screen().name().to_owned();
        self.focus.entry(screen).or_insert_with(|| {
            self.model.focus().map_or(
                Value::Null,
                |(id, widget)| json!({"id": id, "widget": widget}),
            )
        });
    }

    fn dispatch(&mut self, event: ModelEvent) {
        let effects = self.model.handle(event, &mut self.recorder);
        for effect in effects {
            self.apply(effect);
        }
    }

    fn apply(&mut self, effect: ModelEffect) {
        match effect {
            ModelEffect::StartSignIn { attempt } => {
                self.recorder.factory_calls += 1;
                let feed = self.recorder.feeds.pop_front().unwrap_or(Feed::Pending);
                self.dispatch(ModelEvent::SignInStarted {
                    attempt,
                    sign_in_url: ORACLE_SIGN_IN_URL.to_owned(),
                });
                self.dispatch(ModelEvent::SignInStatus {
                    attempt,
                    status: SignInStatus::OpeningBrowser,
                });
                self.dispatch(ModelEvent::SignInStatus {
                    attempt,
                    status: SignInStatus::WaitingForBrowserSignIn,
                });
                match feed {
                    Feed::Pending => {
                        self.recorder.open_attempt = true;
                    }
                    Feed::Success(api_key) => {
                        self.dispatch(ModelEvent::SignInStatus {
                            attempt,
                            status: SignInStatus::Exchanging,
                        });
                        self.dispatch(ModelEvent::SignInStatus {
                            attempt,
                            status: SignInStatus::Completed,
                        });
                        // The worker closes its service before the outcome is
                        // handled, as the reference worker's `finally` does.
                        self.recorder.service_closes += 1;
                        self.dispatch(ModelEvent::SignInCompleted {
                            attempt,
                            api_key: api_key.to_owned(),
                        });
                    }
                    Feed::Failure => {
                        self.recorder.service_closes += 1;
                        self.dispatch(ModelEvent::SignInFailed {
                            attempt,
                            code: Some(SignInErrorCode::PollFailed),
                            message: "scripted failure".to_owned(),
                        });
                    }
                }
            }
            ModelEffect::CancelSignIn => {
                if std::mem::take(&mut self.recorder.open_attempt) {
                    self.recorder.service_closes += 1;
                }
            }
            ModelEffect::ScheduleSuccessExit => {
                // The capture runs the app with a zero success delay, so the
                // completion lands within the same pilot step.
                self.dispatch(ModelEvent::SuccessDelayElapsed);
            }
            ModelEffect::ScheduleUrlHelp { .. } | ModelEffect::CopyUrl { .. } => {}
            ModelEffect::Exit(_) => {
                self.exit_screen = Some(self.model.current_screen().name());
            }
        }
    }
}

fn press_label(key: KeyPress) -> String {
    match key {
        KeyPress::Enter => "press:enter".to_owned(),
        KeyPress::Escape => "press:escape".to_owned(),
        KeyPress::CtrlC => "press:ctrl+c".to_owned(),
        KeyPress::Up => "press:up".to_owned(),
        KeyPress::Down => "press:down".to_owned(),
        KeyPress::Backspace => "press:backspace".to_owned(),
        KeyPress::Char(character) => format!("press:{character}"),
    }
}

// --------------------------------------------------------------------------
// Scripted providers
// --------------------------------------------------------------------------

fn provider(base_url: Option<&str>, api_base_url: &str) -> Table {
    let mut table = Table::new();
    table.insert("name".to_owned(), TomlValue::String("mistral".to_owned()));
    table.insert(
        "backend".to_owned(),
        TomlValue::String("mistral".to_owned()),
    );
    table.insert(
        "api_key_env_var".to_owned(),
        TomlValue::String("MISTRAL_API_KEY".to_owned()),
    );
    table.insert(
        "browser_auth_base_url".to_owned(),
        TomlValue::String(base_url.unwrap_or_default().to_owned()),
    );
    table.insert(
        "browser_auth_api_base_url".to_owned(),
        TomlValue::String(api_base_url.to_owned()),
    );
    table
}

fn context_for(provider: Table) -> OnboardingContext {
    OnboardingContext {
        provider,
        vibe_base_url: context::DEFAULT_VIBE_BASE_URL.to_owned(),
        theme: "auto".to_owned(),
    }
}

/// The browser-capable provider the capture scripts: the shipped defaults.
fn browser_context() -> OnboardingContext {
    context_for(provider(
        Some(DEFAULT_BROWSER_AUTH_BASE_URL),
        DEFAULT_BROWSER_AUTH_API_BASE_URL,
    ))
}

/// A provider whose base URL is empty cannot browser sign-in, which is how
/// the capture disables the four gated screens.
fn manual_context() -> OnboardingContext {
    context_for(provider(None, DEFAULT_BROWSER_AUTH_API_BASE_URL))
}

/// A provider already configured against a custom console.
fn custom_configured_context() -> OnboardingContext {
    context_for(provider(
        Some("https://console.internal.example"),
        "https://console.internal.example/api",
    ))
}

// --------------------------------------------------------------------------
// Family runners
// --------------------------------------------------------------------------

fn run_constants(constants: &Constants, report: &mut Report) {
    report.check(
        "constants",
        "signInSteps",
        "count",
        &u32::try_from(SIGN_IN_STEP_NAMES.len()).unwrap_or_default(),
        &constants.sign_in_steps,
    );
    report.check(
        "constants",
        "signInSteps",
        "names",
        &SIGN_IN_STEP_NAMES.map(str::to_owned).to_vec(),
        &constants.sign_in_step_names,
    );
    report.check(
        "constants",
        "delays",
        "successExit",
        &SUCCESS_EXIT_DELAY_SECONDS,
        &constants.success_exit_delay_seconds,
    );
    report.check(
        "constants",
        "delays",
        "urlHelp",
        &SIGN_IN_URL_HELP_DELAY_SECONDS,
        &constants.sign_in_url_help_delay_seconds,
    );
    report.check(
        "constants",
        "themePresentation",
        "visibleNeighbors",
        &u32::try_from(THEME_VISIBLE_NEIGHBORS).unwrap_or_default(),
        &constants.theme_visible_neighbors,
    );
    report.check(
        "constants",
        "themePresentation",
        "fadeClasses",
        &THEME_FADE_CLASSES.map(str::to_owned).to_vec(),
        &constants.theme_fade_classes,
    );
    report.check(
        "constants",
        "themePresentation",
        "gradientColors",
        &u32::try_from(GRADIENT_COLORS.len()).unwrap_or_default(),
        &constants.gradient_color_count,
    );
}

fn run_domain_validation(cases: &[DomainCase], report: &mut Report) {
    assert!(
        cases.iter().any(|case| case.valid) && cases.iter().any(|case| !case.valid),
        "the corpus must accept and reject at least one domain; regenerate it with \
         {CAPTURE_SCRIPT}"
    );
    assert!(
        cases
            .iter()
            .any(|case| case.private_cloud_warning == Some(true)),
        "the corpus exercises no private-cloud warning; regenerate it with {CAPTURE_SCRIPT}"
    );
    for case in cases {
        report.check(
            "domainValidation",
            &case.case,
            "valid",
            &case.valid,
            &is_valid_custom_domain(&case.input),
        );
        if !case.valid {
            continue;
        }
        let (base, api) = resolve_browser_auth_urls(&case.input);
        report.check(
            "domainValidation",
            &case.case,
            "derivedBaseUrl",
            &case.derived_base_url,
            &Some(base),
        );
        report.check(
            "domainValidation",
            &case.case,
            "derivedApiBaseUrl",
            &case.derived_api_base_url,
            &Some(api),
        );
        report.check(
            "domainValidation",
            &case.case,
            "privateCloudWarning",
            &case.private_cloud_warning,
            &Some(is_likely_mistral_private_cloud_domain(&case.input)),
        );
    }
}

// --------------------------------------------------------------------------
// screenProse
// --------------------------------------------------------------------------

/// The modules that draw or speak the onboarding flow. Their string literals
/// are this port's whole authored surface for these screens, and every one of
/// them is held unequal to the reference's.
const PROSE_SOURCES: [&str; 4] = [
    "crates/vibe-cli/src/tui/onboarding.rs",
    "crates/vibe-cli/src/tui/onboarding/context.rs",
    "crates/vibe-cli/src/tui/onboarding/model.rs",
    "crates/vibe-cli/src/tui/onboarding/render.rs",
];

/// Runs this port reproduces verbatim on purpose, each with why. `NOTICE`
/// forbids shipping authored prose; a product or organization name is an
/// identifier an operator matches on, exactly as the compaction envelope
/// reproduces its `vibe_warning` tag while writing its own sentences. Each
/// entry is stale-checked: a listed run that no longer collides fails the
/// replay, so the list can never grow a permanent exemption for real prose.
const REPRODUCED_NAMES: &[(&str, &str)] = &[
    (
        "Welcome to ",
        "the greeting fragment that introduces the product name, carrying no directive",
    ),
    ("Mistral Vibe", "the product's name"),
    (
        "Mistral AI",
        "the organization's name, the hosted console's label",
    ),
];

/// The floor this port's own prose has to clear, so a refactor that moved the
/// sentences elsewhere fails instead of reporting a clean but empty guard.
const MINIMUM_PORT_PROSE_RUNS: usize = 25;
/// The floor the captured reference side has to clear, for the same reason.
const MINIMUM_REFERENCE_PROSE_RUNS: usize = 50;

/// Whether a literal reads as an authored run rather than an identifier,
/// matching `_is_prose` in the capture script so both sides collect the same
/// kind of string.
fn is_prose(value: &str) -> bool {
    value.len() >= 8
        && value.contains(' ')
        && value.chars().any(|character| {
            character.is_uppercase() || matches!(character, ',' | '.' | '!' | '?' | '\'')
        })
}

/// Every string literal a Rust source carries, with comments skipped and the
/// escapes this repository actually writes decoded, so a recorded digest is
/// compared against the text the screen really draws.
fn string_literals(source: &str) -> Vec<String> {
    let mut literals = Vec::new();
    let mut characters = source.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '/' if characters.peek() == Some(&'/') => {
                for next in characters.by_ref() {
                    if next == '\n' {
                        break;
                    }
                }
            }
            '/' if characters.peek() == Some(&'*') => {
                let mut previous = '\0';
                for next in characters.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
            }
            // A character literal never carries prose, and its quote must not
            // be read as the start of a string.
            '\'' => {
                if characters.peek() == Some(&'\\') {
                    characters.next();
                    characters.next();
                }
                characters.next();
                characters.next();
            }
            '"' => {
                let mut value = String::new();
                while let Some(next) = characters.next() {
                    match next {
                        '"' => break,
                        '\\' => match characters.next() {
                            Some('n') => value.push('\n'),
                            Some('t') => value.push('\t'),
                            Some('r') => value.push('\r'),
                            Some('0') => value.push('\0'),
                            Some('u') => {
                                // `\u{...}`: the braces delimit the code point.
                                let mut code = String::new();
                                if characters.peek() == Some(&'{') {
                                    characters.next();
                                    for digit in characters.by_ref() {
                                        if digit == '}' {
                                            break;
                                        }
                                        code.push(digit);
                                    }
                                }
                                if let Some(decoded) =
                                    u32::from_str_radix(&code, 16).ok().and_then(char::from_u32)
                                {
                                    value.push(decoded);
                                }
                            }
                            // A trailing backslash continues the literal on the
                            // next line, and the compiler drops the newline
                            // with the indentation that follows it.
                            Some('\n') => {
                                while characters
                                    .peek()
                                    .is_some_and(|following| following.is_whitespace())
                                {
                                    characters.next();
                                }
                            }
                            Some(escaped) => value.push(escaped),
                            None => break,
                        },
                        _ => value.push(next),
                    }
                }
                literals.push(value);
            }
            _ => {}
        }
    }
    literals
}

/// This port's own onboarding sentences, gathered from the modules that draw
/// them.
fn port_prose_runs() -> BTreeMap<String, Vec<String>> {
    let root = repo_root();
    PROSE_SOURCES
        .iter()
        .map(|relative| {
            let path = root.join(relative);
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()));
            let runs = string_literals(&source)
                .into_iter()
                .filter(|literal| is_prose(literal))
                .collect::<Vec<_>>();
            ((*relative).to_owned(), runs)
        })
        .collect()
}

/// `NOTICE` forbids shipping the reference's screen text, so every sentence
/// this port draws is compared against every recorded reference run and must
/// differ from all of them. Equality would mean the prose was copied.
fn run_screen_prose(modules: &[ProseModule], report: &mut Report) {
    let recorded = modules
        .iter()
        .flat_map(|module| module.runs.iter())
        .collect::<Vec<_>>();
    assert!(
        recorded.len() >= MINIMUM_REFERENCE_PROSE_RUNS,
        "the corpus records {} reference runs, below the {MINIMUM_REFERENCE_PROSE_RUNS} floor; \
         regenerate it with {CAPTURE_SCRIPT}",
        recorded.len()
    );
    assert!(
        modules
            .iter()
            .any(|module| module.module.starts_with("screens/")),
        "the corpus records no screen module; regenerate it with {CAPTURE_SCRIPT}"
    );
    let port = port_prose_runs();
    let total = port.values().map(Vec::len).sum::<usize>();
    assert!(
        total >= MINIMUM_PORT_PROSE_RUNS,
        "this port's onboarding modules carry {total} authored runs, below the \
         {MINIMUM_PORT_PROSE_RUNS} floor; the guard would pass without measuring anything"
    );
    let digest_of = |run: &str| Digested {
        length: run.len(),
        digest: hex::encode(Sha256::digest(run.as_bytes())),
    };
    for (module, runs) in &port {
        for run in runs {
            if REPRODUCED_NAMES.iter().any(|(name, _)| name == run) {
                continue;
            }
            report.check(
                "screenProse",
                module,
                "original sentence",
                &true,
                &!recorded.contains(&&digest_of(run)),
            );
        }
    }
    // A reproduced name that stopped matching the reference is no longer a
    // reproduction, so its exemption is stale and has to go.
    let all_port_runs = port.values().flatten().collect::<Vec<_>>();
    for (name, reason) in REPRODUCED_NAMES {
        assert!(
            all_port_runs.iter().any(|run| run.as_str() == *name),
            "no onboarding module carries the reproduced name {name:?} ({reason}) any more"
        );
        assert!(
            recorded.contains(&&digest_of(name)),
            "{name:?} is exempted as a reproduced name ({reason}) but the reference no longer \
             carries it, so the exemption is stale"
        );
    }
}

/// The outcome a corpus terminating value names.
fn outcome_from_reference_value(value: Option<&str>) -> OnboardingOutcome {
    let Some(value) = value else {
        return OnboardingOutcome::Cancelled;
    };
    if value == "completed" {
        return OnboardingOutcome::Completed;
    }
    if let Some(detail) = value.strip_prefix("env_var_error:") {
        return OnboardingOutcome::EnvVarError {
            detail: detail.to_owned(),
        };
    }
    if let Some(detail) = value.strip_prefix("save_error:") {
        return OnboardingOutcome::SaveError {
            detail: detail.to_owned(),
        };
    }
    if let Some(detail) = value.strip_prefix("provider_config_error:") {
        return OnboardingOutcome::ProviderConfigError {
            detail: detail.to_owned(),
        };
    }
    panic!("the corpus records a terminating value outside the reference vocabulary: {value}");
}

fn run_terminating(cases: &[TerminatingCase], report: &mut Report) {
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
    let env_file = Path::new("/oracle/.vibe/.env");
    for case in cases {
        let outcome = outcome_from_reference_value(case.value.as_deref());
        // The vocabulary round-trips: the port's serialization of the parsed
        // value is the value the corpus recorded.
        report.check(
            "terminating",
            &case.case,
            "value",
            &case.value,
            &outcome.as_reference_value(),
        );
        let plan = exit_plan(&outcome, env_file);
        report.check(
            "terminating",
            &case.case,
            "exitCode",
            &case.exit_code,
            &plan.exit_code.map(i64::from),
        );
        report.check(
            "terminating",
            &case.case,
            "themePersisted",
            &case.theme_persisted,
            &plan.persist_theme,
        );
        if case.theme_persisted {
            assert_eq!(
                case.theme_writes.len(),
                1,
                "terminating/{}: the reference persists the theme exactly once",
                case.case
            );
            let pointer = case.theme_writes[0].get("pointer").and_then(Value::as_str);
            report.check(
                "terminating",
                &case.case,
                "themeWritePointer",
                &Some("/theme"),
                &pointer,
            );
        } else {
            report.check(
                "terminating",
                &case.case,
                "themeWrites",
                &0usize,
                &case.theme_writes.len(),
            );
        }
        // `NOTICE` holds this port's sentence permanently unequal to the
        // reference's: matching length and digest would mean the prose was
        // copied.
        let digest = hex::encode(Sha256::digest(plan.message.as_bytes()));
        for message in &case.messages {
            report.check(
                "terminating",
                &case.case,
                "messageDigestDiffers",
                &true,
                &(digest != message.digest),
            );
        }
        assert!(
            !case.messages.is_empty() || case.value.is_none() && case.exit_code == Some(0),
            "terminating/{}: every exit path but silent success prints a message",
            case.case
        );
    }
}

// --------------------------------------------------------------------------
// Screen-graph scripts
// --------------------------------------------------------------------------

/// Walks a drive from the welcome screen to the sign-in target screen.
fn onto_target(drive: &mut Drive) {
    drive.typing_finished();
    drive.press(KeyPress::Enter);
    drive.press(KeyPress::Enter);
    drive.press(KeyPress::Enter);
}

/// Walks a drive onto the custom-domain screen.
fn onto_custom_domain(drive: &mut Drive) {
    onto_target(drive);
    drive.feed_key(KeyPress::Down);
    drive.press(KeyPress::Enter);
}

fn drive_scenario(scenario: &GraphScenario) -> Drive {
    let case = scenario.case.as_str();
    let mut drive = match case {
        "graph-browser-provider-installs-seven-screens" => {
            let mut drive = Drive::new(browser_context(), Vec::new(), PersistOutcome::Completed);
            drive.press(KeyPress::Escape);
            drive
        }
        "graph-manual-provider-installs-three-screens" => {
            let mut drive = Drive::new(manual_context(), Vec::new(), PersistOutcome::Completed);
            drive.typing_finished();
            drive.press(KeyPress::Enter);
            drive.press(KeyPress::Enter);
            drive.press(KeyPress::Escape);
            drive
        }
        "welcome-advance-is-inert-until-typing-finishes" => {
            let mut drive = Drive::new(browser_context(), Vec::new(), PersistOutcome::Completed);
            drive.feed_key(KeyPress::Enter);
            let early = drive.model.current_screen().name().to_owned();
            drive.probe("welcome:screenAfterEarlyEnter", json!(early));
            drive.typing_finished();
            drive.press(KeyPress::Enter);
            let late = drive.model.current_screen().name().to_owned();
            drive.probe("welcome:screenAfterLateEnter", json!(late));
            drive.press(KeyPress::Escape);
            drive
        }
        "theme-selection-applies-live-and-leads-on" => {
            let mut drive = Drive::new(browser_context(), Vec::new(), PersistOutcome::Completed);
            drive.typing_finished();
            drive.press(KeyPress::Enter);
            drive.feed_key(KeyPress::Down);
            drive.press(KeyPress::Enter);
            drive.press(KeyPress::Escape);
            drive
        }
        "auth-method-browser-leads-to-target" => {
            let mut drive = Drive::new(browser_context(), Vec::new(), PersistOutcome::Completed);
            onto_target(&mut drive);
            drive.press(KeyPress::Escape);
            drive
        }
        "auth-method-manual-leads-to-api-key" => {
            let mut drive = Drive::new(browser_context(), Vec::new(), PersistOutcome::Completed);
            drive.typing_finished();
            drive.press(KeyPress::Enter);
            drive.press(KeyPress::Enter);
            drive.feed_key(KeyPress::Down);
            drive.press(KeyPress::Enter);
            drive.press(KeyPress::Escape);
            drive
        }
        "auth-method-selection-wraps" => {
            let mut drive = Drive::new(browser_context(), Vec::new(), PersistOutcome::Completed);
            drive.typing_finished();
            drive.press(KeyPress::Enter);
            drive.press(KeyPress::Enter);
            let mut walk = vec![drive.model.method_selection()];
            for _ in 0..3 {
                drive.feed_key(KeyPress::Down);
                walk.push(drive.model.method_selection());
            }
            drive.probe("auth_method:selectionWalk", json!(walk));
            drive.press(KeyPress::Escape);
            drive
        }
        "target-default-applies-mistral-and-signs-in" => {
            let mut drive = Drive::new(
                browser_context(),
                vec![Feed::Pending],
                PersistOutcome::Completed,
            );
            onto_target(&mut drive);
            drive.press(KeyPress::Enter);
            drive.press(KeyPress::Escape);
            drive
        }
        "target-other-leads-to-custom-domain" => {
            let mut drive = Drive::new(browser_context(), Vec::new(), PersistOutcome::Completed);
            onto_custom_domain(&mut drive);
            drive.press(KeyPress::Escape);
            drive
        }
        "target-back-returns-to-method" => {
            let mut drive = Drive::new(browser_context(), Vec::new(), PersistOutcome::Completed);
            onto_target(&mut drive);
            drive.press(KeyPress::Escape);
            drive.press(KeyPress::Escape);
            drive
        }
        "target-overwrite-arms-then-proceeds" => {
            let mut drive = Drive::new(
                custom_configured_context(),
                vec![Feed::Pending],
                PersistOutcome::Completed,
            );
            onto_target(&mut drive);
            drive.feed_key(KeyPress::Enter);
            let armed = drive.model.override_armed();
            drive.probe("sign_in_target:armedAfterFirstEnter", json!(armed));
            drive.press(KeyPress::Enter);
            drive.press(KeyPress::Escape);
            drive
        }
        "target-overwrite-disarms-on-move" => {
            let mut drive = Drive::new(
                custom_configured_context(),
                Vec::new(),
                PersistOutcome::Completed,
            );
            onto_target(&mut drive);
            drive.feed_key(KeyPress::Enter);
            let armed = drive.model.override_armed();
            drive.feed_key(KeyPress::Down);
            let after_move = drive.model.override_armed();
            drive.probe("sign_in_target:armedThenMoved", json!([armed, after_move]));
            drive.press(KeyPress::Escape);
            drive.press(KeyPress::Escape);
            drive
        }
        "custom-domain-validation-classes" => {
            let mut drive = Drive::new(browser_context(), Vec::new(), PersistOutcome::Completed);
            onto_custom_domain(&mut drive);
            let mut states = Vec::new();
            for input in [
                "console.internal.example",
                "console.oracle.mistral.ai",
                "http:/broken",
            ] {
                drive.clear_domain();
                drive.type_text(input);
                let feedback = drive
                    .model
                    .domain_feedback()
                    .expect("typing reveals the validation feedback");
                states.push(json!({
                    "boxClasses": [feedback.box_class()],
                    "expected": feedback.box_class(),
                    "feedbackClasses": [feedback.feedback_class()],
                    "input": input,
                }));
            }
            drive.probe("custom_domain:validationStates", json!(states));
            drive.press(KeyPress::Escape);
            drive.press(KeyPress::Escape);
            drive.press(KeyPress::Escape);
            drive
        }
        "custom-domain-submit-derives-urls-and-signs-in" => {
            let mut drive = Drive::new(
                browser_context(),
                vec![Feed::Pending],
                PersistOutcome::Completed,
            );
            onto_custom_domain(&mut drive);
            drive.type_text("console.internal.example");
            drive.press(KeyPress::Enter);
            drive.press(KeyPress::Escape);
            drive
        }
        "custom-domain-sign-in-upserts-the-provider" => {
            let mut drive = Drive::new(
                browser_context(),
                vec![Feed::Success("oracle-signed-in-key")],
                PersistOutcome::Completed,
            );
            onto_custom_domain(&mut drive);
            drive.type_text("console.internal.example");
            drive.press(KeyPress::Enter);
            drive.wait("the sign-in worker to exit the app");
            drive
        }
        "custom-domain-invalid-submission-stays" => {
            let mut drive = Drive::new(browser_context(), Vec::new(), PersistOutcome::Completed);
            onto_custom_domain(&mut drive);
            drive.type_text("http:/broken");
            drive.feed_key(KeyPress::Enter);
            let still = drive.model.current_screen().name().to_owned();
            drive.probe("custom_domain:stillOnScreen", json!(still));
            drive.clear_domain();
            drive.press(KeyPress::Escape);
            drive.press(KeyPress::Escape);
            drive.press(KeyPress::Escape);
            drive
        }
        "custom-domain-back-returns-to-target" => {
            let mut drive = Drive::new(browser_context(), Vec::new(), PersistOutcome::Completed);
            onto_custom_domain(&mut drive);
            drive.press(KeyPress::Escape);
            drive.press(KeyPress::Escape);
            drive.press(KeyPress::Escape);
            drive
        }
        "browser-sign-in-success-exits-completed" => {
            let mut drive = Drive::new(
                browser_context(),
                vec![Feed::Success("oracle-signed-in-key")],
                PersistOutcome::Completed,
            );
            onto_target(&mut drive);
            drive.press(KeyPress::Enter);
            drive.wait("the sign-in worker to exit the app");
            drive
        }
        "browser-sign-in-error-then-retry-succeeds" => {
            let mut drive = Drive::new(
                browser_context(),
                vec![Feed::Failure, Feed::Success("oracle-signed-in-key")],
                PersistOutcome::Completed,
            );
            onto_target(&mut drive);
            drive.press(KeyPress::Enter);
            let variant = drive.model.sign_in_view().variant.as_str();
            drive.probe("browser_sign_in:errorVariant", json!(variant));
            drive.press(KeyPress::Char('r'));
            drive.wait("the retried worker to exit the app");
            drive
        }
        "browser-sign-in-retry-refused-while-running" => {
            let mut drive = Drive::new(
                browser_context(),
                vec![Feed::Pending],
                PersistOutcome::Completed,
            );
            onto_target(&mut drive);
            drive.press(KeyPress::Enter);
            drive.feed_key(KeyPress::Char('r'));
            drive.press(KeyPress::Escape);
            drive
        }
        "browser-sign-in-manual-fallback" => {
            let mut drive = Drive::new(
                browser_context(),
                vec![Feed::Pending],
                PersistOutcome::Completed,
            );
            onto_target(&mut drive);
            drive.press(KeyPress::Enter);
            drive.press(KeyPress::Char('m'));
            drive.press(KeyPress::Escape);
            drive
        }
        "browser-sign-in-persist-failure-exits-immediately" => {
            let mut drive = Drive::new(
                browser_context(),
                vec![Feed::Success("oracle-signed-in-key")],
                PersistOutcome::SaveError {
                    detail: "scripted-detail".to_owned(),
                },
            );
            onto_target(&mut drive);
            drive.press(KeyPress::Enter);
            drive.wait("the persist failure to exit the app");
            drive
        }
        "browser-sign-in-cancel-closes-the-service" => {
            let mut drive = Drive::new(
                browser_context(),
                vec![Feed::Pending],
                PersistOutcome::Completed,
            );
            onto_target(&mut drive);
            drive.press(KeyPress::Enter);
            drive.press(KeyPress::Escape);
            drive
        }
        "api-key-submit-terminates-with-the-persist-outcome" => {
            let mut drive = Drive::new(manual_context(), Vec::new(), PersistOutcome::Completed);
            drive.typing_finished();
            drive.press(KeyPress::Enter);
            drive.press(KeyPress::Enter);
            drive.probe("api_key:masked", json!(drive.model.key_masked()));
            drive.type_text("oracle-key");
            drive.press(KeyPress::Enter);
            drive
        }
        "api-key-empty-submission-stays" => {
            let mut drive = Drive::new(manual_context(), Vec::new(), PersistOutcome::Completed);
            drive.typing_finished();
            drive.press(KeyPress::Enter);
            drive.press(KeyPress::Enter);
            drive.feed_key(KeyPress::Enter);
            let still = drive.model.current_screen().name().to_owned();
            drive.probe("api_key:stillOnScreen", json!(still));
            drive.press(KeyPress::Escape);
            drive
        }
        "cancel-from-theme-exits-with-nothing" => {
            let mut drive = Drive::new(browser_context(), Vec::new(), PersistOutcome::Completed);
            drive.typing_finished();
            drive.press(KeyPress::Enter);
            drive.press(KeyPress::Escape);
            drive
        }
        other => panic!("no drive is scripted for corpus scenario {other}"),
    };
    drive.note_visit();
    drive
}

fn run_screen_graph(cases: &[GraphScenario], report: &mut Report) {
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
    assert!(
        cases
            .iter()
            .any(|scenario| !scenario.supports_browser_sign_in),
        "the corpus drives no provider without browser sign-in"
    );

    for scenario in cases {
        let case = scenario.case.as_str();
        let drive = drive_scenario(scenario);
        report.check(
            "screenGraph",
            case,
            "supportsBrowserSignIn",
            &scenario.supports_browser_sign_in,
            &drive.model.supports_browser_sign_in(),
        );
        report.check(
            "screenGraph",
            case,
            "installedScreens",
            &scenario.installed_screens,
            &drive
                .model
                .installed_screens()
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        );
        report.check(
            "screenGraph",
            case,
            "entryScreen",
            &scenario.entry_screen.as_str(),
            &"welcome",
        );
        let expected_edges = scenario
            .transitions
            .iter()
            .map(|edge| (edge.event.clone(), edge.from.clone(), edge.to.clone()))
            .collect::<Vec<_>>();
        report.check(
            "screenGraph",
            case,
            "transitions",
            &expected_edges,
            &drive.edges,
        );
        report.check("screenGraph", case, "focus", &scenario.focus, &drive.focus);
        report.check(
            "screenGraph",
            case,
            "result",
            &scenario.result,
            &drive
                .model
                .outcome()
                .and_then(OnboardingOutcome::as_reference_value),
        );
        report.check(
            "screenGraph",
            case,
            "selectedTheme",
            &scenario.selected_theme.as_str(),
            &drive.model.selected_theme(),
        );
        let (base, api) = drive.model.provider_browser_auth();
        report.check(
            "screenGraph",
            case,
            "providerBrowserAuth",
            &scenario.provider_browser_auth,
            &ProviderBrowserAuth {
                api_base_url: api.map(str::to_owned),
                base_url: base.map(str::to_owned),
            },
        );
        report.check(
            "screenGraph",
            case,
            "factoryCalls",
            &scenario.effects.factory_calls.unwrap_or(0),
            &drive.recorder.factory_calls,
        );
        report.check(
            "screenGraph",
            case,
            "serviceCloses",
            &scenario.effects.service_closes,
            &drive.recorder.service_closes,
        );
        report.check(
            "screenGraph",
            case,
            "persistCalls",
            &scenario.effects.persist_calls,
            &drive.recorder.persist_calls,
        );
        report.check(
            "screenGraph",
            case,
            "providerWrites",
            &scenario.effects.provider_writes,
            &drive.recorder.provider_writes,
        );
        if let Some(themes) = &scenario.themes {
            report.check(
                "screenGraph",
                case,
                "themes",
                themes,
                &sorted_theme_names()
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
            );
        }
    }
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

#[test]
fn the_committed_corpus_replays_against_this_port() {
    let corpus = corpus();
    println!(
        "onboarding: divergence ledger ({} entries)",
        DIVERGENCES.len()
    );
    for (case, reason) in DIVERGENCES {
        println!("  {case}: {reason}");
    }
    let mut scenarios = 0;
    let mut report = Report::default();
    run_constants(&corpus.constants, &mut report);
    scenarios += settle(&report, "constants");
    let mut report = Report::default();
    run_screen_graph(&corpus.screen_graph, &mut report);
    scenarios += settle(&report, "screenGraph");
    let mut report = Report::default();
    run_terminating(&corpus.terminating, &mut report);
    scenarios += settle(&report, "terminating");
    let mut report = Report::default();
    run_domain_validation(&corpus.domain_validation, &mut report);
    scenarios += settle(&report, "domainValidation");
    let mut report = Report::default();
    run_screen_prose(&corpus.screen_prose, &mut report);
    scenarios += settle(&report, "screenProse");
    println!(
        "onboarding: {scenarios} comparisons across 4 families plus the constants block \
         replayed at {}",
        &corpus.reference.commit[..12],
    );
    assert!(
        scenarios >= MINIMUM_SCENARIOS,
        "the corpus replays {scenarios} comparisons, below the {MINIMUM_SCENARIOS} floor; \
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
