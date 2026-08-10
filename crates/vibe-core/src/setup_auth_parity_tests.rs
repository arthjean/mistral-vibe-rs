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
//! Every family now has a live comparator: the auth state and persistence
//! replay against `vibe_core::auth`, the sign-in service and gateway against
//! `auth::sign_in` and `auth::sign_in_http` over the same scripted stubs the
//! capture used, and the URL verdicts against `validate_url_against_base`.
//! The error taxonomy is compared for structural equality and its sentences
//! for permanent inequality: this port's prose failing to differ from a
//! reference digest is itself a failure. The `constants` block reads the two
//! browser-auth defaults and the default key variable out of the
//! configuration registry through `default_document`, which is exactly the
//! surface `config/fields/read` serves to clients.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::auth::sign_in::{SignInEvent, SignInService};
use crate::auth::testing::{
    ScriptedBackend, ScriptedError, ScriptedHttpClient, ScriptedOpener, ScriptedSignInGateway,
    ScriptedSignInRuntime, scripted,
};
use crate::auth::{
    self, HttpSignInGateway, KeyringStore, PersistOutcome, RemoveError, SignInErrorCode,
    SignInGateway as _, SignInStatus, validate_url_against_base,
};
use crate::config::DotenvValues;
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
/// A `family/*` entry covers every case of its family and goes stale only
/// when the whole family conforms. The ledger is empty since EP-054 landed
/// the sign-in flow: every family answers, and the only divergences this
/// subtree keeps are the `NOTICE`-mandated prose inequalities, which are
/// asserted directly rather than ledgered.
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
    env_key_kind: String,
    had_value_before_dotenv_load: bool,
    process_env: Option<String>,
    keyring: Option<String>,
    dotenv: String,
    /// The six-state verdict, absent when the reference raised instead.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    can_use_active_provider: Option<bool>,
    #[serde(default)]
    sign_out_available: Option<bool>,
    #[serde(default)]
    reported_env_key: Option<String>,
    #[serde(default)]
    raised: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistenceCase {
    case: String,
    op: String,
    env_key: String,
    #[serde(default)]
    custom_domain: Option<bool>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    outcome_detail_present: Option<bool>,
    #[serde(default)]
    process_env_value: Option<String>,
    #[serde(default)]
    dotenv_value: Option<String>,
    #[serde(default)]
    keyring_stored: Option<BTreeMap<String, String>>,
    #[serde(default)]
    keyring_calls: Option<Vec<String>>,
    #[serde(default)]
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
    op: Option<String>,
    #[serde(default)]
    browser_base: Option<String>,
    #[serde(default)]
    api_base: Option<String>,
    script: Value,
    #[serde(default)]
    events: Option<Vec<Value>>,
    #[serde(default)]
    gateway_calls: Option<Vec<String>>,
    #[serde(default)]
    poll_count: Option<u32>,
    #[serde(default)]
    sleeps: Option<Vec<f64>>,
    #[serde(default)]
    browser_opened: Option<Vec<String>>,
    #[serde(default)]
    challenge: Option<String>,
    #[serde(default)]
    requests: Option<Vec<Value>>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    process_id: Option<String>,
    #[serde(default)]
    sign_in_url: Option<String>,
    #[serde(default)]
    poll_url: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    exchange_token: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UrlValidationCase {
    case: String,
    value: String,
    base: String,
    verdict: String,
    #[serde(default)]
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
    report.check(
        "constants",
        "keyringService",
        "service name",
        &corpus.keyring_service,
        &auth::KEYRING_SERVICE.to_owned(),
    );
    report.check(
        "constants",
        "legacyKeyringServices",
        "reference legacy read set",
        &corpus.legacy_keyring_services,
        &auth::LEGACY_KEYRING_SERVICES
            .iter()
            .map(|service| (*service).to_owned())
            .collect::<Vec<_>>(),
    );
    report.check(
        "constants",
        "defaultEnvKeyConstant",
        "auth module default",
        &corpus.default_env_key,
        &auth::DEFAULT_MISTRAL_API_ENV_KEY.to_owned(),
    );
    report.check(
        "constants",
        "pollIntervalSeconds",
        "poll cadence",
        &corpus.poll_interval_seconds,
        &auth::POLL_INTERVAL_SECONDS,
    );
    report.check(
        "constants",
        "maxConsecutivePollFailures",
        "failure tolerance",
        &corpus.max_consecutive_poll_failures,
        &auth::MAX_CONSECUTIVE_POLL_FAILURES,
    );
    report.check(
        "constants",
        "statuses",
        "status vocabulary",
        &corpus.statuses,
        &SignInStatus::ALL
            .iter()
            .map(|status| status.as_str().to_owned())
            .collect::<Vec<_>>(),
    );
    report.check(
        "constants",
        "httpGoneStatus",
        "expiry status",
        &corpus.http_gone_status,
        &auth::sign_in_http::HTTP_GONE,
    );
    report.check(
        "constants",
        "defaultPorts",
        "default port table",
        &corpus.default_ports,
        &auth::sign_in_http::DEFAULT_PORTS
            .iter()
            .map(|(scheme, port)| ((*scheme).to_owned(), *port))
            .collect::<BTreeMap<_, _>>(),
    );
    report.check(
        "constants",
        "codeChallengeMethod",
        "challenge method",
        &corpus.code_challenge_method,
        &auth::CODE_CHALLENGE_METHOD.to_owned(),
    );
    report.check(
        "constants",
        "signInPath",
        "creation path",
        &corpus.sign_in_path,
        &auth::sign_in_http::SIGN_IN_PATH.to_owned(),
    );
    report.check(
        "constants",
        "exchangePathTemplate",
        "exchange path",
        &corpus.exchange_path_template,
        &auth::sign_in_http::EXCHANGE_PATH_TEMPLATE.to_owned(),
    );
    report.check(
        "constants",
        "pkce/scriptedChallenge",
        "challenge derivation",
        &corpus.pkce.scripted_challenge,
        &auth::code_challenge(&corpus.pkce.scripted_verifier),
    );
    let generated = auth::generate_code_verifier().unwrap_or_default();
    report.check(
        "constants",
        "pkce/generatedLength",
        "verifier length",
        &corpus.pkce.generated_length,
        &generated.len(),
    );
    report.check(
        "constants",
        "pkce/generatedCharsetIsUnreserved",
        "verifier charset",
        &corpus.pkce.generated_charset_is_unreserved,
        &generated
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')),
    );
    settle(&report, "constants")
}

/// The custom key variable the capture uses for the unsupported-provider rows.
const CUSTOM_ENV_KEY: &str = "ORACLE_CUSTOM_KEY";

fn case_env_key(kind: &str, case: &str) -> &'static str {
    match kind {
        "default" => auth::DEFAULT_MISTRAL_API_ENV_KEY,
        "custom" => CUSTOM_ENV_KEY,
        "empty" => "",
        other => panic!("authState/{case} records unknown envKeyKind {other}"),
    }
}

