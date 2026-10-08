//! Differential oracle for the layers around the merge: the file stack, the
//! writes routed into it and the text they leave, the environment, the agent
//! profile, the global dotenv file, and the sign-in origin rewrite a provider
//! can opt into.
//!
//! `scripts/parity/config_surface.py` builds each stack through the reference's
//! own `build_default_orchestrator` over a temporary home and checkout, reads
//! the environment and agent profile through their own layers, the dotenv file
//! through `load_dotenv_values`, and the sign-in URLs through the gateway's
//! validator. This module replays every case against the port and fails on any
//! difference the stack ledger below does not name, and on an entry whose
//! difference stopped reproducing.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::{Map, Value as JsonValue};

use super::*;
use crate::parity::REFERENCE_COMMIT;

const CORPUS_RELATIVE: &str = "tests/config-surface/corpus.json";
/// What the capture writes for the per-scenario temporary root.
const ROOT_PLACEHOLDER: &str = "{root}";

/// v2.26.0 turned background session titles on by default.
const GENERATE_TITLES: &str = "v2.26.0 turns background session titles on by default (models.py:112); \
     registry.rs ships `generate_titles = false`";

/// Pointers at which a stack or write case diverges, as `(case, pointer,
/// reason)`. A comparison descends into tables, so an entry names the one key
/// that moved. Every entry dates from the v2.26.0 re-pin (`376f6a3`).
const STACK_DIVERGENCES: &[(&str, &str, &str)] = &[
    (
        "stack-trusted-project-inherits-the-user-file",
        "/session_logging/generate_titles",
        GENERATE_TITLES,
    ),
    (
        "write-keeps-the-file-order-and-appends-new-keys",
        "/session_logging/generate_titles",
        GENERATE_TITLES,
    ),
    (
        "write-keeps-the-file-order-and-appends-new-keys",
        "after the writes: /session_logging/generate_titles",
        GENERATE_TITLES,
    ),
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Corpus {
    reference: Reference,
    stack: StackCorpus,
    environment: Vec<EnvironmentCase>,
    agent_profiles: Vec<AgentProfileCase>,
    dotenv: Vec<DotenvCase>,
    origin_rewrite: Vec<RewriteCase>,
    encoding: Vec<EncodingCase>,
}

/// A document authored for the corpus and the text `tomli_w` writes it as.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EncodingCase {
    document: String,
    written: String,
}

#[derive(Debug, Deserialize)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackCorpus {
    stacks: Vec<StackCase>,
    writes: Vec<StackCase>,
}

/// One home and checkout: the user file, the project files by directory, the
/// directories the trust store trusts, the working directory, and the writes
/// made once the stack loaded.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StackCase {
    name: String,
    user: Option<String>,
    projects: BTreeMap<String, String>,
    trusted: Vec<String>,
    cwd: String,
    effective: Map<String, JsonValue>,
    writable_layer: String,
    #[serde(default)]
    writes: Vec<WriteCase>,
    files: Option<BTreeMap<String, String>>,
    after_writes: Option<Map<String, JsonValue>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteCase {
    path: String,
    value: JsonValue,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentCase {
    name: String,
    variables: BTreeMap<String, String>,
    layer: Map<String, JsonValue>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentProfileCase {
    name: String,
    overrides: Map<String, JsonValue>,
    layer: Map<String, JsonValue>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DotenvCase {
    name: String,
    file: String,
    environ: BTreeMap<String, String>,
    result: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RewriteCase {
    value: String,
    base: String,
    answered: Option<String>,
}

fn corpus() -> Corpus {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CORPUS_RELATIVE);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("configuration corpus at {}: {error}", path.display()));
    let corpus: Corpus = serde_json::from_str(&raw)
        .unwrap_or_else(|error| panic!("configuration corpus at {}: {error}", path.display()));
    assert_eq!(corpus.reference.commit, REFERENCE_COMMIT);
    corpus
}

fn json(value: &impl serde::Serialize) -> JsonValue {
    serde_json::to_value(value).expect("the value serializes")
}

fn toml_table(map: &Map<String, JsonValue>) -> Table {
    Table::try_from(JsonValue::Object(map.clone())).expect("the document converts to TOML")
}

/// Substitutes the temporary root the capture wrote a placeholder for.
fn resolve_root(value: &JsonValue, root: &Path) -> JsonValue {
    match value {
        JsonValue::String(text) => {
            JsonValue::String(text.replace(ROOT_PLACEHOLDER, &root.display().to_string()))
        }
        JsonValue::Object(entries) => JsonValue::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), resolve_root(value, root)))
                .collect(),
        ),
        JsonValue::Array(entries) => JsonValue::Array(
            entries
                .iter()
                .map(|entry| resolve_root(entry, root))
                .collect(),
        ),
        value => value.clone(),
    }
}

