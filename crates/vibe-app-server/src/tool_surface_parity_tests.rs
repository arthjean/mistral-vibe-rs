//! Differential oracle for the published tool surface.
//!
//! The Python reference is the authority on which tools exist and what
//! `tools[].function.parameters` each one publishes. This module captures that
//! surface from the pinned checkout and diffs it against the definitions a real
//! session registers, reporting missing names, invented names and per-name
//! schema divergence as JSON pointers.
//!
//! Two rules keep the comparison honest. Description *text* is never compared,
//! only its presence: `NOTICE` forbids shipping reference prose, so the Rust
//! descriptions are original and are held to directive coverage elsewhere.
//! And the corpus itself is a gitignored local artifact, because it holds that
//! prose verbatim.
//!
//! The surviving divergence is checked against a committed baseline, so the
//! remaining gap is explicit and any *new* divergence fails the suite while the
//! later epics close the known one.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use secrecy::SecretString;
use serde::Deserialize;
use serde_json::{Map, Value};
use vibe_core::matching::NameFilter;
use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionStore, ToolGuard,
    TrustDecision, TrustRootKind,
};
use vibe_core::skills::SkillDiscovery;
use vibe_core::tools::builtins::{BuiltinTools, WebSearchAccess};
use vibe_core::tools::{
    ToolError, ToolHandler, ToolHandlerFuture, ToolInvocation, ToolOutputSink, ToolRegistry,
    ToolSpec, coerce_and_validate,
};
use vibe_core::workspace::{ReviewManager, Workspace, WorkspaceTools};

use vibe_core::platform::Platform;
use vibe_core::tools::shell::{HostShells, ShellRollout, ShellTools};

use crate::client::interactive::InteractiveSessionToolFactory;
use crate::client::live::delegation::task_spec;
use crate::server::SessionToolFactory;

use vibe_core::parity::{REFERENCE_COMMIT, off_pin_reason, pinned_interpreter, reference_root};

const CORPUS_RELATIVE: &str = ".parity/tool-surface-corpus.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 4;
const CAPTURE_SCRIPT: &str = "scripts/parity/tool_surface.py";
const BASELINE_RELATIVE: &str = "crates/vibe-app-server/tests/tool-surface/baseline.json";
/// The committed conformance target, which is what CI diffs against: it has no
/// pinned Python checkout, so the corpus cannot be recaptured there.
const DIGEST_RELATIVE: &str = "crates/vibe-app-server/tests/tool-surface/digest.json";
/// The digest layout this runner reads, matching `DIGEST_SCHEMA_VERSION` in the
/// capture script.
const DIGEST_SCHEMA_VERSION: u32 = 1;
/// The committed argument fixtures, replayed unconditionally: unlike the corpus
/// they carry no reference prose, so CI reports a conformance count rather than
/// skipping for want of a checkout.
const FIXTURES_RELATIVE: &str = "crates/vibe-app-server/tests/tool-surface/fixtures.json";
/// The fixture layout this runner reads, matching `FIXTURES_SCHEMA_VERSION` in
/// the capture script.
const FIXTURES_SCHEMA_VERSION: u32 = 2;
/// The floor the fixture set commits to, so a regeneration that captured almost
/// nothing fails instead of reporting a clean but empty run.
const MINIMUM_FIXTURES: usize = 92;
/// The committed filter gates: the names the reference publishes for each
/// `enabled_tools` and `disabled_tools` pair. Replayed unconditionally for the
/// same reason as the fixtures, since a gate case carries no prose either.
const GATES_RELATIVE: &str = "crates/vibe-app-server/tests/tool-surface/gates.json";
/// The gate layout this runner reads, matching `GATES_SCHEMA_VERSION` in the
/// capture script.
const GATES_SCHEMA_VERSION: u32 = 1;
/// The floor the multi-argument probes commit to, so a regeneration that lost
/// them reports a failure rather than a vacuous pass.
const MINIMUM_MULTI_ARGUMENT_PROBES: usize = 6;
/// Stands in for every description string so presence is compared and text is
/// not, keeping the diff inside what `NOTICE` allows.
const DESCRIBED: &str = "<described>";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    platform: String,
    tools: Vec<ReferenceTool>,
    /// The surface the managed shell rollout selects, which replaces `bash`
    /// with its eight-property variant and adds the four session tools. It is
    /// defaulted so an older corpus still parses and reaches the version skip.
    #[serde(default)]
    managed_tools: Vec<ReferenceTool>,
    /// The two Windows-only families, read from the reference declarations
    /// because a Linux host cannot make them available. Defaulted for the same
    /// reason as `managed_tools`.
    #[serde(default)]
    windows_tools: Vec<WindowsTool>,
    fixtures: Vec<Fixture>,
}

#[derive(Debug, Deserialize)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
struct ReferenceTool {
    name: String,
    parameters: Value,
}

#[derive(Debug, Deserialize)]
struct WindowsTool {
    family: String,
    name: String,
    parameters: Value,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    tool: String,
    case: String,
    arguments: Value,
    accepted: bool,
    /// Present exactly when the reference rejected the payload. It records the
    /// shape of the error a model reads back, never the text: `NOTICE` forbids
    /// committing reference prose, so the string itself is only a digest.
    #[serde(default)]
    rejection: Option<Rejection>,
}

/// What the reference answered a rejected call, recorded structurally.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rejection {
    /// The exception class the model saw, quoted in a divergence report so it
    /// says what the reference raised rather than only where.
    error: String,
    /// Whether the message named the tool that refused the call.
    names_tool: bool,
    /// The top-level arguments it objected to.
    #[expect(dead_code, reason = "the pointers carry the same names, spelled fully")]
    arguments: Vec<String>,
    /// Every place it objected to, in this repository's own pointer spelling.
    pointers: Vec<String>,
    /// A digest of the full text, so a message that changed upstream is caught
    /// without the message ever being stored.
    #[expect(
        dead_code,
        reason = "the digest is a re-pin signal, not a local assertion"
    )]
    digest: String,
}

/// A payload breaking more than one argument at once, and the answer it drew.
#[derive(Debug, Deserialize)]
struct MultiArgumentProbe {
    tool: String,
    case: String,
    arguments: Value,
    rejection: Rejection,
}