/// Builds the scenario's dotenv path inside `root`, mirroring the capture's
/// `absent`, `value`, `empty`, `directory` and `unreadable` states.
fn stage_dotenv(root: &Path, state: &str, env_key: &str, case: &str) -> PathBuf {
    let env_path = root.join("global.env");
    match state {
        "absent" => {}
        "value" => fs::write(&env_path, format!("{env_key}=oracle-dotenv-value\n"))
            .expect("the dotenv fixture writes"),
        "empty" => {
            fs::write(&env_path, format!("{env_key}=\n")).expect("the dotenv fixture writes");
        }
        "directory" => fs::create_dir(&env_path).expect("the dotenv directory fixture creates"),
        "unreadable" => {
            fs::write(&env_path, format!("{env_key}=oracle-dotenv-value\n"))
                .expect("the dotenv fixture writes");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&env_path, fs::Permissions::from_mode(0o000))
                    .expect("the dotenv fixture chmods");
            }
        }
        other => panic!("authState/{case} records unknown dotenv state {other}"),
    }
    env_path
}

fn observed_auth_state(case: &AuthStateCase) -> String {
    let env_key = case_env_key(&case.env_key_kind, &case.case);
    let temporary = tempfile::tempdir().expect("a scenario scratch directory");
    let env_path = stage_dotenv(temporary.path(), &case.dotenv, env_key, &case.case);
    let mut environ = BTreeMap::new();
    if let Some(value) = &case.process_env {
        environ.insert(env_key.to_owned(), value.clone());
    }
    let seeded: Vec<(&str, &str)> = case
        .keyring
        .as_deref()
        .map(|value| vec![(auth::KEYRING_SERVICE, value)])
        .unwrap_or_default();
    let store = KeyringStore::new(Box::new(scripted(&seeded, None, None, &[])));
    let outcome = auth::assess_auth_state(
        env_key,
        &env_path,
        &environ,
        case.had_value_before_dotenv_load,
        &store,
    );
    match outcome {
        Ok(state) => format!(
            "{} canUse={} signOut={} envKey={}",
            state.kind.as_str(),
            state.can_use_active_provider,
            state.sign_out_available,
            state.env_key.as_deref().unwrap_or("<none>"),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            "raised PermissionError".to_owned()
        }
        Err(error) => format!("raised {:?}", error.kind()),
    }
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
        if cfg!(not(unix)) && case.dotenv == "unreadable" {
            println!(
                "setup-auth: authState/{} skipped: an unreadable file needs unix permissions",
                case.case
            );
            continue;
        }
        let expected = match (&case.kind, &case.raised) {
            (Some(kind), _) => format!(
                "{kind} canUse={} signOut={} envKey={}",
                case.can_use_active_provider.unwrap_or_default(),
                case.sign_out_available.unwrap_or_default(),
                case.reported_env_key.as_deref().unwrap_or("<none>"),
            ),
            (None, Some(raised)) => format!("raised {raised}"),
            (None, None) => panic!(
                "authState/{} records neither a verdict nor a raise",
                case.case
            ),
        };
        let observed = observed_auth_state(case);
        report.check("authState", &case.case, "assessment", &expected, &observed);
    }
    settle(&report, "authState")
}