/// Drops every table left empty, which pydantic keeps for a nested model whose
/// only key it ignored and which merges as nothing either way.
fn pruned(value: &JsonValue) -> Option<JsonValue> {
    match value {
        JsonValue::Object(entries) => {
            let kept: Map<String, JsonValue> = entries
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), pruned(value)?)))
                .collect();
            (!kept.is_empty()).then_some(JsonValue::Object(kept))
        }
        value => Some(value.clone()),
    }
}

/// Every pointer under `pointer` at which two values disagree, as
/// `(pointer, reference, port)`. Tables are walked key by key, and a key one
/// side lacks is one difference rendered `absent` on that side; any other pair
/// is one difference.
fn differences(
    pointer: &str,
    reference: &JsonValue,
    port: &JsonValue,
    found: &mut Vec<(String, String, String)>,
) {
    let rendered =
        |value: Option<&JsonValue>| value.map_or_else(|| "absent".to_owned(), JsonValue::to_string);
    match (reference, port) {
        (JsonValue::Object(reference), JsonValue::Object(port)) => {
            let keys = reference.keys().chain(port.keys()).collect::<BTreeSet<_>>();
            for key in keys {
                let nested = format!("{pointer}/{key}");
                match (reference.get(key), port.get(key)) {
                    (Some(reference), Some(port)) => differences(&nested, reference, port, found),
                    (reference, port) => {
                        found.push((nested, rendered(reference), rendered(port)));
                    }
                }
            }
        }
        (reference, port) if reference == port => {}
        (reference, port) => {
            found.push((pointer.to_owned(), reference.to_string(), port.to_string()))
        }
    }
}

/// The name the reference gives the layer a write without a target lands in.
fn layer_name(target: ConfigTarget) -> &'static str {
    match target {
        ConfigTarget::User => "user-toml",
        ConfigTarget::Project => "project-toml",
        ConfigTarget::Ephemeral => "overrides",
    }
}

/// Every `config.toml` under `root`, by its path relative to it.
fn config_files(root: &Path) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).expect("the directory reads") {
            let path = entry.expect("the entry reads").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name().is_some_and(|name| name == CONFIG_FILE) {
                let relative = path
                    .strip_prefix(root)
                    .expect("under the root")
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(relative, fs::read_to_string(&path).expect("the file reads"));
            }
        }
    }
    files
}

fn replay_stack(case: &StackCase, observed: &mut Vec<(String, String, String, String)>) {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path().canonicalize().expect("canonical root");
    let home = root.join(".vibe");
    fs::create_dir_all(&home).expect("home directory");
    if let Some(user) = &case.user {
        fs::write(home.join(CONFIG_FILE), user).expect("user fixture");
    }
    for (directory, document) in &case.projects {
        let directory = root.join(directory).join(".vibe");
        fs::create_dir_all(&directory).expect("project directory");
        fs::write(directory.join(CONFIG_FILE), document).expect("project fixture");
    }
    let cwd = root.join(&case.cwd);
    fs::create_dir_all(&cwd).expect("working directory");
    let store = crate::trust::TrustStore::for_vibe_home(&home);
    for trusted in &case.trusted {
        store.add_trusted(&root.join(trusted)).expect("trust grant");
    }
    let config = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: cwd.clone(),
        },
        registry::default_document(),
    )
    .with_project_trusted(store.is_trusted(&cwd) == Some(true));

    let mut check = |what: &str, reference: &JsonValue, port: &JsonValue| {
        let mut found = Vec::new();
        differences(what, &resolve_root(reference, &root), port, &mut found);
        observed.extend(
            found
                .into_iter()
                .map(|(pointer, reference, port)| (case.name.clone(), pointer, reference, port)),
        );
    };
    let snapshot = config
        .load()
        .unwrap_or_else(|error| panic!("{}: {error}", case.name));
    let effective = json(&snapshot.effective);
    for (key, expected) in &case.effective {
        let found = effective.get(key).cloned().unwrap_or(JsonValue::Null);
        check(&format!("/{key}"), expected, &found);
    }
    check(
        "the writable layer",
        &JsonValue::from(case.writable_layer.as_str()),
        &JsonValue::from(layer_name(snapshot.selected_target)),
    );
    if case.writes.is_empty() {
        return;
    }
    for write in &case.writes {
        let value = Value::try_from(write.value.clone()).expect("the value converts to TOML");
        let outcome = config
            .apply_patch(
                &[ConfigPatchOp {
                    mutation: ConfigMutation::set([write.path.as_str()], value),
                    target: None,
                }],
                "parity",
            )
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        assert!(
            outcome.failures.is_empty(),
            "{}: {:?}",
            case.name,
            outcome.failures
        );
    }
    check(
        "the files",
        &json(case.files.as_ref().expect("a write case records its files")),
        &json(&config_files(&root)),
    );
    let after = json(&config.load().expect("the stack reloads").effective);
    for (key, expected) in case.after_writes.iter().flatten() {
        let found = after.get(key).cloned().unwrap_or(JsonValue::Null);
        check(&format!("after the writes: /{key}"), expected, &found);
    }
}

