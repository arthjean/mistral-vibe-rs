//! The permission model: the four scopes, the rules they are stored under, and
//! the file-tool chain that produces them.
//!
//! The differential half lives in `permission_parity_tests`, which replays the
//! committed capture of the reference vocabulary. These cases cover what the
//! capture cannot reach: the composition with this port's trust roots, the
//! atomicity of a revocation, and the wiring from a registered tool to a
//! decision.

use super::*;
use crate::scratchpad::init_scratchpad;
use crate::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolExecutionOutput, ToolPresentationKind,
    ToolRegistry, ToolSource, ToolSpec,
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::tempdir;
use tokio::sync::Notify;

struct FixedApproval(ApprovalDecision);

impl ApprovalAgent for FixedApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async move { Ok(self.0) })
    }
}

/// An approval agent recording what it was asked to approve.
#[derive(Default)]
struct RecordingApproval {
    decision: Option<ApprovalDecision>,
    seen: StdRwLock<Vec<ApprovalRequest>>,
}

impl RecordingApproval {
    fn approving(decision: ApprovalDecision) -> Self {
        Self {
            decision: Some(decision),
            seen: StdRwLock::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ApprovalRequest> {
        self.seen.read().expect("recorded requests").clone()
    }
}

impl ApprovalAgent for RecordingApproval {
    fn request<'a>(&'a self, request: ApprovalRequest) -> ApprovalFuture<'a> {
        self.seen.write().expect("record").push(request);
        let decision = self.decision.unwrap_or(ApprovalDecision::Deny);
        Box::pin(async move { Ok(decision) })
    }
}

/// A store trusting `root`, which is what a session opening a workspace does.
async fn store_trusting(root: &Path) -> PermissionStore {
    let store = PermissionStore::default();
    store
        .set_trust(root, TrustDecision::Trusted, TrustRootKind::Workspace)
        .await
        .expect("trust");
    store
}

// --------------------------------------------------------------------------
// US-105: the four scopes and the four-field requirement
// --------------------------------------------------------------------------

/// A requirement crosses the wire as exactly `scope`, `invocationPattern`,
/// `sessionPattern` and `label`, and refuses anything else.
#[test]
fn a_requirement_serializes_as_the_four_camel_cased_fields_and_nothing_else() {
    let requirement = PermissionRequirement::outside_directory("/outside/*");
    let wire = serde_json::to_value(&requirement).expect("serialize");
    assert_eq!(
        wire,
        json!({
            "scope": "outside_directory",
            "invocationPattern": "/outside/*",
            "sessionPattern": "/outside/*",
            "label": "outside workdir (/outside/*)",
        })
    );

    let mut surplus = wire.clone();
    surplus["reason"] = json!("extra");
    assert!(
        serde_json::from_value::<PermissionRequirement>(surplus).is_err(),
        "the requirement forbids a surplus field, as the reference model does"
    );
    assert_eq!(
        serde_json::from_value::<PermissionRequirement>(wire).expect("round trip"),
        requirement
    );
}

/// Only the four reference scopes exist, and each one spells its wire value.
#[test]
fn the_scope_vocabulary_is_exactly_the_four_reference_values() {
    let spoken = PermissionScope::ALL
        .into_iter()
        .map(|scope| serde_json::to_value(scope).expect("serialize"))
        .collect::<Vec<_>>();
    assert_eq!(
        spoken,
        [
            json!("command_pattern"),
            json!("outside_directory"),
            json!("file_pattern"),
            json!("url_pattern"),
        ]
    );
    for invented in ["mcp", "destructive", "read", "write", "shell", "network"] {
        assert!(
            serde_json::from_value::<PermissionScope>(json!(invented)).is_err(),
            "`{invented}` is not a scope the Python client can read"
        );
    }
}

/// A path outside the working directory is labeled by the parent joined
/// with `*`, which is what the operator approves for the session.
#[tokio::test]
async fn an_outside_path_is_named_by_its_parent_directory() {
    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let store = store_trusting(workspace.path()).await;
    let escaping = outside.path().join("secret.txt");
    std::fs::write(&escaping, "secret").expect("write");

    let resolution = store
        .resolve(
            "read_file",
            &PermissionContext::deferred().over_paths(vec![escaping]),
        )
        .await
        .expect("resolution");

    let glob = format!(
        "{}/*",
        outside.path().canonicalize().expect("canonical").display()
    );
    assert_eq!(resolution.mode, PermissionMode::Ask);
    assert_eq!(
        resolution.required_permissions,
        [PermissionRequirement::outside_directory(&glob)]
    );
    assert_eq!(
        resolution.required_permissions[0].label,
        format!("outside workdir ({glob})")
    );
}