/// Drops the calls that reach the prior-build service `mistral-vibe-rs`: the
/// reference does not know that name, so the corpus cannot record them, and
/// consulting it is this port's own deliberate compatibility read (US-184).
fn without_prior_build_calls(calls: Vec<String>) -> Vec<String> {
    calls
        .into_iter()
        .filter(|call| call.split(':').nth(1) != Some(auth::PRIOR_BUILD_KEYRING_SERVICE))
        .collect()
}

/// Makes the dotenv at `path` readable but not replaceable.
fn lock_dotenv(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let parent = path.parent().expect("the dotenv sits in a directory");
        fs::set_permissions(parent, fs::Permissions::from_mode(0o500))
            .expect("the dotenv parent chmods");
    }
    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(path)
            .expect("the seeded dotenv exists")
            .permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions).expect("the dotenv chmods");
    }
}

/// Undoes [`lock_dotenv`], so the scratch directory can still be cleaned up.
fn unlock_dotenv(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let parent = path.parent().expect("the dotenv sits in a directory");
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .expect("the dotenv parent chmods back");
    }
    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(path)
            .expect("the seeded dotenv exists")
            .permissions();
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions).expect("the dotenv chmods back");
    }
}

fn read_dotenv_value(path: &Path, env_key: &str) -> Option<String> {
    if env_key.is_empty() {
        return None;
    }
    DotenvValues::load(path)
        .file_variable(env_key)
        .map(str::to_owned)
}

fn corpus_telemetry_flags(case: &PersistenceCase) -> Vec<bool> {
    case.telemetry
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|event| {
            event
                .get("customDomain")
                .and_then(Value::as_bool)
                .unwrap_or_default()
        })
        .collect()
}

