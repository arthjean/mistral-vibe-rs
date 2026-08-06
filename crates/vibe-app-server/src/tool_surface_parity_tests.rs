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
use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionStore,
    TrustDecision, TrustRootKind,
};
use vibe_core::tools::builtins::{BuiltinTools, WebSearchAccess};
use vibe_core::tools::{
    ToolError, ToolHandler, ToolHandlerFuture, ToolInvocation, ToolOutputSink, ToolRegistry,
    ToolSpec, validate_arguments,
};
use vibe_core::workspace::{ReviewManager, Workspace, WorkspaceTools};

use vibe_core::platform::Platform;
use vibe_core::tools::shell::{HostShells, ShellRollout, ShellTools};

use crate::client::{InteractiveSessionToolFactory, task_spec};
use crate::server::SessionToolFactory;

/// The reference commit the corpus is captured from. A checkout at any other
/// revision is not an oracle for this corpus.
const REFERENCE_COMMIT: &str = "68ff32e6a92e80a874c8153312f0aa8ae4955477";
const REFERENCE_ROOT: &str = "/home/arthur/dev/mistral-vibe";
const CORPUS_RELATIVE: &str = ".parity/tool-surface-corpus.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 3;
const CAPTURE_SCRIPT: &str = "scripts/parity/tool_surface.py";
const BASELINE_RELATIVE: &str = "crates/vibe-app-server/tests/tool-surface/baseline.json";
/// The committed conformance target, which is what CI diffs against: it has no
/// pinned Python checkout, so the corpus cannot be recaptured there.
const DIGEST_RELATIVE: &str = "crates/vibe-app-server/tests/tool-surface/digest.json";
/// The digest layout this runner reads, matching `DIGEST_SCHEMA_VERSION` in the
/// capture script.
const DIGEST_SCHEMA_VERSION: u32 = 1;
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

/// The pinned checkout, or `None` when this machine cannot act as the oracle.
fn pinned_reference() -> Option<PathBuf> {
    let root = PathBuf::from(REFERENCE_ROOT);
    if !root.join(".venv/bin/python").is_file() {
        return None;
    }
    let revision = Command::new("git")
        .args(["-C", REFERENCE_ROOT, "rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !revision.status.success() {
        return None;
    }
    let head = String::from_utf8(revision.stdout).ok()?;
    if head.trim() != REFERENCE_COMMIT {
        eprintln!(
            "the tool-surface oracle needs {REFERENCE_ROOT} at {REFERENCE_COMMIT}, found {}",
            head.trim()
        );
        return None;
    }
    Some(root)
}

/// Recaptures the corpus when the pinned checkout is present, otherwise falls
/// back to a corpus captured earlier from that same commit.
fn corpus() -> Option<Corpus> {
    let root = repo_root();
    let path = root.join(CORPUS_RELATIVE);
    if let Some(reference) = pinned_reference() {
        let capture = Command::new(reference.join(".venv/bin/python"))
            .arg(root.join(CAPTURE_SCRIPT))
            .args(["--reference", REFERENCE_ROOT])
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
            "skipping the tool-surface oracle: no corpus at {} and no pinned checkout at \
             {REFERENCE_ROOT} on {REFERENCE_COMMIT}",
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
        model: WebSearchAccess::DEFAULT_MODEL.to_owned(),
        api_key: SecretString::from("probe"),
    });
    BuiltinTools::new(directory.path(), access)
        .register(
            "session-1",
            directory.path(),
            true,
            &registry,
            policy.clone(),
            Arc::new(RejectApproval),
        )
        .expect("universal tools register");
    WorkspaceTools::new(workspace, review)
        .register(&registry, policy.clone(), Arc::new(RejectApproval))
        .expect("workspace tools register");
    ShellTools::with_host(directory.path().join("home"), rollout, host)
        .register(
            "session-1",
            directory.path(),
            &registry,
            policy,
            Arc::new(RejectApproval),
            None,
        )
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
    registry.list().expect("list")
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

#[tokio::test]
async fn arguments_the_reference_rejects_are_rejected_here_too() {
    let Some(corpus) = corpus() else {
        return;
    };
    let schemas = corpus
        .tools
        .iter()
        .map(|tool| (tool.name.as_str(), &tool.parameters))
        .collect::<BTreeMap<_, _>>();

    let mut checked = 0;
    let mut wrongly_accepted = Vec::new();
    let mut stricter_than_pydantic = Vec::new();
    for fixture in &corpus.fixtures {
        let Some(schema) = schemas.get(fixture.tool.as_str()) else {
            continue;
        };
        checked += 1;
        let verdict: Result<(), ToolError> = validate_arguments(&fixture.arguments, schema);
        match (fixture.accepted, verdict) {
            (false, Ok(())) => wrongly_accepted.push(format!(
                "{}/{}: {}",
                fixture.tool, fixture.case, fixture.arguments
            )),
            (true, Err(error)) => {
                stricter_than_pydantic.push(format!("{}/{}: {error}", fixture.tool, fixture.case));
            }
            _ => {}
        }
    }
    println!(
        "argument fixtures: {checked} replayed, {} accepted here and rejected there, {} rejected \
         here and accepted there",
        wrongly_accepted.len(),
        stricter_than_pydantic.len()
    );
    for line in &stricter_than_pydantic {
        println!("stricter than the reference: {line}");
    }
    assert!(
        wrongly_accepted.is_empty(),
        "arguments the reference rejects were accepted here: {}",
        wrongly_accepted.join("; ")
    );
    assert!(checked > 0, "the corpus carries no argument fixtures");
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