/// US-105: an approval written under the retired vocabulary cannot be read as
/// one of the new rules, so a session resumed after the change re-asks rather
/// than reinterpreting a scope string it no longer understands.
#[test]
fn a_rule_written_under_the_retired_vocabulary_is_refused_rather_than_reinterpreted() {
    for retired in [
        json!({"tool": "read_file", "scope": "read /workspace/secret", "mode": "ALWAYS", "rationale": "session approval"}),
        json!({"tool": "shell", "scope": "shell git *", "mode": "ALWAYS", "rationale": "session approval"}),
        json!({"tool": "server_tool", "scope": "mcp server/tool", "mode": "ALWAYS", "rationale": "permanent approval"}),
    ] {
        assert!(
            serde_json::from_value::<PermissionRule>(retired.clone()).is_err(),
            "{retired} still parses as a rule"
        );
    }
}

// --------------------------------------------------------------------------
// US-107: coverage by a stored rule
// --------------------------------------------------------------------------

/// A stored rule covers a later invocation of the same tool and scope whose
/// invocation pattern matches, with the trailing arguments optional.
#[tokio::test]
async fn a_stored_rule_covers_a_matching_later_invocation() {
    let store = PermissionStore::default();
    store
        .add_rule(PermissionRequirement::command("git status").approved_rule("bash", "session"))
        .await;

    for covered in ["git status", "git status --short"] {
        let resolution = store
            .resolve(
                "bash",
                &PermissionContext::asking(vec![PermissionRequirement::command(covered)]),
            )
            .await
            .expect("resolution");
        assert_eq!(
            resolution.mode,
            PermissionMode::Always,
            "`{covered}` is covered by `git status *`"
        );
    }

    // A different subcommand reduces to another session pattern and is asked.
    let uncovered = store
        .resolve(
            "bash",
            &PermissionContext::asking(vec![PermissionRequirement::command("git push")]),
        )
        .await
        .expect("resolution");
    assert_eq!(uncovered.mode, PermissionMode::Ask);
    assert_eq!(uncovered.required_permissions.len(), 1);

    // A rule belongs to one tool: an identically shaped requirement raised by
    // another tool is not covered.
    let other_tool = store
        .resolve(
            "powershell",
            &PermissionContext::asking(vec![PermissionRequirement::command("git status")]),
        )
        .await
        .expect("resolution");
    assert_eq!(other_tool.mode, PermissionMode::Ask);
}

/// A rule answers for its own scope only, so an approval for a host never
/// covers a command that happens to spell the same text.
#[tokio::test]
async fn a_rule_answers_for_its_own_scope_only() {
    let store = PermissionStore::default();
    store
        .add_rule(
            PermissionRequirement::url_domain("example.com").approved_rule("web_fetch", "session"),
        )
        .await;

    let same_scope = store
        .resolve(
            "web_fetch",
            &PermissionContext::asking(vec![PermissionRequirement::url_domain("example.com")]),
        )
        .await
        .expect("resolution");
    assert_eq!(same_scope.mode, PermissionMode::Always);

    let other_scope = store
        .resolve(
            "web_fetch",
            &PermissionContext::asking(vec![PermissionRequirement::exact_command("example.com")]),
        )
        .await
        .expect("resolution");
    assert_eq!(other_scope.mode, PermissionMode::Ask);
}