/// The names the reference publishes under each configured filter pair.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Gates {
    schema_version: u32,
    reference_commit: String,
    #[expect(dead_code, reason = "the platform documents the capture host")]
    platform: String,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    gates: Vec<Gate>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Gate {
    case: String,
    enabled_tools: Vec<String>,
    disabled_tools: Vec<String>,
    names: Vec<String>,
}

/// The committed argument fixtures: payloads this repository authored and the
/// accept-or-reject verdict the reference Pydantic gave each one.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Fixtures {
    schema_version: u32,
    reference_commit: String,
    #[expect(dead_code, reason = "the platform documents the capture host")]
    platform: String,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    fixtures: Vec<Fixture>,
    /// Payloads breaking more than one argument at once. They sit beside the
    /// fixture list rather than in it, because a fixture is an accept-or-reject
    /// verdict and these are a second measurement over the same surface: what a
    /// rejection names when several arguments are wrong.
    multi_argument: Vec<MultiArgumentProbe>,
}

/// The committed canonical surface: published names and schema structure, with
/// every description reduced to [`DESCRIBED`] so no reference prose is stored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Digest {
    schema_version: u32,
    reference_commit: String,
    platform: String,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    tools: BTreeMap<String, Value>,
    managed_tools: BTreeMap<String, Value>,
    windows_tools: BTreeMap<String, BTreeMap<String, Value>>,
}

/// The divergence the port still carries, shrunk by the epics that follow.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Baseline {
    reference_commit: String,
    platform: String,
    note: String,
    missing_names: BTreeSet<String>,
    extra_names: BTreeSet<String>,
    schema_divergence: BTreeMap<String, BTreeSet<String>>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

/// The pinned checkout and an interpreter that can drive it, or `None` when
/// this machine cannot act as the oracle.
fn pinned_reference() -> Option<(PathBuf, PathBuf)> {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "tool-surface") {
        eprintln!("{reason}");
        return None;
    }
    let interpreter = pinned_interpreter(&root)?;
    Some((root, interpreter))
}

/// Recaptures the corpus when the pinned checkout is present, otherwise falls
/// back to a corpus captured earlier from that same commit.
fn corpus() -> Option<Corpus> {
    let root = repo_root();
    let path = root.join(CORPUS_RELATIVE);
    if let Some((reference, interpreter)) = pinned_reference() {
        let capture = Command::new(interpreter)
            .arg(root.join(CAPTURE_SCRIPT))
            .arg("--reference")
            .arg(&reference)
            .arg("--output")
            .arg(&path)
            .current_dir(&root)
            .output()
            .expect("the capture script runs");
        assert!(
            capture.status.success(),
            "tool-surface capture failed: {}",
            String::from_utf8_lossy(&capture.stderr)
        );
    }
    let Ok(raw) = fs::read_to_string(&path) else {
        eprintln!(
            "skipping the tool-surface oracle: no corpus at {} and no pinned checkout on \
             {REFERENCE_COMMIT}",
            path.display()
        );
        return None;
    };
    let corpus: Corpus = serde_json::from_str(&raw).expect("the corpus parses");
    if let Some(reason) = skip_reason_for(
        corpus.schema_version,
        &corpus.reference.commit,
        &corpus.platform,
        running_platform(),
    ) {
        eprintln!("{reason}");
        return None;
    }
    Some(corpus)
}

fn running_platform() -> &'static str {
    std::env::consts::OS
}

/// Why a corpus cannot answer for this run, or `None` when it can.
///
/// A corpus is only an oracle for the commit it was captured from and for the
/// platform whose availability rules it recorded, so both mismatches skip with
/// an explicit message rather than failing or passing silently.
fn skip_reason_for(
    captured_version: u32,
    captured_commit: &str,
    captured_platform: &str,
    running: &str,
) -> Option<String> {
    if captured_version != CORPUS_SCHEMA_VERSION {
        return Some(format!(
            "skipping the tool-surface oracle: the corpus is at schema version {captured_version}, \
             not the expected {CORPUS_SCHEMA_VERSION}; regenerate it with {CAPTURE_SCRIPT}"
        ));
    }
    if captured_commit != REFERENCE_COMMIT {
        return Some(format!(
            "skipping the tool-surface oracle: the corpus was captured from {captured_commit}, \
             not the pinned {REFERENCE_COMMIT}"
        ));
    }
    if captured_platform != running {
        return Some(format!(
            "skipping the tool-surface oracle: the corpus records the {captured_platform} \
             surface and this host is {running}"
        ));
    }
    None
}

/// Refuses every approval, so a case measures the published surface rather than
/// a prompt.
struct RejectApproval;

impl ApprovalAgent for RejectApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::Deny) })
    }
}

/// The tool definitions a real interactive session publishes: the universal
/// builtins and the workspace tools the server registers, the interactive tools
/// its surface extension adds, and `task`, which the live driver registers from
/// [`task_spec`] once per turn, before it sends the definitions to the model.
///
/// `web_search` is conditional on a Mistral key resolving, so it registers here
/// exactly when the corpus recorded the reference publishing it. The
/// availability rule itself is proven by a unit test, because the oracle can
/// only compare the two surfaces under one configuration at a time.
async fn published_specs(web_search: bool) -> Vec<ToolSpec> {
    published_specs_with(web_search, ShellRollout::Legacy, posix_host()).await
}

/// The host every non-Windows case runs against, stated rather than detected so
/// the surface under test does not depend on the machine running the suite.
fn posix_host() -> HostShells {
    HostShells {
        platform: Platform::Posix,
        git_bash: None,
        powershell: None,
    }
}

async fn published_specs_with(
    web_search: bool,
    rollout: ShellRollout,
    host: HostShells,
) -> Vec<ToolSpec> {
    let (_directory, registry) = published_registry_with(web_search, rollout, host).await;
    registry.list().expect("list")
}

