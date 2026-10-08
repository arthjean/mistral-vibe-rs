//! Differential oracle for a session's rollout from its first configuration
//! read to the end of its lookup.
//!
//! `scripts/parity/experiments.py` builds sessions the way the reference's app
//! server builds one, through `HarnessProcess.build_root_blueprint` and the
//! loop's `start_initialize_experiments`, over a gate document, an eval cache
//! seed and a new or a resumed session, and records the `startup` family into
//! `crates/vibe-app-server/tests/experiments/corpus.json`. This module replays
//! each case through [`SessionExperiments`] end to end: the cache applied when
//! the enrollment is built, the decision whether a new session waits on its
//! lookup for a model, the lookup itself against a loopback eval endpoint, and
//! what the configuration and the cache hold afterward. Only the recapture
//! probe at the bottom skips when the checkout is absent or off-pin.
//!
//! Two inputs are rebuilt rather than copied. The cache seed is recorded by its
//! age, because the capture ran against a fixed clock and this replay runs
//! against the wall clock, so the entry is written at the same distance from
//! now. The document gains the loopback endpoint in its `[experiments]` table,
//! where the capture answered the request one call before the connection.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use serde_json::{Map, Value};
use vibe_core::events::ModelMessage;
use vibe_core::experiments::{EVAL_CACHE_FILE_NAME, hash_api_key};
use vibe_core::identity::{IdentityFuture, IdentityResolver, IdentityResult};
use vibe_core::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};
use vibe_core::telemetry::ExperimentExposures;
use vibe_core::whoami::{WhoAmIResolver, WhoAmIResult};

use super::SessionExperiments;
use super::tests::{EvalStub, credentials, service};

const CORPUS_RELATIVE: &str = "crates/vibe-app-server/tests/experiments/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/experiments.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the capture
/// script.
const CORPUS_SCHEMA_VERSION: u64 = 1;
/// Keys the corpus carries that are not families.
const METADATA: [&str; 3] = ["schemaVersion", "reference", "note"];
/// The credential the capture's Mistral provider resolves to, whose digest
/// keys the seeded entry.
const ORACLE_KEY: &str = "oracle-mistral-sentinel";
/// Every field a case authored, and every field both sides answer.
const INPUTS: [&str; 4] = ["document", "cached", "session", "live"];
const ANSWERS: [&str; 6] = [
    "startConfig",
    "awaitingExperimentModel",
    "hydrated",
    "afterLookup",
    "awaitingAfterLookup",
    "cachedAfterLookup",
];
/// The configuration fields a case reads, `STARTUP_FIELDS` in the capture.
const FIELDS: [&str; 5] = [
    "system_prompt_id",
    "managed_shell_tools_enabled",
    "smart_approve_available",
    "smart_approve_default",
    "experimental_enable_registry_skills",
];
/// The comparison floor, so a regeneration that captured almost nothing fails
/// instead of reporting a clean but empty run.
const MINIMUM_COMPARISONS: usize = 180;

/// v2.26.0 deletes the eval cache when a session opts out.
const OPT_OUT_CLEARS_THE_CACHE: &str = "v2.26.0 deletes the whole eval cache file when telemetry or experiments are off, \
     before the lookup is skipped (vibe/core/experiments/session.py:118, cache.py:63), because \
     the reference's launcher reads that cache without the configuration; this port skips the \
     lookup and leaves the seeded entry in place";