/// Only the uncovered requirements reach the prompt.
#[tokio::test]
async fn the_prompt_lists_exactly_the_uncovered_requirements() {
    let store = PermissionStore::default();
    store
        .add_rule(PermissionRequirement::command("ls -la").approved_rule("bash", "session"))
        .await;
    let approval = Arc::new(RecordingApproval::approving(ApprovalDecision::ApproveOnce));

    store
        .authorize(
            "bash",
            Value::Null,
            PermissionContext::asking(vec![
                PermissionRequirement::command("ls -la"),
                PermissionRequirement::command("cat notes.txt"),
            ]),
            approval.as_ref(),
        )
        .await
        .expect("the operator approves the rest");

    let requests = approval.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .requirements
            .iter()
            .map(|requirement| requirement.invocation_pattern.as_str())
            .collect::<Vec<_>>(),
        ["cat notes.txt"],
        "the already-approved segment is not asked again"
    );
}

/// An approval for the session stores one rule per uncovered requirement, and a
/// permanent one also writes the tool's configured allowlist so the next
/// session starts already covered.
#[tokio::test]
async fn a_permanent_approval_writes_the_allowlist_a_session_one_does_not() {
    let written = Arc::new(StdRwLock::new(Vec::<(String, Vec<String>)>::new()));
    let recorder = written.clone();
    let store = PermissionStore::default().with_allowlist_persistence(Arc::new(
        move |tool: &str, patterns: &[String]| {
            recorder
                .write()
                .expect("record")
                .push((tool.to_owned(), patterns.to_vec()));
            Ok(())
        },
    ));

    store
        .authorize(
            "bash",
            Value::Null,
            PermissionContext::asking(vec![PermissionRequirement::command("npm run build")]),
            &FixedApproval(ApprovalDecision::ApproveForSession),
        )
        .await
        .expect("session approval");
    assert!(
        written.read().expect("written").is_empty(),
        "a session approval writes nothing to disk"
    );

    store
        .authorize(
            "bash",
            Value::Null,
            PermissionContext::asking(vec![PermissionRequirement::command("cargo test")]),
            &FixedApproval(ApprovalDecision::ApprovePermanently),
        )
        .await
        .expect("permanent approval");
    assert_eq!(
        *written.read().expect("written"),
        [("bash".to_owned(), vec!["cargo test *".to_owned()])],
        "a permanent approval extends the tool's configured allowlist"
    );

    // Both are covered for the rest of this session.
    for covered in ["npm run build --watch", "cargo test --lib"] {
        assert_eq!(
            store
                .resolve(
                    "bash",
                    &PermissionContext::asking(vec![PermissionRequirement::command(covered)])
                )
                .await
                .expect("resolution")
                .mode,
            PermissionMode::Always,
            "`{covered}`"
        );
    }
}

/// A persistence failure keeps the approval for the session and is reported
/// rather than failing the call the operator just approved.
#[tokio::test]
async fn a_failed_permanent_write_is_reported_without_failing_the_call() {
    let store = PermissionStore::default().with_allowlist_persistence(Arc::new(
        |_tool: &str, _patterns: &[String]| Err("the configuration file is read-only".to_owned()),
    ));

    store
        .authorize(
            "bash",
            Value::Null,
            PermissionContext::asking(vec![PermissionRequirement::command("cargo test")]),
            &FixedApproval(ApprovalDecision::ApprovePermanently),
        )
        .await
        .expect("the call still runs");

    let diagnostics = store.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert!(
        diagnostics[0].contains("read-only") && diagnostics[0].contains("bash"),
        "{}",
        diagnostics[0]
    );
}

// --------------------------------------------------------------------------
// US-108: the file-tool chain
// --------------------------------------------------------------------------

fn settings_for(tool: &str) -> SharedToolConfig {
    ToolConfigResolver::new().view(tool)
}

/// The scratchpad is the runtime's own capability: a path inside it is granted
/// before any list is read.
#[test]
fn a_scratchpad_path_is_granted_before_any_list_is_consulted() {
    let scratchpad = init_scratchpad("policy-scratchpad-probe").expect("scratchpad");
    let mut settings = settings_for("read_file");
    // Even a denylisted name is granted inside the scratchpad, which is what
    // "without consulting any list" means.
    settings.denylist = vec!["*".to_owned()];

    let context = resolve_file_tool_permission(
        &scratchpad.join(".env"),
        "read_file",
        &settings,
        Some(&scratchpad),
    );

    assert_eq!(context.permission, Some(PermissionMode::Always));
    assert!(context.requirements.is_empty());
    assert!(
        context.paths.is_empty(),
        "no boundary check follows a grant"
    );
    crate::scratchpad::cleanup_scratchpad(Some(&scratchpad));
}