/// The same surface, kept as a registry so a test can ask it what the two
/// configured filters publish rather than only what it holds.
///
/// The temporary directory is returned with it: the registered tools hold paths
/// under it, so dropping it early would pull the workspace out from under them.
async fn published_registry_with(
    web_search: bool,
    rollout: ShellRollout,
    host: HostShells,
) -> (tempfile::TempDir, ToolRegistry) {
    let directory = tempfile::tempdir().expect("tempdir");
    let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
    let review = Arc::new(ReviewManager::new(workspace.clone()));
    let policy = PermissionStore::default();
    policy
        .set_trust(
            directory.path(),
            TrustDecision::Trusted,
            TrustRootKind::Workspace,
        )
        .await
        .expect("trust");
    let registry = ToolRegistry::default();
    let access = web_search.then(|| WebSearchAccess {
        endpoint: WebSearchAccess::DEFAULT_ENDPOINT.to_owned(),
        api_key: SecretString::from("probe"),
    });
    let guard = ToolGuard::new(policy, Arc::new(RejectApproval));
    // The shell family reads its rollout off the session configuration, which
    // is what reference `_is_enabled_for_shell_rollout` reads.
    guard
        .config
        .set_managed_shell_tools(rollout == ShellRollout::Managed);
    BuiltinTools::new(directory.path(), access)
        .register(
            "session-1",
            // The census reads the published surface, never a skill body.
            SkillDiscovery::default(),
            &registry,
            &guard,
        )
        .expect("universal tools register");
    WorkspaceTools::new(workspace, review)
        .register(&registry, &guard)
        .expect("workspace tools register");
    ShellTools::with_host(directory.path().join("home"), host)
        .register("session-1", directory.path(), &registry, None, &guard)
        .expect("the shell family registers");
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    InteractiveSessionToolFactory {
        sender,
        plan_directory: Some(directory.path().to_path_buf()),
    }
    .register("session-1", &registry)
    .expect("interactive tools register");
    registry
        .register(task_spec(), Arc::new(UnreachableHandler))
        .expect("the subagent tool registers");
    (directory, registry)
}

/// Answers connector calls for the ordering case, which never places one.
struct UnreachableConnector;

impl vibe_core::integrations::ConnectorBackend for UnreachableConnector {
    fn call<'a>(
        &'a self,
        _connector_id: &'a str,
        _tool: &'a str,
        _arguments: Value,
        _max_response_bytes: usize,
    ) -> vibe_core::integrations::ConnectorFuture<'a> {
        Box::pin(async {
            Err(vibe_core::integrations::IntegrationError::Tool(
                "the ordering case never calls a connector".to_owned(),
            ))
        })
    }
}

/// The surface a session publishes carries the order its families registered
/// in, and a connector integrated afterwards lands behind all of them.
///
/// This case states no reference expectation and therefore needs no checkout:
/// the reference's own sequence comes from the `.py` import order of
/// `_iter_tool_classes`, which this port does not reproduce. What is held here
/// is the shape that order has, which is that the surface is published in the
/// session's registration order rather than in the name order the registry
/// stores under.
#[tokio::test]
async fn the_published_surface_carries_the_registration_order_not_the_name_order() {
    let (directory, registry) =
        published_registry_with(false, ShellRollout::Legacy, posix_host()).await;
    let builtins = registry
        .list()
        .expect("the registered surface")
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    let mut alphabetical = builtins.clone();
    alphabetical.sort();
    assert_ne!(
        builtins, alphabetical,
        "the surface is published by name, which is the order this port used to carry"
    );
    // `BuiltinTools` registers the universal tools, `WorkspaceTools` the file
    // family, `ShellTools` the shell, then the interactive tools and `task`.
    assert_eq!(
        &builtins[..7],
        [
            "todo",
            "skill",
            "web_fetch",
            "read_file",
            "grep",
            "edit",
            "write_file"
        ]
    );
    assert_eq!(builtins.last().map(String::as_str), Some("task"));

    let connectors = vibe_core::integrations::ConnectorRegistry::default();
    connectors
        .discover(
            vec![vibe_core::integrations::ConnectorDefinition {
                id: "drive-id".to_owned(),
                name: "Drive".to_owned(),
                base_url: url::Url::parse("https://connectors.example/drive")
                    .expect("connector URL"),
                auth_kind: vibe_core::integrations::ConnectorAuthKind::None,
                tools: vec![vibe_core::integrations::ConnectorTool {
                    name: "search".to_owned(),
                    description: "Search files".to_owned(),
                    input_schema: serde_json::json!({"type": "object"}),
                    output_schema: None,
                }],
            }],
            "credential",
            &url::Url::parse("https://connectors.example").expect("catalog URL"),
            0,
        )
        .await
        .expect("the connector is discovered");
    connectors
        .register_tools(
            &registry,
            Arc::new(UnreachableConnector),
            PermissionStore::default(),
            Arc::new(RejectApproval),
        )
        .expect("the connector tool registers");
    let published = registry
        .list()
        .expect("the registered surface")
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    assert_eq!(&published[..builtins.len()], builtins.as_slice());
    assert_eq!(
        &published[builtins.len()..],
        ["connector_Drive_search"],
        "a connector integrated after the builtins publishes behind them"
    );
    drop(directory);
}

/// Stands in for the live driver's subagent handler, which needs a provider and
/// a session store the oracle has no reason to build: the oracle reads
/// specifications and never invokes one.
struct UnreachableHandler;

impl ToolHandler for UnreachableHandler {
    fn invoke<'a>(
        &'a self,
        _invocation: &'a ToolInvocation,
        _output: ToolOutputSink,
    ) -> ToolHandlerFuture<'a> {
        Box::pin(async {
            Err(ToolError::Unavailable(
                "the oracle never invokes a tool".to_owned(),
            ))
        })
    }
}

/// Replaces every description with a sentinel so the diff reports a missing
/// description without ever comparing reference prose.
fn canonicalize(schema: &Value) -> Value {
    match schema {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    if key == "description" && value.is_string() {
                        (key.clone(), Value::String(DESCRIBED.to_owned()))
                    } else {
                        (key.clone(), canonicalize(value))
                    }
                })
                .collect::<Map<String, Value>>(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

#[derive(Debug)]
struct Divergence {
    pointer: String,
    expected: String,
    actual: String,
}

fn diff(expected: &Value, actual: &Value, pointer: &str, found: &mut Vec<Divergence>) {
    match (expected, actual) {
        (Value::Object(expected_object), Value::Object(actual_object)) => {
            for key in expected_object
                .keys()
                .chain(actual_object.keys())
                .collect::<BTreeSet<_>>()
            {
                match (expected_object.get(key), actual_object.get(key)) {
                    (Some(left), Some(right)) => {
                        diff(left, right, &format!("{pointer}/{key}"), found);
                    }
                    (left, right) => found.push(Divergence {
                        pointer: format!("{pointer}/{key}"),
                        expected: render(left),
                        actual: render(right),
                    }),
                }
            }
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                diff(left, right, &format!("{pointer}/{index}"), found);
            }
        }
        (left, right) if left != right => found.push(Divergence {
            pointer: pointer.to_owned(),
            expected: render(Some(left)),
            actual: render(Some(right)),
        }),
        _ => {}
    }
}