fn persist_line(
    outcome: &str,
    detail_present: bool,
    process_env: Option<&str>,
    dotenv: Option<&str>,
    stored: &BTreeMap<String, String>,
    calls: &[String],
    telemetry: &[bool],
) -> String {
    format!(
        "outcome={outcome} detailPresent={detail_present} processEnv={process_env:?} \
         dotenv={dotenv:?} stored={stored:?} calls={calls:?} telemetry={telemetry:?}"
    )
}

fn replay_persist_case(case: &PersistenceCase) -> (String, String) {
    let (set_error, seed_stale, break_parent, break_unset) = match case.case.as_str() {
        "persist-keyring-success-removes-stale-dotenv" => (None, true, false, false),
        "persist-keyring-success-custom-domain-telemetry" => (None, false, false, false),
        "persist-keyring-failure-falls-back-to-dotenv" => {
            (Some(ScriptedError::Backend), false, false, false)
        }
        "persist-no-backend-falls-back-to-dotenv" => {
            (Some(ScriptedError::NoBackend), false, false, false)
        }
        "persist-empty-env-var" => (None, false, false, false),
        "persist-keyring-and-dotenv-both-fail" => {
            (Some(ScriptedError::Backend), false, true, false)
        }
        "persist-stale-dotenv-removal-failure-still-completes" => (None, true, false, true),
        other => panic!("persistence/{other} has no scripted replay; add it beside the capture"),
    };
    let temporary = tempfile::tempdir().expect("a scenario scratch directory");
    let env_file = if break_parent {
        // The env-file parent is a *file*, so the fallback mkdir fails.
        let blocked = temporary.path().join("not-a-directory");
        fs::write(&blocked, "occupied").expect("the blocking file writes");
        blocked.join("nested").join(".env")
    } else {
        temporary.path().join(".env")
    };
    if seed_stale {
        fs::write(&env_file, format!("{}=stale-dotenv-copy\n", case.env_key))
            .expect("the stale dotenv seeds");
    }
    // The capture injects the removal failure by raising `OSError` out of
    // `unset_key`, so the replay has to reach the same failure without
    // depending on how this port happens to rewrite the file: a location whose
    // contents can be read but not replaced. On unix that is a parent the
    // staging file cannot be created in, and elsewhere a destination the move
    // is refused over.
    if break_unset {
        lock_dotenv(&env_file);
    }
    let backend = scripted(&[], set_error, None, &[]);
    let store = KeyringStore::new(Box::new(backend.clone()));
    let mut process_env: BTreeMap<String, String> = BTreeMap::new();
    let report = auth::persist_api_key(
        &case.env_key,
        true,
        "oracle-api-key",
        case.custom_domain.unwrap_or_default(),
        &mut process_env,
        &env_file,
        &store,
    );
    if break_unset {
        unlock_dotenv(&env_file);
    }
    let full = report.outcome.as_reference_string();
    let detail_present = full
        .split_once(':')
        .is_some_and(|(_, detail)| !detail.is_empty());
    let observed_outcome = if matches!(report.outcome, PersistOutcome::SaveError { .. }) {
        // The save_error detail is an OS-authored sentence and is not
        // deterministic across machines, so only its kind commits.
        "save_error".to_owned()
    } else {
        full.clone()
    };
    let observed = persist_line(
        &observed_outcome,
        detail_present,
        process_env.get(&case.env_key).map(String::as_str),
        read_dotenv_value(&env_file, &case.env_key).as_deref(),
        &backend.stored(),
        &without_prior_build_calls(backend.calls()),
        &report
            .telemetry
            .iter()
            .map(|event| event.custom_domain)
            .collect::<Vec<_>>(),
    );
    let expected = persist_line(
        case.outcome.as_deref().unwrap_or("<unrecorded>"),
        case.outcome_detail_present.unwrap_or_default(),
        case.process_env_value.as_deref(),
        case.dotenv_value.as_deref(),
        case.keyring_stored.as_ref().unwrap_or(&BTreeMap::new()),
        case.keyring_calls.as_deref().unwrap_or_default(),
        &corpus_telemetry_flags(case),
    );
    (expected, observed)
}