/// Comparisons where this build answers something other than the reference,
/// keyed `startup/{field}/{case}` as a divergence prints, each with the
/// reason. A divergence no entry names fails the replay, and so does an entry
/// whose divergence stopped reproducing. Every entry dates from the v2.26.0
/// re-pin (`376f6a3`).
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "startup/cachedAfterLookup/telemetry-disabled/cached/new",
        OPT_OUT_CLEARS_THE_CACHE,
    ),
    (
        "startup/cachedAfterLookup/telemetry-disabled/cached/resumed",
        OPT_OUT_CLEARS_THE_CACHE,
    ),
    (
        "startup/cachedAfterLookup/telemetry-disabled/cached-past-the-bound/new",
        OPT_OUT_CLEARS_THE_CACHE,
    ),
    (
        "startup/cachedAfterLookup/telemetry-disabled/cached-past-the-bound/resumed",
        OPT_OUT_CLEARS_THE_CACHE,
    ),
    (
        "startup/cachedAfterLookup/telemetry-disabled/cached-at-the-baseline/new",
        OPT_OUT_CLEARS_THE_CACHE,
    ),
    (
        "startup/cachedAfterLookup/telemetry-disabled/cached-at-the-baseline/resumed",
        OPT_OUT_CLEARS_THE_CACHE,
    ),
    (
        "startup/cachedAfterLookup/experiments-disabled/cached/new",
        OPT_OUT_CLEARS_THE_CACHE,
    ),
    (
        "startup/cachedAfterLookup/experiments-disabled/cached/resumed",
        OPT_OUT_CLEARS_THE_CACHE,
    ),
    (
        "startup/cachedAfterLookup/experiments-disabled/cached-past-the-bound/new",
        OPT_OUT_CLEARS_THE_CACHE,
    ),
    (
        "startup/cachedAfterLookup/experiments-disabled/cached-past-the-bound/resumed",
        OPT_OUT_CLEARS_THE_CACHE,
    ),
    (
        "startup/cachedAfterLookup/experiments-disabled/cached-at-the-baseline/new",
        OPT_OUT_CLEARS_THE_CACHE,
    ),
    (
        "startup/cachedAfterLookup/experiments-disabled/cached-at-the-baseline/resumed",
        OPT_OUT_CLEARS_THE_CACHE,
    ),
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root resolves")
}

fn corpus() -> Map<String, Value> {
    let text = fs::read_to_string(repo_root().join(CORPUS_RELATIVE)).expect("the corpus reads");
    let corpus = serde_json::from_str::<Value>(&text)
        .expect("the corpus parses")
        .as_object()
        .cloned()
        .expect("the corpus is an object");
    assert_eq!(
        corpus.get("schemaVersion").and_then(Value::as_u64),
        Some(CORPUS_SCHEMA_VERSION)
    );
    assert_eq!(
        corpus
            .get("reference")
            .and_then(|reference| reference.get("commit"))
            .and_then(Value::as_str),
        Some(REFERENCE_COMMIT),
        "the corpus was captured at another pin; regenerate it with {CAPTURE_SCRIPT}"
    );
    corpus
}

/// A user the lookup resolves, so the response it answers is stored.
struct OracleUser;

impl IdentityResolver for OracleUser {
    fn resolve<'a>(
        &'a self,
        _base_url: &'a str,
        _api_key: &'a str,
        _timeout: Option<std::time::Duration>,
    ) -> IdentityFuture<'a, Option<IdentityResult>> {
        Box::pin(std::future::ready(Some(IdentityResult {
            id: "oracle-user".to_owned(),
            email: None,
            first_name: None,
            last_name: None,
            workspace: None,
            organization: None,
        })))
    }
}

/// An account read that answers nothing, as the capture's does.
struct NoAccount;

impl WhoAmIResolver for NoAccount {
    fn resolve<'a>(
        &'a self,
        _base_url: &'a str,
        _api_key: &'a str,
        _timeout: Option<std::time::Duration>,
    ) -> IdentityFuture<'a, Option<WhoAmIResult>> {
        Box::pin(std::future::ready(None))
    }
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// The fields a case reads off one configuration load, with whether the
/// configuration reports a session still waiting on its lookup.
fn read_configuration(service: &crate::workspace::WorkspaceService) -> (Value, bool) {
    let snapshot = service
        .layered_config()
        .load()
        .expect("the configuration composes");
    let fields = FIELDS
        .into_iter()
        .map(|field| {
            let value = snapshot
                .effective
                .get(field)
                .and_then(|value| serde_json::to_value(value).ok())
                .unwrap_or(Value::Null);
            (field.to_owned(), value)
        })
        .collect::<Map<_, _>>();
    (Value::Object(fields), snapshot.awaiting_experiment_model)
}