fn render(value: Option<&Value>) -> String {
    value.map_or_else(|| "<absent>".to_owned(), ToString::to_string)
}

fn baseline() -> Baseline {
    let raw = fs::read_to_string(repo_root().join(BASELINE_RELATIVE)).expect("baseline");
    serde_json::from_str(&raw).expect("the baseline parses")
}

#[tokio::test]
async fn the_published_tool_surface_matches_the_reference_except_for_the_recorded_gap() {
    let Some(corpus) = corpus() else {
        return;
    };
    let reference_names = corpus
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<BTreeSet<_>>();
    let published = published_specs(reference_names.contains("web_search")).await;

    let published_names = published
        .iter()
        .map(|spec| spec.name.clone())
        .collect::<BTreeSet<_>>();
    let missing = reference_names
        .difference(&published_names)
        .cloned()
        .collect::<BTreeSet<_>>();
    let extra = published_names
        .difference(&reference_names)
        .cloned()
        .collect::<BTreeSet<_>>();

    let mut schema_divergence: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut report = Vec::new();
    let mut conformant_schemas = 0;
    for tool in &corpus.tools {
        let Some(spec) = published.iter().find(|spec| spec.name == tool.name) else {
            continue;
        };
        let mut found = Vec::new();
        diff(
            &canonicalize(&tool.parameters),
            &canonicalize(&spec.input_schema),
            "",
            &mut found,
        );
        if found.is_empty() {
            conformant_schemas += 1;
            continue;
        }
        for divergence in &found {
            report.push(format!(
                "tool `{}` diverges at {}: expected {}, got {}",
                tool.name, divergence.pointer, divergence.expected, divergence.actual
            ));
        }
        schema_divergence.insert(
            tool.name.clone(),
            found
                .into_iter()
                .map(|divergence| divergence.pointer)
                .collect(),
        );
    }

    let matched_names = reference_names.len() - missing.len();
    println!(
        "tool surface: {matched_names}/{} names, {conformant_schemas}/{} schemas",
        reference_names.len(),
        reference_names.len()
    );
    for line in &report {
        println!("{line}");
    }
    if !extra.is_empty() {
        println!(
            "invented names: {}",
            extra.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }

    let baseline = baseline();
    assert_eq!(
        baseline.reference_commit, REFERENCE_COMMIT,
        "the baseline records another reference commit"
    );
    // The availability matrix, stated as a count: this host and this
    // configuration publish exactly as many names as the reference does.
    if baseline.missing_names.is_empty() && baseline.extra_names.is_empty() {
        assert_eq!(
            published_names.len(),
            reference_names.len(),
            "the published name count left the reference count"
        );
    }
    assert_eq!(baseline.platform, corpus.platform, "baseline platform");
    assert_eq!(
        (missing, extra, schema_divergence),
        (
            baseline.missing_names,
            baseline.extra_names,
            baseline.schema_divergence
        ),
        "the tool surface moved away from the recorded gap; regenerate {BASELINE_RELATIVE} \
         only when the change is the intended one"
    );
}

/// The managed rollout selects another `bash` and adds four session tools, a
/// surface the default corpus cannot answer for. It is captured separately and
/// diffed here under the same rules: names first, then canonicalized schemas.
#[tokio::test]
async fn the_managed_shell_surface_matches_the_reference_under_its_rollout() {
    let Some(corpus) = corpus() else {
        return;
    };
    let reference_names = corpus
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<BTreeSet<_>>();
    let published = published_specs_with(
        reference_names.contains("web_search"),
        ShellRollout::Managed,
        posix_host(),
    )
    .await
    .into_iter()
    .map(|spec| (spec.name.clone(), spec))
    .collect::<BTreeMap<_, _>>();

    let mut report = Vec::new();
    let mut missing = BTreeSet::new();
    let mut conformant = 0;
    for tool in &corpus.managed_tools {
        let Some(spec) = published.get(&tool.name) else {
            missing.insert(tool.name.clone());
            continue;
        };
        let mut found = Vec::new();
        diff(
            &canonicalize(&tool.parameters),
            &canonicalize(&spec.input_schema),
            "",
            &mut found,
        );
        if found.is_empty() {
            conformant += 1;
            continue;
        }
        for divergence in &found {
            report.push(format!(
                "tool `{}` diverges at {}: expected {}, got {}",
                tool.name, divergence.pointer, divergence.expected, divergence.actual
            ));
        }
    }
    println!(
        "managed shell surface: {}/{} names, {conformant}/{} schemas",
        corpus.managed_tools.len() - missing.len(),
        corpus.managed_tools.len(),
        corpus.managed_tools.len()
    );
    for line in &report {
        println!("{line}");
    }
    // The four session tools are what the rollout adds, and the epic's whole
    // point is that they are published rather than named.
    for name in [
        "bash",
        "bash_output",
        "bash_stdin",
        "bash_sessions",
        "bash_log_file",
    ] {
        assert!(
            published.contains_key(name),
            "the managed rollout must publish `{name}`"
        );
    }
    assert!(
        missing.is_empty(),
        "the managed rollout does not publish: {}",
        missing.into_iter().collect::<Vec<_>>().join(", ")
    );
    assert!(report.is_empty(), "{}", report.join("\n"));
}

/// The two Windows-only families, which no Linux surface can carry.
///
/// The corpus records what the reference classes declare, so this diffs the
/// same names and canonicalized schemas as the other two cases, once per
/// family, against a session registered on a stated Windows host. The two
/// families are mutually exclusive there — reference
/// `_powershell_treatment_available` withholds PowerShell wherever Git Bash
/// resolves — so each is measured under the host that publishes it.
#[tokio::test]
async fn the_windows_families_match_the_reference_on_a_windows_host() {
    let Some(corpus) = corpus() else {
        return;
    };
    assert!(
        !corpus.windows_tools.is_empty(),
        "the corpus carries no Windows family"
    );
    let hosts = [
        (
            "git_bash",
            HostShells {
                platform: Platform::Windows,
                git_bash: Some(PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")),
                powershell: Some(PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe")),
            },
        ),
        (
            "powershell",
            HostShells {
                platform: Platform::Windows,
                git_bash: None,
                powershell: Some(PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe")),
            },
        ),
    ];

    let mut report = Vec::new();
    let mut missing = BTreeSet::new();
    let mut conformant = 0;
    let mut expected = 0;
    for (family, host) in hosts {
        let published = published_specs_with(false, ShellRollout::Managed, host)
            .await
            .into_iter()
            .map(|spec| (spec.name.clone(), spec))
            .collect::<BTreeMap<_, _>>();
        for tool in corpus
            .windows_tools
            .iter()
            .filter(|tool| tool.family == family)
        {
            expected += 1;
            let Some(spec) = published.get(&tool.name) else {
                missing.insert(tool.name.clone());
                continue;
            };
            let mut found = Vec::new();
            diff(
                &canonicalize(&tool.parameters),
                &canonicalize(&spec.input_schema),
                "",
                &mut found,
            );
            if found.is_empty() {
                conformant += 1;
            }
            for divergence in &found {
                report.push(format!(
                    "tool `{}` diverges at {}: expected {}, got {}",
                    tool.name, divergence.pointer, divergence.expected, divergence.actual
                ));
            }
        }
        // The other family must stay absent: the host publishes one of them.
        let other = if family == "git_bash" {
            "powershell"
        } else {
            "git_bash"
        };
        for name in corpus
            .windows_tools
            .iter()
            .filter(|tool| tool.family == other)
            .map(|tool| &tool.name)
        {
            assert!(
                !published.contains_key(name),
                "a {family} host published `{name}`"
            );
        }
    }
    println!(
        "windows shell surface: {}/{expected} names, {conformant}/{expected} schemas",
        expected - missing.len()
    );
    for line in &report {
        println!("{line}");
    }
    assert!(
        missing.is_empty(),
        "a Windows host does not publish: {}",
        missing.into_iter().collect::<Vec<_>>().join(", ")
    );
    assert!(report.is_empty(), "{}", report.join("\n"));
}

/// The same ten names are absent from the surface this host publishes, which
/// is the other half of the platform gate and the reason they never reach the
/// recorded gap as invented names.
#[tokio::test]
async fn no_windows_family_name_reaches_a_posix_surface() {
    let Some(corpus) = corpus() else {
        return;
    };
    for rollout in [ShellRollout::Legacy, ShellRollout::Managed] {
        let published = published_specs_with(false, rollout, posix_host())
            .await
            .into_iter()
            .map(|spec| spec.name)
            .collect::<BTreeSet<_>>();
        for tool in &corpus.windows_tools {
            assert!(
                !published.contains(&tool.name),
                "a POSIX host published `{}`",
                tool.name
            );
        }
    }
}

/// Replays every committed argument fixture through the same coercion and
/// validation the registry runs before dispatch.
///
/// This runs unconditionally: the fixtures and the schemas they are validated
/// against are both committed, so CI reports a conformance count instead of
/// skipping. A fixture carries no reference prose, only a payload this
/// repository authored and the accept-or-reject verdict the reference gave it.
#[test]
fn arguments_the_reference_rejects_are_rejected_here_too() {
    let root = repo_root();
    let raw = fs::read_to_string(root.join(FIXTURES_RELATIVE)).expect("the fixtures are committed");
    let fixtures: Fixtures = serde_json::from_str(&raw).expect("the fixtures parse");
    assert_eq!(
        fixtures.schema_version, FIXTURES_SCHEMA_VERSION,
        "the fixture layout moved; regenerate with `--fixtures`"
    );
    assert_eq!(
        fixtures.reference_commit, REFERENCE_COMMIT,
        "the fixtures were captured from another commit than this build asserts"
    );

    let digest = digest();
    assert_eq!(
        digest.reference_commit, fixtures.reference_commit,
        "a fixture and the schema it is validated against must describe the same surface"
    );

    let mut checked = 0;
    let mut wrongly_accepted = Vec::new();
    let mut stricter_than_the_reference = Vec::new();
    let mut anonymous = Vec::new();
    let mut divergent_pointers = Vec::new();
    for fixture in &fixtures.fixtures {
        let Some(schema) = digest.tools.get(&fixture.tool) else {
            continue;
        };
        checked += 1;
        // The registry entry point, so the verdict this reports is the verdict a
        // real call gets rather than one a test-only path computed.
        let mut arguments = fixture.arguments.clone();
        let verdict: Result<(), ToolError> =
            coerce_and_validate(&fixture.tool, &mut arguments, schema);
        match (fixture.accepted, verdict) {
            (false, Ok(())) => wrongly_accepted.push(format!(
                "{}/{}: {}",
                fixture.tool, fixture.case, fixture.arguments
            )),
            (true, Err(error)) => stricter_than_the_reference
                .push(format!("{}/{}: {error}", fixture.tool, fixture.case)),
            (false, Err(error)) => {
                let Some(rejection) = &fixture.rejection else {
                    continue;
                };
                audit_rejection(
                    &fixture.tool,
                    &fixture.case,
                    &error,
                    rejection,
                    &mut anonymous,
                    &mut divergent_pointers,
                );
            }
            (true, Ok(())) => {}
        }
    }

    println!(
        "argument fixtures: {checked}/{} replayed with the reference verdict, {} wrongly accepted, \
         {} stricter than the reference, {} anonymous, {} with divergent pointers",
        fixtures.fixtures.len(),
        wrongly_accepted.len(),
        stricter_than_the_reference.len(),
        anonymous.len(),
        divergent_pointers.len()
    );
    assert!(
        wrongly_accepted.is_empty(),
        "arguments the reference rejects were accepted here: {}",
        wrongly_accepted.join("; ")
    );
    assert!(
        stricter_than_the_reference.is_empty(),
        "arguments the reference accepts were rejected here: {}",
        stricter_than_the_reference.join("; ")
    );
    assert!(
        anonymous.is_empty(),
        "a rejection must name the tool that refused the call: {}",
        anonymous.join("; ")
    );
    assert!(
        divergent_pointers.is_empty(),
        "a rejection must name the same arguments the reference named: {}",
        divergent_pointers.join("; ")
    );
    assert_eq!(
        checked,
        fixtures.fixtures.len(),
        "every committed fixture names a tool the digest publishes"
    );
    assert!(
        checked >= MINIMUM_FIXTURES,
        "the fixture set shrank to {checked}"
    );
}

/// Compares one rejection against the shape the reference gave the same payload.
///
/// Two things are held. The message must name the tool, which is what the
/// reference does by raising from the tool wrapper and what a model needs to
/// know which call it has to fix. And the places it objected to must be the
/// places the reference objected to, which is only measurable because the
/// capture renders a Pydantic location in this repository's own pointer
/// spelling.
fn audit_rejection(
    tool: &str,
    case: &str,
    error: &ToolError,
    rejection: &Rejection,
    anonymous: &mut Vec<String>,
    divergent_pointers: &mut Vec<String>,
) {
    let ToolError::InvalidArguments { violations, .. } = error else {
        anonymous.push(format!(
            "{tool}/{case}: {error} is not an argument rejection"
        ));
        return;
    };
    let message = error.to_string();
    if rejection.names_tool && !message.contains(tool) {
        anonymous.push(format!("{tool}/{case}: {message}"));
    }
    let reported = violations
        .iter()
        .map(|violation| violation.path.clone())
        .collect::<BTreeSet<_>>();
    let expected = rejection.pointers.iter().cloned().collect::<BTreeSet<_>>();
    if reported != expected {
        divergent_pointers.push(format!(
            "{tool}/{case}: the reference {} named {expected:?}, this port named {reported:?}",
            rejection.error
        ));
    }
}

/// Every pointer of a call breaking several arguments at once, not only the
/// first.
///
/// The fixture set breaks one argument per payload, so it cannot answer this:
/// a validator that stopped at the first violation would replay all 92 of them
/// cleanly. These probes are the measurement that separates the two, and the
/// reference's answer to each is a pointer set with more than one entry.
#[test]
fn a_rejection_names_every_argument_the_reference_names() {
    let root = repo_root();
    let raw = fs::read_to_string(root.join(FIXTURES_RELATIVE)).expect("the fixtures are committed");
    let fixtures: Fixtures = serde_json::from_str(&raw).expect("the fixtures parse");
    let digest = digest();

    let mut checked = 0;
    let mut anonymous = Vec::new();
    let mut divergent_pointers = Vec::new();
    let mut accepted = Vec::new();
    for probe in &fixtures.multi_argument {
        let Some(schema) = digest.tools.get(&probe.tool) else {
            continue;
        };
        assert!(
            probe.rejection.pointers.len() > 1,
            "{}/{} is not a multi-argument probe",
            probe.tool,
            probe.case
        );
        checked += 1;
        let mut arguments = probe.arguments.clone();
        match coerce_and_validate(&probe.tool, &mut arguments, schema) {
            Ok(()) => accepted.push(format!("{}/{}", probe.tool, probe.case)),
            Err(error) => audit_rejection(
                &probe.tool,
                &probe.case,
                &error,
                &probe.rejection,
                &mut anonymous,
                &mut divergent_pointers,
            ),
        }
    }

    println!(
        "multi-argument probes: {checked}/{} replayed, {} accepted, {} anonymous, {} with \
         divergent pointers",
        fixtures.multi_argument.len(),
        accepted.len(),
        anonymous.len(),
        divergent_pointers.len()
    );
    assert!(
        accepted.is_empty(),
        "payloads the reference rejects were accepted here: {}",
        accepted.join("; ")
    );
    assert!(
        anonymous.is_empty(),
        "a rejection must name the tool that refused the call: {}",
        anonymous.join("; ")
    );
    assert!(
        divergent_pointers.is_empty(),
        "a rejection must name every argument the reference named: {}",
        divergent_pointers.join("; ")
    );
    assert!(
        checked >= MINIMUM_MULTI_ARGUMENT_PROBES,
        "the multi-argument probe set shrank to {checked}"
    );
}

/// Replays the two configured filters against the names the reference publishes
/// under the same pair.
///
/// The gate is decided by the list an operator wrote, not by what it compiled
/// to: a list of nothing but blanks or of one uncompilable expression narrows
/// the surface to nothing, while an absent list publishes everything. A filter
/// reading its compiled patterns gets those two cases backwards and fails open,
/// which is the regression this holds shut.
#[tokio::test]
async fn the_configured_filters_publish_what_the_reference_publishes() {
    let root = repo_root();
    let raw = fs::read_to_string(root.join(GATES_RELATIVE)).expect("the gate corpus is committed");
    let gates: Gates = serde_json::from_str(&raw).expect("the gate corpus parses");
    assert_eq!(
        gates.schema_version, GATES_SCHEMA_VERSION,
        "the gate layout moved; regenerate with `--gates`"
    );
    assert_eq!(
        gates.reference_commit, REFERENCE_COMMIT,
        "the gates were captured from another commit than this build asserts"
    );

    // The surface itself still diverges from the reference's, which row 4 of the
    // PRD records by name. What is compared here is the gate, so the comparison
    // runs over the names both sides publish with neither list written.
    let unfiltered = gates
        .gates
        .iter()
        .find(|gate| gate.enabled_tools.is_empty() && gate.disabled_tools.is_empty())
        .expect("the corpus records the unfiltered surface");
    let (_directory, registry) =
        published_registry_with(false, ShellRollout::Legacy, posix_host()).await;
    let here = registry
        .available(None, &NameFilter::default())
        .expect("the unfiltered surface")
        .into_iter()
        .map(|spec| spec.name)
        .collect::<BTreeSet<_>>();
    let shared = unfiltered
        .names
        .iter()
        .filter(|name| here.contains(*name))
        .cloned()
        .collect::<BTreeSet<_>>();

    let mut divergent = Vec::new();
    for gate in &gates.gates {
        // An absent list is the only state that publishes everything, so the
        // gate reads whether the operator wrote one rather than what it matched.
        let enabled =
            (!gate.enabled_tools.is_empty()).then(|| NameFilter::new(&gate.enabled_tools));
        let disabled = NameFilter::new(&gate.disabled_tools);
        let published = registry
            .available(enabled.as_ref(), &disabled)
            .expect("the filtered surface")
            .into_iter()
            .map(|spec| spec.name)
            .filter(|name| shared.contains(name))
            .collect::<BTreeSet<_>>();
        let expected = gate
            .names
            .iter()
            .filter(|name| shared.contains(*name))
            .cloned()
            .collect::<BTreeSet<_>>();
        if published != expected {
            divergent.push(format!(
                "{}: reference published {expected:?}, this port published {published:?}",
                gate.case
            ));
        }
    }

    println!(
        "filter gates: {}/{} replayed over {} shared names",
        gates.gates.len() - divergent.len(),
        gates.gates.len(),
        shared.len()
    );
    assert!(
        !shared.is_empty(),
        "the two surfaces share no name, so the gate comparison would be vacuous"
    );
    assert!(
        divergent.is_empty(),
        "the configured filters diverge from the reference: {}",
        divergent.join("; ")
    );
}

/// The captured corpus and the committed fixtures must not drift apart: when a
/// checkout is present the corpus is recaptured on every run, so a verdict that
/// changed upstream is caught here rather than at the next regeneration.
#[tokio::test]
async fn the_committed_fixtures_match_a_freshly_captured_corpus() {
    let Some(corpus) = corpus() else {
        return;
    };
    let root = repo_root();
    let raw = fs::read_to_string(root.join(FIXTURES_RELATIVE)).expect("the fixtures are committed");
    let fixtures: Fixtures = serde_json::from_str(&raw).expect("the fixtures parse");

    let committed = fixtures
        .fixtures
        .iter()
        .map(|fixture| ((&fixture.tool, &fixture.case), fixture.accepted))
        .collect::<BTreeMap<_, _>>();
    let captured = corpus
        .fixtures
        .iter()
        .map(|fixture| ((&fixture.tool, &fixture.case), fixture.accepted))
        .collect::<BTreeMap<_, _>>();

    let divergent = captured
        .iter()
        .filter(|((tool, case), accepted)| {
            committed
                .get(&(*tool, *case))
                .is_none_or(|held| held != *accepted)
        })
        .map(|((tool, case), accepted)| format!("{tool}/{case}: reference says {accepted}"))
        .collect::<Vec<_>>();

    assert!(
        divergent.is_empty(),
        "the committed fixtures disagree with the pinned reference; regenerate them with \
         `scripts/parity/tool_surface.py --fixtures`: {}",
        divergent.join("; ")
    );
    assert_eq!(committed.len(), captured.len());
}

#[test]
fn a_corpus_from_another_commit_platform_or_layout_skips_with_an_explicit_message() {
    let moved = skip_reason_for(
        CORPUS_SCHEMA_VERSION,
        "0123456789abcdef0123456789abcdef01234567",
        "linux",
        "linux",
    )
    .expect("a corpus from another commit cannot answer");
    assert!(moved.contains(REFERENCE_COMMIT), "{moved}");
    assert!(moved.contains("0123456789abcdef"), "{moved}");

    let elsewhere = skip_reason_for(CORPUS_SCHEMA_VERSION, REFERENCE_COMMIT, "windows", "linux")
        .expect("a corpus from another platform cannot answer");
    assert!(
        elsewhere.contains("windows") && elsewhere.contains("linux"),
        "{elsewhere}"
    );

    // A corpus captured before the managed surface was recorded carries no
    // answer for it, so it is regenerated rather than partly believed.
    let stale = skip_reason_for(1, REFERENCE_COMMIT, "linux", "linux")
        .expect("an older corpus layout cannot answer");
    assert!(stale.contains(CAPTURE_SCRIPT), "{stale}");

    assert!(
        skip_reason_for(
            CORPUS_SCHEMA_VERSION,
            REFERENCE_COMMIT,
            running_platform(),
            running_platform()
        )
        .is_none()
    );
}

fn digest() -> Digest {
    let raw = fs::read_to_string(repo_root().join(DIGEST_RELATIVE)).expect("digest");
    let digest: Digest = serde_json::from_str(&raw).expect("the digest parses");
    assert_eq!(
        digest.schema_version, DIGEST_SCHEMA_VERSION,
        "the digest is at another layout; regenerate it with {CAPTURE_SCRIPT} --digest"
    );
    assert_eq!(
        digest.reference_commit, REFERENCE_COMMIT,
        "the digest records another reference commit"
    );
    digest
}

/// Diffs one published surface against its canonical expectation, returning the
/// number of conformant schemas and every divergence as a reportable line.
fn diff_surface(
    label: &str,
    expected: &BTreeMap<String, Value>,
    published: &BTreeMap<String, ToolSpec>,
) -> (usize, Vec<String>) {
    let mut report = Vec::new();
    let mut conformant = 0_usize;
    for (name, parameters) in expected {
        let Some(spec) = published.get(name) else {
            report.push(format!(
                "{label}: `{name}` is in the digest and is not published"
            ));
            continue;
        };
        let mut found = Vec::new();
        diff(
            &canonicalize(parameters),
            &canonicalize(&spec.input_schema),
            "",
            &mut found,
        );
        if found.is_empty() {
            conformant = conformant.saturating_add(1);
            continue;
        }
        for divergence in found {
            report.push(format!(
                "{label}: tool `{name}` diverges at {}: expected {}, got {}",
                divergence.pointer, divergence.expected, divergence.actual
            ));
        }
    }
    (conformant, report)
}

async fn published_by_name(
    web_search: bool,
    rollout: ShellRollout,
    host: HostShells,
) -> BTreeMap<String, ToolSpec> {
    published_specs_with(web_search, rollout, host)
        .await
        .into_iter()
        .map(|spec| (spec.name.clone(), spec))
        .collect()
}

/// The conformance gate CI actually runs.
///
/// Unlike the oracle tests above it never skips: the digest is committed, so a
/// pull request that adds a tool, drops one, or edits a published schema fails
/// here on a machine that has no reference checkout at all. Regenerating the
/// digest is a deliberate act (`{CAPTURE_SCRIPT} --digest`) and shows up in the
/// diff as the intended change.
#[tokio::test]
async fn the_published_surface_matches_the_committed_digest() {
    let digest = digest();
    let web_search = digest.tools.contains_key("web_search");
    let mut report = Vec::new();
    let mut conformant = 0;
    let mut expected = digest.tools.len() + digest.managed_tools.len();

    for (label, canonical, published) in [
        (
            "tool surface",
            &digest.tools,
            published_by_name(web_search, ShellRollout::Legacy, posix_host()).await,
        ),
        (
            "managed shell surface",
            &digest.managed_tools,
            published_by_name(web_search, ShellRollout::Managed, posix_host()).await,
        ),
    ] {
        let (matched, mut lines) = diff_surface(label, canonical, &published);
        conformant += matched;
        for name in published.keys() {
            if !canonical.contains_key(name) {
                lines.push(format!(
                    "{label}: `{name}` is published and is not in the digest"
                ));
            }
        }
        report.append(&mut lines);
    }

    // The two Windows families never share a host: Git Bash withholds
    // PowerShell wherever it resolves, so each is measured under the host that
    // publishes it.
    for (family, host) in windows_hosts() {
        let Some(canonical) = digest.windows_tools.get(family) else {
            report.push(format!("the digest carries no `{family}` family"));
            continue;
        };
        expected += canonical.len();
        let published = published_by_name(false, ShellRollout::Managed, host).await;
        let (matched, mut lines) = diff_surface(family, canonical, &published);
        conformant += matched;
        // A Windows-only name is invisible to the two surfaces above, so the
        // "published and not in the digest" half is checked here too: without
        // it a sixth family tool would merge with no corpus entry at all.
        for name in published.keys() {
            if !canonical.contains_key(name)
                && !digest.tools.contains_key(name)
                && !digest.managed_tools.contains_key(name)
            {
                lines.push(format!(
                    "{family}: `{name}` is published and is in no digest surface"
                ));
            }
        }
        report.append(&mut lines);
    }

    println!(
        "tool-surface conformance: {conformant}/{expected} schemas match the committed digest at \
         {}",
        &digest.reference_commit[..12]
    );
    assert!(report.is_empty(), "{}", report.join("\n"));
    assert_eq!(
        conformant, expected,
        "every digest entry must be published and conformant"
    );
}

/// The two Windows hosts, each publishing exactly one family.
fn windows_hosts() -> [(&'static str, HostShells); 2] {
    [
        (
            "git_bash",
            HostShells {
                platform: Platform::Windows,
                git_bash: Some(PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")),
                powershell: Some(PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe")),
            },
        ),
        (
            "powershell",
            HostShells {
                platform: Platform::Windows,
                git_bash: None,
                powershell: Some(PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe")),
            },
        ),
    ]
}

/// The digest is only a conformance target while it still says what the
/// reference says, so a machine that can reach the oracle re-derives it and
/// refuses a digest that drifted.
#[test]
fn the_committed_digest_still_says_what_the_reference_says() {
    let Some(corpus) = corpus() else {
        return;
    };
    let digest = digest();
    assert_eq!(digest.platform, corpus.platform, "digest platform");
    let derived = corpus
        .tools
        .iter()
        .map(|tool| (tool.name.clone(), canonicalize(&tool.parameters)))
        .collect::<BTreeMap<_, _>>();
    let recorded = digest
        .tools
        .iter()
        .map(|(name, parameters)| (name.clone(), canonicalize(parameters)))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        derived, recorded,
        "the digest drifted from the reference; regenerate it with {CAPTURE_SCRIPT} --digest"
    );
    let derived_managed = corpus
        .managed_tools
        .iter()
        .map(|tool| (tool.name.clone(), canonicalize(&tool.parameters)))
        .collect::<BTreeMap<_, _>>();
    let recorded_managed = digest
        .managed_tools
        .iter()
        .map(|(name, parameters)| (name.clone(), canonicalize(parameters)))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(derived_managed, recorded_managed, "managed surface digest");
    for tool in &corpus.windows_tools {
        assert_eq!(
            digest
                .windows_tools
                .get(&tool.family)
                .and_then(|family| family.get(&tool.name))
                .map(canonicalize),
            Some(canonicalize(&tool.parameters)),
            "`{}` diverges from the reference in the digest",
            tool.name
        );
    }
}

/// What makes the digest committable under `NOTICE`: it records that a
/// description exists and never what it says.
#[test]
fn the_digest_carries_no_reference_prose() {
    let digest = digest();
    let surfaces = digest
        .tools
        .values()
        .chain(digest.managed_tools.values())
        .chain(digest.windows_tools.values().flat_map(BTreeMap::values));
    for schema in surfaces {
        let mut offenders = Vec::new();
        collect_descriptions(schema, "", &mut offenders);
        assert!(
            offenders.is_empty(),
            "the digest carries reference prose at {}",
            offenders.join(", ")
        );
    }
}

/// Every description in `schema` that is not the sentinel, by JSON pointer.
fn collect_descriptions(schema: &Value, pointer: &str, offenders: &mut Vec<String>) {
    match schema {
        Value::Object(object) => {
            for (key, value) in object {
                let child = format!("{pointer}/{key}");
                if key == "description" && value.as_str().is_some_and(|text| text != DESCRIBED) {
                    offenders.push(child);
                } else {
                    collect_descriptions(value, &child, offenders);
                }
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_descriptions(item, &format!("{pointer}/{index}"), offenders);
            }
        }
        _ => {}
    }
}

#[test]
fn the_corpus_stays_out_of_the_repository_and_the_baseline_carries_no_prose() {
    let ignored = Command::new("git")
        .args(["check-ignore", CORPUS_RELATIVE])
        .current_dir(repo_root())
        .output()
        .expect("git check-ignore runs");
    assert!(
        ignored.status.success(),
        "{CORPUS_RELATIVE} must stay out of the repository: it holds reference description text"
    );
    for (tool, pointers) in baseline().schema_divergence {
        for pointer in pointers {
            assert!(
                pointer.starts_with('/'),
                "the baseline records JSON pointers only, `{tool}` carries `{pointer}`"
            );
        }
    }
}