/// The denylist is consulted first, and the allowlist grants what it does not
/// close.
#[test]
fn the_denylist_is_consulted_before_the_allowlist() {
    let workspace = tempdir().expect("workspace");
    let path = workspace.path().join("notes.txt");
    std::fs::write(&path, "notes").expect("write");
    let mut settings = settings_for("read_file");
    settings.allowlist = vec![format!("{}/*", workspace.path().display())];
    settings.denylist = vec![format!("{}/notes.txt", workspace.path().display())];

    let denied = resolve_file_tool_permission(&path, "read_file", &settings, None);
    assert_eq!(denied.permission, Some(PermissionMode::Never));

    settings.denylist.clear();
    let granted = resolve_file_tool_permission(&path, "read_file", &settings, None);
    assert_eq!(granted.permission, Some(PermissionMode::Always));
}

/// US-108: a sensitive path raises a `file_pattern` requirement naming the file,
/// granting the class for the session, even where the tool is `always`.
#[tokio::test]
async fn a_sensitive_path_asks_even_at_permission_always() {
    let directory = tempdir().expect("tempdir");
    let store = store_trusting(directory.path()).await;
    assert_eq!(
        store.tool_settings("read_file").permission,
        PermissionMode::Always,
        "the reference declares read_file as always"
    );
    let settings = store.tool_settings("read_file");

    let ordinary = store
        .resolve(
            "read_file",
            &resolve_file_tool_permission(
                &directory.path().join("notes.txt"),
                "read_file",
                &settings,
                None,
            ),
        )
        .await
        .expect("resolution");
    assert_eq!(ordinary.mode, PermissionMode::Always);

    let sensitive_context =
        resolve_file_tool_permission(&directory.path().join(".env"), "read_file", &settings, None);
    assert_eq!(
        sensitive_context.requirements,
        [PermissionRequirement::sensitive_file(".env", "read_file")]
    );
    assert_eq!(
        sensitive_context.requirements[0].session_pattern, "*",
        "an approval covers the sensitive class for the session"
    );
    let sensitive = store
        .resolve("read_file", &sensitive_context)
        .await
        .expect("resolution");
    assert_eq!(sensitive.mode, PermissionMode::Ask);
}

/// A tool configured to `never` refuses an outside path outright rather than
/// producing a requirement the operator could approve.
#[tokio::test]
async fn an_outside_path_under_permission_never_is_refused_without_a_requirement() {
    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let resolver = ToolConfigResolver::new();
    resolver.update(
        "[read_file]\npermission = \"never\"\n"
            .parse::<toml::Table>()
            .expect("settings parse"),
    );
    let store = PermissionStore::default().with_tool_config(resolver);
    store
        .set_trust(
            workspace.path(),
            TrustDecision::Trusted,
            TrustRootKind::Workspace,
        )
        .await
        .expect("trust");

    let resolution = store
        .resolve(
            "read_file",
            &PermissionContext::deferred().over_paths(vec![outside.path().join("secret.txt")]),
        )
        .await
        .expect("resolution");

    assert_eq!(resolution.mode, PermissionMode::Never);
    assert!(resolution.required_permissions.is_empty());
}

/// A path that resolves nowhere is treated as outside the working directory
/// rather than as inside it, and the guard fails toward asking.
#[tokio::test]
async fn an_unresolvable_path_is_treated_as_outside_the_working_directory() {
    let workspace = tempdir().expect("workspace");
    let store = store_trusting(workspace.path()).await;

    let resolution = store
        .resolve(
            "read_file",
            &PermissionContext::deferred()
                .over_paths(vec![workspace.path().join("missing/../../escape.txt")]),
        )
        .await
        .expect("resolution");

    assert_eq!(
        resolution.mode,
        PermissionMode::Ask,
        "a path the policy cannot resolve never resolves into the trusted root"
    );
    assert_eq!(
        resolution.required_permissions[0].scope,
        PermissionScope::OutsideDirectory
    );
}