#[test]
fn every_layer_stack_composes_and_writes_as_the_reference_does() {
    let corpus = corpus();
    let cases = corpus
        .stack
        .stacks
        .iter()
        .chain(&corpus.stack.writes)
        .collect::<Vec<_>>();
    assert!(!corpus.stack.writes.is_empty());
    let mut observed = Vec::new();
    for case in &cases {
        replay_stack(case, &mut observed);
    }
    let unrecorded = observed
        .iter()
        .filter(|(case, pointer, ..)| {
            !STACK_DIVERGENCES
                .iter()
                .any(|(name, entry, _)| name == case && entry == pointer)
        })
        .map(|(case, pointer, reference, port)| {
            format!("{case}: {pointer}: reference {reference}, port {port}")
        })
        .collect::<Vec<_>>();
    assert!(
        unrecorded.is_empty(),
        "the layer stacks diverge where no ledger entry records it:\n{}",
        unrecorded.join("\n")
    );
    let stale = STACK_DIVERGENCES
        .iter()
        .filter(|(name, entry, _)| {
            !observed
                .iter()
                .any(|(case, pointer, ..)| case == name && pointer == entry)
        })
        .map(|(name, entry, _)| format!("{name}: {entry}"))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "the layer stacks: these recorded divergences no longer reproduce: {stale:?}"
    );
    let diverging = cases
        .iter()
        .filter(|case| observed.iter().any(|(name, ..)| name == &case.name))
        .count();
    println!(
        "config surface: {}/{} layer stacks conform ({diverging} ledgered), {} with writes",
        cases.len() - diverging,
        cases.len(),
        corpus.stack.writes.len()
    );
}

#[test]
fn every_environment_layer_reads_as_the_reference_reads_it() {
    let corpus = corpus();
    let mut failures = Vec::new();
    for case in &corpus.environment {
        let table = environment::environment_table(&case.variables)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let port = pruned(&json(&table)).unwrap_or_default();
        let reference = pruned(&JsonValue::Object(case.layer.clone())).unwrap_or_default();
        if port != reference {
            failures.push(format!("{}: reference {reference}, port {port}", case.name));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    println!(
        "config surface: {n}/{n} environment layers conform",
        n = corpus.environment.len()
    );
}

#[test]
fn every_agent_profile_layer_keeps_what_the_reference_keeps() {
    let corpus = corpus();
    for case in &corpus.agent_profiles {
        let temporary = tempfile::tempdir().expect("temporary root");
        let config = LayeredConfig::new(
            ConfigPaths {
                vibe_home: temporary.path().join(".vibe"),
                working_directory: temporary.path().to_path_buf(),
            },
            Table::new(),
        )
        .with_agent_overlay(toml_table(&case.overrides));
        assert_eq!(
            json(&config.agent),
            JsonValue::Object(case.layer.clone()),
            "{}",
            case.name
        );
    }
}

#[test]
fn every_dotenv_file_resolves_as_the_reference_loads_it() {
    let corpus = corpus();
    let names = regex::Regex::new(r"[A-Z][A-Z0-9_]+").expect("the pattern compiles");
    for case in &corpus.dotenv {
        let values = DotenvValues::parse(&case.file);
        // Every name the file or the seeded environment mentions, so a
        // variable only the port declares is caught as well as one it misses.
        let candidates = names
            .find_iter(&case.file)
            .map(|found| found.as_str().to_owned())
            .chain(case.environ.keys().cloned())
            .collect::<BTreeSet<_>>();
        let port = candidates
            .into_iter()
            .filter_map(|name| {
                let file = values.file_variable(&name).map(str::to_owned);
                let resolved = dotenv::resolve(case.environ.get(&name).cloned(), file.as_ref())
                    .or_else(|| case.environ.get(&name).cloned())?;
                Some((name, resolved))
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(port, case.result, "{}", case.name);
    }
}

#[test]
fn every_rewritten_sign_in_url_lands_where_the_reference_lands_it() {
    let corpus = corpus();
    for case in &corpus.origin_rewrite {
        let port =
            crate::auth::sign_in_http::rehome_url_against_base(&case.value, &case.base, true).ok();
        assert_eq!(port, case.answered, "{} against {}", case.value, case.base);
    }
}

#[test]
fn every_document_is_written_back_as_the_reference_writes_it() {
    let corpus = corpus();
    for case in &corpus.encoding {
        let table = case
            .document
            .parse::<Table>()
            .unwrap_or_else(|error| panic!("{:?}: {error}", case.document));
        assert_eq!(
            encode::encode_document(&table),
            case.written,
            "{:?}",
            case.document
        );
    }
    println!(
        "config surface: {n}/{n} documents written back as the reference writes them",
        n = corpus.encoding.len()
    );
}