fn remove_line(
    raised: Option<&str>,
    process_env: Option<&str>,
    dotenv: Option<&str>,
    stored: &BTreeMap<String, String>,
    calls: &[String],
) -> String {
    format!(
        "raised={raised:?} processEnv={process_env:?} dotenv={dotenv:?} stored={stored:?} \
         calls={calls:?}"
    )
}

fn replay_remove_case(case: &PersistenceCase) -> (String, String) {
    let backend = match case.case.as_str() {
        "remove-clears-both-services-dotenv-and-environ" => scripted(
            &[
                (auth::KEYRING_SERVICE, "current-value"),
                ("vibe", "legacy-value"),
            ],
            None,
            None,
            &[],
        ),
        "remove-with-nothing-stored-is-a-no-op" => scripted(&[], None, None, &[]),
        "remove-with-no-backend-still-clears-the-rest" => scripted(
            &[],
            None,
            None,
            &[
                (auth::KEYRING_SERVICE, ScriptedError::NoBackend),
                ("vibe", ScriptedError::NoBackend),
            ],
        ),
        "remove-with-a-real-backend-error-clears-then-raises" => scripted(
            &[("vibe", "legacy-value")],
            None,
            None,
            &[(auth::KEYRING_SERVICE, ScriptedError::Backend)],
        ),
        "remove-empty-env-var-is-refused" => scripted(&[], None, None, &[]),
        other => panic!("persistence/{other} has no scripted replay; add it beside the capture"),
    };
    let temporary = tempfile::tempdir().expect("a scenario scratch directory");
    let env_file = temporary.path().join(".env");
    let mut process_env: BTreeMap<String, String> = BTreeMap::new();
    if !case.env_key.is_empty() {
        process_env.insert(case.env_key.clone(), "process-copy".to_owned());
        fs::write(&env_file, format!("{}=dotenv-copy\n", case.env_key))
            .expect("the dotenv copy seeds");
    }
    let store = KeyringStore::new(Box::new(backend.clone()));
    let raised = match auth::remove_api_key(&case.env_key, &mut process_env, &env_file, &store) {
        Ok(()) => None,
        Err(RemoveError::EmptyEnvKey) => Some("ValueError"),
        Err(RemoveError::Keyring(_)) => Some("KeyringError"),
        Err(RemoveError::EnvFile(_)) => Some("OSError"),
    };
    let observed = remove_line(
        raised,
        process_env.get(&case.env_key).map(String::as_str),
        read_dotenv_value(&env_file, &case.env_key).as_deref(),
        &backend.stored(),
        &without_prior_build_calls(backend.calls()),
    );
    let expected = remove_line(
        case.raised.as_deref(),
        case.process_env_value.as_deref(),
        case.dotenv_value.as_deref(),
        case.keyring_stored.as_ref().unwrap_or(&BTreeMap::new()),
        case.keyring_calls.as_deref().unwrap_or_default(),
    );
    (expected, observed)
}

fn read_line(value: Option<&str>, stored: &BTreeMap<String, String>, calls: &[String]) -> String {
    format!("value={value:?} stored={stored:?} calls={calls:?}")
}