/// A write into a directory that does not exist yet resolves against the
/// deepest ancestor that does, and a traversal hidden behind that missing
/// directory is refused rather than resolved into the trusted root.
#[test]
fn a_path_below_a_missing_directory_resolves_without_admitting_a_traversal() {
    let root = tempdir().expect("tempdir");
    let canonical = std::fs::canonicalize(root.path()).expect("canonical root");

    assert_eq!(
        canonicalize_for_policy(&canonical.join("missing/deeper/file.txt"))
            .expect("a missing parent chain resolves"),
        canonical.join("missing/deeper/file.txt")
    );
    assert!(matches!(
        canonicalize_for_policy(&canonical.join("missing/../../escape.txt")),
        Err(PolicyError::Denied(_))
    ));
}

/// A list match may only tighten what the path itself resolves to. A root the
/// operator refused stays refused, so an allowlist entry cannot reopen it.
#[tokio::test]
async fn a_listed_subject_never_reopens_an_untrusted_root() {
    let directory = tempdir().expect("tempdir");
    let store = PermissionStore::default();
    store
        .set_trust(
            directory.path(),
            TrustDecision::Untrusted,
            TrustRootKind::Workspace,
        )
        .await
        .expect("trust");
    let mut settings = store.tool_settings("read_file");
    settings.allowlist = vec![format!("{}/*", directory.path().display())];

    for name in ["notes.txt", ".env"] {
        let path = directory.path().join(name);
        std::fs::write(&path, "content").expect("write");
        let mut context = resolve_file_tool_permission(&path, "read_file", &settings, None);
        // Even a context the lists already granted carries its path, so the
        // refused root is still seen.
        context.paths = vec![path];
        let resolution = store
            .resolve("read_file", &context)
            .await
            .expect("resolution");
        assert_eq!(
            resolution.mode,
            PermissionMode::Never,
            "`{name}` sits in an untrusted root: {}",
            resolution.rationale
        );
    }
}

/// US-102: the configured permission is the whole decision for a tool that
/// produces no granular requirement, and `never` refuses outright.
#[tokio::test]
async fn the_configured_permission_decides_a_tool_with_no_requirement() {
    let store = PermissionStore::default();
    let granted = store
        .resolve("todo", &PermissionContext::deferred())
        .await
        .expect("resolution");
    assert_eq!(
        granted.mode,
        PermissionMode::Always,
        "the reference declares todo as always"
    );

    let resolver = ToolConfigResolver::new();
    resolver.update(
        "[todo]\npermission = \"never\"\n"
            .parse::<toml::Table>()
            .expect("settings parse"),
    );
    let refusing = PermissionStore::default().with_tool_config(resolver);
    let refused = refusing
        .resolve("todo", &PermissionContext::deferred())
        .await
        .expect("resolution");
    assert_eq!(refused.mode, PermissionMode::Never);

    // The session override outranks the configured value, both ways.
    refusing.set_tool_permission("todo", PermissionMode::Always);
    assert_eq!(
        refusing
            .resolve("todo", &PermissionContext::deferred())
            .await
            .expect("resolution")
            .mode,
        PermissionMode::Always
    );
}

/// An approval carrying no granular requirement has no pattern to store a rule
/// under, so what it grants is the tool for the session.
#[tokio::test]
async fn an_approval_without_a_requirement_grants_the_tool_for_the_session() {
    // A store nobody handed a resolver still observes its own session
    // overrides: the reference declares `web_fetch` as `ask`, so the grant
    // below is what moves it.
    let store = PermissionStore::default();
    assert_eq!(
        store.tool_settings("web_fetch").permission,
        PermissionMode::Ask
    );
    assert!(store.tool_permission("web_fetch").is_none());

    store
        .authorize(
            "web_fetch",
            Value::Null,
            PermissionContext::deferred(),
            &FixedApproval(ApprovalDecision::ApproveForSession),
        )
        .await
        .expect("the operator approves");
    assert_eq!(
        store.tool_permission("web_fetch"),
        Some(PermissionMode::Always)
    );
    // The next call resolves without asking again.
    assert_eq!(
        store
            .resolve("web_fetch", &PermissionContext::deferred())
            .await
            .expect("resolution")
            .mode,
        PermissionMode::Always
    );
}

#[derive(Default)]
struct GatedApproval {
    entered: Notify,
    release: Notify,
}

impl ApprovalAgent for GatedApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(ApprovalDecision::ApproveOnce)
        })
    }
}