/// What the cache holds for the capture's credential.
fn cached_payload(vibe_home: &std::path::Path) -> Value {
    fs::read_to_string(vibe_home.join(EVAL_CACHE_FILE_NAME))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|entries| {
            entries
                .get(hash_api_key(ORACLE_KEY))?
                .get("payload")
                .cloned()
        })
        .unwrap_or(Value::Null)
}

/// One case driven through [`SessionExperiments`], answered field by field.
async fn port_case(case: &Map<String, Value>) -> Map<String, Value> {
    let live = case.get("live").map(Value::to_string).unwrap_or_default();
    let stub = EvalStub::start(live).await;
    let document = case
        .get("document")
        .and_then(Value::as_str)
        .expect("every startup case carries its document")
        .replacen(
            "[experiments]\n",
            &format!(
                "[experiments]\napi_host = \"http://127.0.0.1:{}\"\nclient_key = \"sdk-oracle\"\n",
                stub.port
            ),
            1,
        );
    let root = tempfile::tempdir().expect("a scratch directory");
    let vibe_home = root.path().join("home/.vibe");
    fs::create_dir_all(&vibe_home).expect("the vibe home");
    if let Some(seed) = case.get("cached").and_then(Value::as_object) {
        let owner = match seed.get("owner").and_then(Value::as_str) {
            Some("oracle") => hash_api_key(ORACLE_KEY),
            _ => "0".repeat(32),
        };
        let age = seed
            .get("ageSeconds")
            .and_then(Value::as_i64)
            .expect("a seed records its age");
        let entries = serde_json::json!({
            owner: {
                "stored_at_timestamp": now_seconds() - age,
                "payload": seed.get("payload").cloned().unwrap_or(Value::Null),
            }
        });
        fs::write(vibe_home.join(EVAL_CACHE_FILE_NAME), entries.to_string())
            .expect("the seed writes");
    }
    let service = service(root.path(), &document);
    let store = service.session_store();
    let working = service
        .layered_config()
        .working_directory()
        .display()
        .to_string();
    let session_id = match case.get("session").and_then(Value::as_str) {
        Some("resumed") => {
            let mut metadata = store
                .create("oracle-resumed-session", &working, None, 1)
                .expect("the session is named");
            store
                .append_messages(&mut metadata, &[ModelMessage::user("resume me")], 2)
                .expect("the session is saved");
            metadata.id
        }
        _ => {
            store
                .create("oracle-new-session", &working, None, 1)
                .expect("the session is named")
                .id
        }
    };

    let experiments = Arc::new(
        SessionExperiments::new(
            &service,
            credentials(),
            None,
            ExperimentExposures::default(),
        )
        .resolving_identity_through(Arc::new(OracleUser))
        .resolving_account_through(Arc::new(NoAccount)),
    );
    let mut answers = Map::new();
    let (start_config, _) = read_configuration(&service);
    answers.insert("startConfig".to_owned(), start_config);
    answers.insert(
        "hydrated".to_owned(),
        experiments
            .manager
            .lock()
            .await
            .export_state()
            .and_then(|state| serde_json::to_value(state).ok())
            .unwrap_or(Value::Null),
    );
    // The lookup runs on this current-thread runtime only once the test
    // yields, so this read sees the decision before the lookup could settle,
    // as the capture read the loop's flag before it started the task.
    experiments.start(&session_id);
    let (_, awaiting) = read_configuration(&service);
    assert_eq!(
        awaiting,
        experiments.awaiting_model(),
        "the configuration publishes what the enrollment decided"
    );
    answers.insert("awaitingExperimentModel".to_owned(), Value::Bool(awaiting));
    experiments.settle().await;
    let (after, awaiting_after) = read_configuration(&service);
    assert_eq!(awaiting_after, experiments.awaiting_model());
    answers.insert("afterLookup".to_owned(), after);
    answers.insert(
        "awaitingAfterLookup".to_owned(),
        Value::Bool(awaiting_after),
    );
    answers.insert("cachedAfterLookup".to_owned(), cached_payload(&vibe_home));
    experiments.close().await;
    answers
}