fn replay_keyring_read_case(case: &PersistenceCase) -> (String, String) {
    let (backend, disabled, read_twice): (std::sync::Arc<ScriptedBackend>, bool, bool) =
        match case.case.as_str() {
            "read-resolves-from-the-current-service-first" => (
                scripted(
                    &[
                        (auth::KEYRING_SERVICE, "current-value"),
                        ("vibe", "legacy-value"),
                    ],
                    None,
                    None,
                    &[],
                ),
                false,
                false,
            ),
            "read-falls-back-to-legacy-and-migrates" => (
                scripted(&[("vibe", "legacy-value")], None, None, &[]),
                false,
                false,
            ),
            "read-migration-write-failure-still-returns-the-key" => (
                scripted(
                    &[("vibe", "legacy-value")],
                    Some(ScriptedError::Backend),
                    None,
                    &[],
                ),
                false,
                false,
            ),
            "read-migration-delete-failure-still-returns-the-key" => (
                scripted(
                    &[("vibe", "legacy-value")],
                    None,
                    None,
                    &[("vibe", ScriptedError::Backend)],
                ),
                false,
                false,
            ),
            "read-nothing-stored-anywhere" => (scripted(&[], None, None, &[]), false, false),
            "read-backend-error-reads-as-absent" => (
                scripted(&[], None, Some(ScriptedError::Backend), &[]),
                false,
                false,
            ),
            "read-empty-env-key-consults-nothing" => (
                scripted(&[(auth::KEYRING_SERVICE, "current-value")], None, None, &[]),
                false,
                false,
            ),
            "read-disabled-consults-nothing" => (
                scripted(&[(auth::KEYRING_SERVICE, "current-value")], None, None, &[]),
                true,
                false,
            ),
            "read-second-lookup-is-served-from-the-cache" => (
                scripted(&[(auth::KEYRING_SERVICE, "current-value")], None, None, &[]),
                false,
                true,
            ),
            other => {
                panic!("persistence/{other} has no scripted replay; add it beside the capture")
            }
        };
    let store = if disabled {
        KeyringStore::disabled(Box::new(backend.clone()))
    } else {
        KeyringStore::new(Box::new(backend.clone()))
    };
    let mut value = store.get_api_key(&case.env_key);
    if read_twice {
        value = store.get_api_key(&case.env_key);
    }
    let observed = read_line(
        value.as_deref(),
        &backend.stored(),
        &without_prior_build_calls(backend.calls()),
    );
    let expected = read_line(
        case.value.as_deref(),
        case.keyring_stored.as_ref().unwrap_or(&BTreeMap::new()),
        case.keyring_calls.as_deref().unwrap_or_default(),
    );
    (expected, observed)
}

fn run_persistence(cases: &[PersistenceCase]) -> usize {
    let mut report = Report::default();
    for case in cases {
        let (expected, observed) = match case.op.as_str() {
            "persist" => replay_persist_case(case),
            "remove" => replay_remove_case(case),
            "keyringRead" => replay_keyring_read_case(case),
            other => panic!("persistence/{} records unknown op {other}", case.case),
        };
        report.check("persistence", &case.case, "effects", &expected, &observed);
    }
    settle(&report, "persistence")
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds")
        .block_on(future)
}

fn corpus_event_line(event: &Value) -> String {
    let field = |name: &str| {
        event
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or("<unrecorded>")
    };
    match field("event") {
        "attemptStarted" => format!(
            "attemptStarted {} {}",
            field("signInUrl"),
            field("expiresAt")
        ),
        "status" => format!("status {}", field("status")),
        other => panic!("the corpus records unknown event kind {other}"),
    }
}

fn observed_event_line(event: &SignInEvent) -> String {
    match event {
        SignInEvent::AttemptStarted {
            sign_in_url,
            expires_at,
        } => format!("attemptStarted {sign_in_url} {}", expires_at.to_iso8601()),
        SignInEvent::StatusChanged(status) => format!("status {}", status.as_str()),
    }
}

fn service_line(
    events: &[String],
    calls: &[String],
    poll_count: usize,
    sleeps: &[f64],
    opened: &[String],
    challenge: Option<&str>,
    outcome: &str,
) -> String {
    format!(
        "events={events:?} calls={calls:?} pollCount={poll_count} sleeps={sleeps:?} \
         opened={opened:?} challenge={challenge:?} outcome={outcome}"
    )
}