#[tokio::test]
async fn closest_untrusted_root_overrides_trusted_ancestor() {
    let directory = tempdir().expect("tempdir");
    let nested = directory.path().join("nested");
    std::fs::create_dir(&nested).expect("nested");
    let store = PermissionStore::default();
    store
        .set_trust(
            directory.path(),
            TrustDecision::Trusted,
            TrustRootKind::Ancestor,
        )
        .await
        .expect("trust ancestor");
    store
        .set_trust(&nested, TrustDecision::Untrusted, TrustRootKind::Workspace)
        .await
        .expect("deny nested");
    let resolution = store
        .resolve(
            "read_file",
            &PermissionContext::deferred().over_paths(vec![nested.join("missing.txt")]),
        )
        .await
        .expect("resolve");
    assert_eq!(resolution.mode, PermissionMode::Never);
}

/// An agent profile installs its refusal and its narrower grant through the
/// same table, and the grant is the one that answers for the path it names.
#[tokio::test]
async fn a_profile_grant_outranks_the_blanket_refusal_written_next_to_it() {
    let store = PermissionStore::default();
    store
        .add_rule(PermissionRule {
            tool: "edit".to_owned(),
            scope: None,
            pattern: "*".to_owned(),
            mode: PermissionMode::Never,
            rationale: "agent-profile:plan".to_owned(),
        })
        .await;
    store
        .add_rule(PermissionRule {
            tool: "edit".to_owned(),
            scope: Some(PermissionScope::OutsideDirectory),
            pattern: "/workspace/plans/*".to_owned(),
            mode: PermissionMode::Always,
            rationale: "agent-profile:plan".to_owned(),
        })
        .await;

    let allowed = store
        .resolve(
            "edit",
            &PermissionContext::asking(vec![PermissionRequirement::outside_directory(
                "/workspace/plans/*",
            )]),
        )
        .await
        .expect("resolution");
    assert_eq!(allowed.mode, PermissionMode::Always);

    let refused = store
        .resolve(
            "edit",
            &PermissionContext::asking(vec![PermissionRequirement::outside_directory(
                "/workspace/src/*",
            )]),
        )
        .await
        .expect("resolution");
    assert_eq!(refused.mode, PermissionMode::Never);
}

#[tokio::test]
async fn approval_lock_serializes_user_decisions_without_holding_policy_state() {
    let store = PermissionStore::default();
    let directory = tempdir().expect("tempdir");
    store
        .set_trust(
            directory.path(),
            TrustDecision::Trusted,
            TrustRootKind::Workspace,
        )
        .await
        .expect("trust");
    let approval = Arc::new(GatedApproval::default());
    let first_store = store.clone();
    let first_approval = approval.clone();
    let first = tokio::spawn(async move {
        first_store
            .authorize(
                "web_fetch",
                Value::Null,
                PermissionContext::asking(vec![PermissionRequirement::url_domain("one.example")]),
                first_approval.as_ref(),
            )
            .await
    });
    approval.entered.notified().await;
    let safe = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        store.resolve(
            "read_file",
            &PermissionContext::deferred().over_paths(vec![directory.path().join("safe.txt")]),
        ),
    )
    .await
    .expect("safe policy resolution is not blocked")
    .expect("safe policy resolution");
    assert_eq!(safe.mode, PermissionMode::Always);
    let second_store = store.clone();
    let second_approval = approval.clone();
    let second = tokio::spawn(async move {
        second_store
            .authorize(
                "web_fetch",
                Value::Null,
                PermissionContext::asking(vec![PermissionRequirement::url_domain("two.example")]),
                second_approval.as_ref(),
            )
            .await
    });
    tokio::task::yield_now().await;
    approval.release.notify_one();
    first.await.expect("join first").expect("first approval");
    approval.entered.notified().await;
    approval.release.notify_one();
    second.await.expect("join second").expect("second approval");
}

#[tokio::test]
async fn trust_revocation_invalidates_lease_before_side_effect() {
    let directory = tempdir().expect("tempdir");
    let store = store_trusting(directory.path()).await;
    let lease = store
        .authorize(
            "read_file",
            Value::Null,
            PermissionContext::deferred().over_paths(vec![directory.path().join("file.txt")]),
            &FixedApproval(ApprovalDecision::Deny),
        )
        .await
        .expect("trusted root");
    store.revoke_trust(directory.path()).await.expect("revoke");
    assert!(matches!(
        lease.revalidate().await,
        Err(PolicyError::StaleApproval)
    ));
}