#[tokio::test]
async fn the_committed_corpus_replays_against_this_port() {
    let corpus = corpus();
    let families = corpus
        .keys()
        .map(String::as_str)
        .filter(|key| !METADATA.contains(key))
        .collect::<Vec<_>>();
    assert_eq!(
        families,
        ["startup"],
        "the corpus and this replay disagree on which families exist"
    );
    let cases = corpus
        .get("startup")
        .and_then(Value::as_array)
        .expect("the startup family is a list");
    let mut comparisons = 0;
    let mut divergences = Vec::new();
    let mut observed = Vec::new();
    for case in cases {
        let case = case.as_object().expect("a case is an object");
        let identifier = case.get("id").and_then(Value::as_str).unwrap_or_default();
        let unread = case
            .keys()
            .filter(|key| {
                *key != "id" && !INPUTS.contains(&key.as_str()) && !ANSWERS.contains(&key.as_str())
            })
            .collect::<Vec<_>>();
        assert!(
            unread.is_empty(),
            "the {identifier} case carries fields this replay does not read: {unread:?}"
        );
        let answers = port_case(case).await;
        for field in ANSWERS {
            let Some(expected) = case.get(field) else {
                continue;
            };
            comparisons += 1;
            let actual = answers.get(field).unwrap_or(&Value::Null);
            if actual != expected {
                let key = format!("startup/{field}/{identifier}");
                if !DIVERGENCES.iter().any(|(entry, _)| *entry == key) {
                    divergences.push(format!("{key}: reference {expected}, port {actual}"));
                }
                observed.push(key);
            }
        }
    }
    println!(
        "experiments startup: {}/{comparisons} conform across {} cases ({} ledgered) at {}",
        comparisons - observed.len(),
        cases.len(),
        observed.len() - divergences.len(),
        &REFERENCE_COMMIT[..12],
    );
    assert!(
        divergences.is_empty(),
        "the startup family diverges from the reference where no ledger entry records it:\n{}",
        divergences.join("\n")
    );
    let stale = DIVERGENCES
        .iter()
        .map(|(entry, _)| *entry)
        .filter(|entry| !observed.iter().any(|key| key == entry))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "these startup entries conform now and their ledger entry is stale: {stale:?}"
    );
    assert!(
        comparisons >= MINIMUM_COMPARISONS,
        "the corpus replays {comparisons} comparisons, below the {MINIMUM_COMPARISONS} floor; \
         regenerate it with {CAPTURE_SCRIPT}"
    );
}

/// The corpus is only an oracle for as long as it still describes the pinned
/// reference. This probe recaptures it where the checkout is present and on the
/// pin, and skips everywhere else naming the pin and the way back.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "experiments startup") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let recaptured = repository.join("target/startup-corpus.json");
    let output = Command::new("python3")
        .arg(repository.join(CAPTURE_SCRIPT))
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/startup-full.json"))
        .arg("--corpus")
        .arg(repository.join("target/startup-engine-corpus.json"))
        .arg("--promo-corpus")
        .arg(repository.join("target/startup-promo-corpus.json"))
        .arg("--startup-corpus")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the experiments capture script runs");
    assert!(
        output.status.success(),
        "the experiments capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fresh: Value = serde_json::from_str(
        &fs::read_to_string(&recaptured).expect("the recaptured corpus is readable"),
    )
    .expect("the recaptured corpus parses");
    let committed: Value = serde_json::from_str(
        &fs::read_to_string(repository.join(CORPUS_RELATIVE)).expect("the corpus is readable"),
    )
    .expect("the corpus parses");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate it \
         with `{CAPTURE_SCRIPT}`"
    );
}