/// Drives the port's `SignInService` over the corpus scenario's scripted
/// gateway, clock, opener and verifier, mirroring the capture's stubs.
fn replay_service_case(case: &ProtocolCase, scripted_verifier: &str) -> (String, String) {
    let script = &case.script;
    let gateway = ScriptedSignInGateway::new(
        script
            .get("createError")
            .and_then(Value::as_str)
            .map(str::to_owned),
        script
            .get("expiresIn")
            .and_then(Value::as_f64)
            .unwrap_or(600.0),
        script
            .get("polls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        script.get("exchange").cloned().unwrap_or(Value::Null),
    );
    let opener = ScriptedOpener::from_kind(
        script
            .get("opener")
            .and_then(Value::as_str)
            .unwrap_or("accept"),
    );
    let runtime = ScriptedSignInRuntime::new(opener, scripted_verifier);
    let mut service = SignInService::new(gateway, runtime);
    let mut events: Vec<String> = Vec::new();
    let outcome = block_on(async {
        let mut on_event = |event: SignInEvent| events.push(observed_event_line(&event));
        service.authenticate(&mut on_event).await
    });
    let (gateway, runtime) = service.into_parts();
    let outcome_line = match outcome {
        Ok(api_key) => format!("api key {api_key}"),
        Err(error) => format!("error {}", error.code.as_str()),
    };
    let observed = service_line(
        &events,
        &gateway.calls,
        gateway.calls.iter().filter(|call| *call == "poll").count(),
        &runtime.sleeps,
        &runtime.opened,
        gateway.challenge.as_deref(),
        &outcome_line,
    );
    let expected_events = case
        .events
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(corpus_event_line)
        .collect::<Vec<_>>();
    let expected_outcome = if let Some(code) = &case.error_code {
        format!("error {code}")
    } else if let Some(api_key) = &case.api_key {
        format!("api key {api_key}")
    } else {
        "<unrecorded>".to_owned()
    };
    let expected = service_line(
        &expected_events,
        case.gateway_calls.as_deref().unwrap_or_default(),
        case.poll_count.unwrap_or_default() as usize,
        case.sleeps.as_deref().unwrap_or_default(),
        case.browser_opened.as_deref().unwrap_or_default(),
        case.challenge.as_deref(),
        &expected_outcome,
    );
    (expected, observed)
}

/// Drives the port's HTTP gateway over the scenario's scripted responses,
/// comparing the issued requests and the outcome, both in the corpus's own
/// serialization.
fn replay_gateway_case(case: &ProtocolCase) -> (String, String) {
    let client = ScriptedHttpClient::new(case.script.as_array().cloned().unwrap_or_default());
    let mut gateway = HttpSignInGateway::new(
        case.browser_base.as_deref().unwrap_or_default(),
        case.api_base.as_deref().unwrap_or_default(),
        client,
    );
    let operation = case.op.as_deref().unwrap_or_default();
    let outcome = block_on(async {
        match operation {
            "create" => gateway
                .create_process("oracle-challenge")
                .await
                .map(|process| {
                    format!(
                        "process {} signIn {} poll {} expires {}",
                        process.process_id,
                        process.sign_in_url,
                        process.poll_url,
                        process.expires_at.to_iso8601(),
                    )
                }),
            "poll" => {
                // The capture feeds an off-base URL only to the revalidation
                // scenario; the input is not recorded, so it is restated here.
                let poll_url = if case.case == "gateway-poll-revalidates-before-requesting" {
                    "https://evil.example/api/oracle/poll"
                } else {
                    "https://console.mistral.ai/api/oracle/poll"
                };
                gateway.poll(poll_url).await.map(|poll| {
                    format!(
                        "status {} token {:?} message {:?}",
                        poll.status, poll.exchange_token, poll.message,
                    )
                })
            }
            "exchange" => gateway
                .exchange("oracle-process", "oracle-token", "oracle-verifier")
                .await
                .map(|api_key| format!("api key {api_key}")),
            other => panic!("signInProtocol/{} records unknown op {other}", case.case),
        }
    });
    let outcome_line = match outcome {
        Ok(line) => line,
        Err(error) => format!("error {}", error.code.as_str()),
    };
    let requests = Value::Array(gateway.into_client().requests);
    let observed = format!("requests={requests} outcome={outcome_line}");
    let expected_outcome = if let Some(code) = &case.error_code {
        format!("error {code}")
    } else if let Some(api_key) = &case.api_key {
        format!("api key {api_key}")
    } else if let Some(process_id) = &case.process_id {
        format!(
            "process {process_id} signIn {} poll {} expires {}",
            case.sign_in_url.as_deref().unwrap_or("<unrecorded>"),
            case.poll_url.as_deref().unwrap_or("<unrecorded>"),
            case.expires_at.as_deref().unwrap_or("<unrecorded>"),
        )
    } else if let Some(status) = &case.status {
        format!(
            "status {status} token {:?} message {:?}",
            case.exchange_token, case.message,
        )
    } else {
        "<unrecorded>".to_owned()
    };
    let expected_requests = Value::Array(case.requests.clone().unwrap_or_default());
    let expected = format!("requests={expected_requests} outcome={expected_outcome}");
    (expected, observed)
}

fn run_sign_in_protocol(cases: &[ProtocolCase], scripted_verifier: &str) -> usize {
    let mut report = Report::default();
    for case in cases {
        let (expected, observed) = match case.layer.as_str() {
            "service" => replay_service_case(case, scripted_verifier),
            "gateway" => replay_gateway_case(case),
            other => panic!("signInProtocol/{} records unknown layer {other}", case.case),
        };
        report.check(
            "signInProtocol",
            &case.case,
            "behavior",
            &expected,
            &observed,
        );
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
        // The port's validator vouches for the URL without rewriting it, so
        // an accepted verdict always passes the value through unchanged.
        let observed = match validate_url_against_base(&case.value, &case.base) {
            Ok(()) => "accepted unchanged",
            Err(auth::UrlRejection) => "rejected",
        };
        let expected = match case.verdict.as_str() {
            "accepted" if case.returned_unchanged == Some(true) => "accepted unchanged",
            "accepted" => "accepted rewritten",
            _ => "rejected",
        };
        report.check("urlValidation", &case.case, "verdict", &expected, &observed);
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
    report.check(
        "errorTaxonomy",
        "codeCount",
        "declared codes",
        &cases.len(),
        &SignInErrorCode::ALL.len(),
    );
    for (index, code) in cases.iter().enumerate() {
        let Some(port_code) = SignInErrorCode::ALL.get(index) else {
            report.check(
                "errorTaxonomy",
                &code.value,
                "declaration order",
                &code.value,
                &"<no port code at this position>".to_owned(),
            );
            continue;
        };
        report.check(
            "errorTaxonomy",
            &code.value,
            "code value and order",
            &format!("{} = {}", code.name, code.value),
            &format!(
                "{} = {}",
                port_code.as_str().to_ascii_uppercase(),
                port_code.as_str()
            ),
        );
        // `NOTICE`: this port's sentence must stay permanently unequal to
        // every reference-authored sentence for the code, compared by length
        // plus SHA-256 since the reference text is never committed.
        let sentence = port_code.message();
        let port_digest = Digested {
            length: sentence.len(),
            digest: Sha256::digest(sentence.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        };
        let collides = code.messages.contains(&port_digest);
        report.check(
            "errorTaxonomy",
            &format!("{}/prose", code.value),
            "original sentence",
            &"an original sentence".to_owned(),
            &if collides {
                "the reference's own sentence".to_owned()
            } else {
                "an original sentence".to_owned()
            },
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
    if DIVERGENCES.is_empty() {
        println!("  (empty: every family conforms)");
    }
    for (case, reason) in DIVERGENCES {
        println!("  {case}: {reason}");
    }
    let mut scenarios = 0;
    scenarios += run_constants(&corpus.constants);
    scenarios += run_auth_state(&corpus.auth_state);
    scenarios += run_persistence(&corpus.persistence);
    scenarios += run_sign_in_protocol(
        &corpus.sign_in_protocol,
        &corpus.constants.pkce.scripted_verifier,
    );
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