/// US-107: a covered invocation is re-validated before its side effect, so a
/// revocation lands even on a call a stored rule had already granted.
#[tokio::test]
async fn revocation_is_atomic_with_guarded_side_effects() {
    let directory = tempdir().expect("tempdir");
    let store = store_trusting(directory.path()).await;

    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let handler: Arc<dyn ToolHandler> = Arc::new({
        let entered = entered.clone();
        let release = release.clone();
        let calls = calls.clone();
        move |_invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let entered = entered.clone();
            let release = release.clone();
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                entered.notify_one();
                release.notified().await;
                Ok(ToolExecutionOutput::text("done"))
            })
        }
    });
    let required_path = directory.path().join("file.txt");
    let guarded = Arc::new(PolicyGuardedTool::new(
        "read_file",
        store.clone(),
        Arc::new(FixedApproval(ApprovalDecision::Deny)),
        Arc::new(move |_invocation| {
            Ok(PermissionContext::deferred().over_paths(vec![required_path.clone()]))
        }),
        handler,
    ));
    let registry = ToolRegistry::default();
    registry
        .register(
            ToolSpec {
                name: "read_file".to_owned(),
                description: "read".to_owned(),
                input_schema: json!({"type": "object", "additionalProperties": false}),
                output_schema: None,
                config: Value::Null,
                state: Value::Null,
                availability: ToolAvailability::Available,
                presentation: ToolPresentationKind::Generic,
                source: ToolSource::BuiltIn,
                selection_priority: 0,
            },
            guarded,
        )
        .expect("register");

    let first_registry = registry.clone();
    let first = tokio::spawn(async move {
        first_registry
            .invoke(
                "read_file",
                ToolInvocation {
                    call_id: "first".to_owned(),
                    arguments: json!({}),
                },
            )
            .await
    });
    entered.notified().await;

    let revoking_store = store.clone();
    let revoking_path = directory.path().to_path_buf();
    let revoke = tokio::spawn(async move { revoking_store.revoke_trust(revoking_path).await });
    tokio::task::yield_now().await;
    assert!(
        !revoke.is_finished(),
        "revocation waits for the active effect"
    );
    let second_registry = registry.clone();
    let second = tokio::spawn(async move {
        second_registry
            .invoke(
                "read_file",
                ToolInvocation {
                    call_id: "second".to_owned(),
                    arguments: json!({}),
                },
            )
            .await
    });

    release.notify_one();
    first.await.expect("first join").expect("first completes");
    revoke.await.expect("revoke join").expect("revoke");
    assert!(second.await.expect("second join").is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_escape_requires_external_permission() {
    use std::os::unix::fs::symlink;

    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    symlink(outside.path(), workspace.path().join("escape")).expect("symlink");
    let store = store_trusting(workspace.path()).await;
    let escaping = workspace.path().join("escape/secret.txt");
    std::fs::write(&escaping, "secret").expect("write");

    let resolution = store
        .resolve(
            "read_file",
            &PermissionContext::deferred().over_paths(vec![escaping]),
        )
        .await
        .expect("resolve");

    assert_eq!(resolution.mode, PermissionMode::Ask);
    assert_eq!(
        resolution.required_permissions[0].scope,
        PermissionScope::OutsideDirectory
    );
}

/// A tool executor reports its failures as strings, so the prefix a refusal
/// carries is the only thing that tells an operator's refusal from a tool that
/// failed on its own once the error has crossed that boundary. This holds the
/// constant to the rendering it names.
#[test]
fn the_denial_prefix_is_what_a_denial_reports() {
    let denied = PolicyError::Denied("approval denied".to_owned()).to_string();
    assert!(
        denied.starts_with(super::DENIAL_PREFIX),
        "`{denied}` no longer starts with `{}`",
        super::DENIAL_PREFIX
    );
    assert!(
        !PolicyError::StaleApproval
            .to_string()
            .starts_with(super::DENIAL_PREFIX),
        "only a refusal carries the prefix"
    );
}
