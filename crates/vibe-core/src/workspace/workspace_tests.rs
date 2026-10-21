use std::time::Duration;

use serde_json::json;
use tempfile::tempdir;

use super::*;

use super::tools::WorkspaceTools;
use crate::checkpoints::{Owner, ReviewTarget};
use crate::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionMode,
    PermissionStore, ToolGuard, TrustDecision, TrustRootKind,
};
use crate::process::ClientToolIo;
use crate::tools::config::ToolConfigResolver;
use crate::tools::{ToolInvocation, ToolRegistry};

/// What a rewind of the turn numbered zero asks of the review surface.
const FIRST_TURN: ReviewTarget = ReviewTarget::Scope {
    owner: Owner::Agent { turn_id: 0 },
};

/// The key the review log files `relative` under: its resolved absolute
/// path, joined one component at a time so the separators are native.
fn log_key(root: &Path, relative: &str) -> String {
    relative
        .split('/')
        .fold(
            crate::platform::canonicalize(root).expect("canonical root"),
            |path, part| path.join(part),
        )
        .to_string_lossy()
        .into_owned()
}

/// The declared `grep` options with the call's own limit and ignore
/// choice, which is what the handler composes.
fn probe_options(limit: usize, use_default_ignore: bool) -> SearchOptions {
    SearchOptions {
        limit,
        use_default_ignore,
        ..SearchOptions::default()
    }
}

struct RejectApproval;

impl ApprovalAgent for RejectApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::Deny) })
    }
}

#[test]
fn discovery_reads_searches_and_honors_ignore_order() {
    let directory = tempdir().expect("tempdir");
    // `rg` reads a `.gitignore` only inside a repository, and so does the
    // `ignore` crate it is built on, so the marker is what makes this
    // fixture's ignore file apply at all.
    std::fs::create_dir(directory.path().join(".git")).expect("git marker");
    std::fs::write(directory.path().join(".gitignore"), "ignored.txt\n").expect("ignore");
    std::fs::write(directory.path().join("visible.txt"), "alpha\nbeta\n").expect("visible");
    std::fs::write(directory.path().join("ignored.txt"), "alpha\n").expect("ignored");
    let workspace = Workspace::open(directory.path()).expect("workspace");
    let discovered = workspace.discover().expect("discover");
    assert!(discovered.iter().any(|entry| entry.path == "visible.txt"));
    assert!(discovered.iter().all(|entry| entry.path != "ignored.txt"));
    let read = workspace.read("visible.txt", 2, Some(1)).expect("read");
    assert_eq!(read.numbered_content, "2|beta");
    let matched = workspace
        .search("alpha", ".", &probe_options(10, true))
        .expect("search");
    assert_eq!(matched.match_count, 1);
    assert_eq!(matched.matches, "./visible.txt:1:alpha");

    // `use_default_ignore: false` drops the .gitignore entries and keeps
    // the always-excluded directories, matching the reference split.
    let unfiltered = workspace
        .search("alpha", ".", &probe_options(10, false))
        .expect("search");
    assert_eq!(unfiltered.match_count, 2);
}

#[test]
fn search_reaches_past_the_reader_page_limit() {
    let directory = tempdir().expect("tempdir");
    let mut content = "filler\n".repeat(DEFAULT_MAX_LINES + 10);
    content.push_str("needle\n");
    std::fs::write(directory.path().join("long.txt"), content).expect("long file");
    let workspace = Workspace::open(directory.path()).expect("workspace");

    let matched = workspace
        .search("needle", ".", &probe_options(10, true))
        .expect("search");

    assert_eq!(matched.match_count, 1);
    assert_eq!(
        matched.matches,
        format!("./long.txt:{}:needle", DEFAULT_MAX_LINES + 11)
    );
}

#[test]
fn edit_diff_reports_only_the_changed_span() {
    let directory = tempdir().expect("tempdir");
    let before = (0..500)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(directory.path().join("wide.txt"), &before).expect("seed file");
    let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
    let review = ReviewManager::new(workspace);
    review.begin_turn_at(0).expect("turn");

    let result = review
        .edit(
            "wide.txt",
            &[EditOperation {
                old_text: "line 250".to_owned(),
                new_text: "line 250 edited".to_owned(),
                replace_all: false,
            }],
        )
        .expect("edit");

    assert!(result.diff.contains("@@ -248,7 +248,7 @@"));
    assert!(result.diff.contains("-line 250"));
    assert!(result.diff.contains("+line 250 edited"));
    assert_eq!(
        result
            .diff
            .lines()
            .filter(|line| line.starts_with('-'))
            .count(),
        2,
        "only the changed line and the `--- before` header start with a dash"
    );
}

#[test]
fn binary_invalid_encoding_and_traversal_fail_closed() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("binary"), [0, 1, 2]).expect("binary");
    std::fs::write(directory.path().join("invalid"), [0xff, 0xfe]).expect("invalid");
    let workspace = Workspace::open(directory.path()).expect("workspace");
    assert!(matches!(
        workspace.read("binary", 1, None),
        Err(WorkspaceError::Binary(_))
    ));
    assert!(matches!(
        workspace.read("invalid", 1, None),
        Err(WorkspaceError::InvalidEncoding(_))
    ));
    assert!(matches!(
        workspace.read("../secret", 1, None),
        Err(WorkspaceError::OutsideRoot(_))
    ));
}

#[cfg(unix)]
#[test]
fn capability_directory_rejects_symlink_escape() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().expect("tempdir");
    let outside = tempdir().expect("outside");
    std::fs::write(outside.path().join("secret"), "secret").expect("secret");
    symlink(outside.path(), directory.path().join("escape")).expect("symlink");
    let workspace = Workspace::open(directory.path()).expect("workspace");
    assert!(workspace.read("escape/secret", 1, None).is_err());
}

#[test]
fn edit_requires_unique_content_and_review_reverts_atomically() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("file.txt"), "one\ntwo\n").expect("file");
    let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
    let review = ReviewManager::new(workspace.clone());
    review.begin_turn_at(0).expect("turn");
    let result = review
        .edit(
            "file.txt",
            &[EditOperation {
                old_text: "two".to_owned(),
                new_text: "three".to_owned(),
                replace_all: false,
            }],
        )
        .expect("edit");
    assert!(result.diff.contains("+three"));
    review.seal_turn().expect("seal");
    assert_eq!(review.review_state().expect("review state").files.len(), 1);
    review.revert_review(&FIRST_TURN).expect("revert");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("file.txt")).expect("read"),
        "one\ntwo\n"
    );
}

#[test]
fn manual_drift_is_reconciled_before_review() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("file.txt"), "before\n").expect("file");
    let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
    let review = ReviewManager::new(workspace);
    review.begin_turn_at(0).expect("turn");
    review
        .edit(
            "file.txt",
            &[EditOperation {
                old_text: "before".to_owned(),
                new_text: "tool".to_owned(),
                replace_all: false,
            }],
        )
        .expect("edit");
    std::fs::write(directory.path().join("file.txt"), "manual\n").expect("manual drift");
    review.seal_turn().expect("seal");
    let diff = review
        .scope_file_diff(
            &log_key(directory.path(), "file.txt"),
            Owner::Agent { turn_id: 0 },
        )
        .expect("scope diff");
    assert_eq!(diff.baseline, "before\n");
    assert_eq!(diff.current, "manual\n");
}

#[test]
fn rewind_restoration_is_target_specific_transactional_and_truncates_the_log() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("main.txt"), "zero\n").expect("main fixture");
    std::fs::create_dir(directory.path().join("generated")).expect("generated directory");
    let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
    let review = ReviewManager::new(workspace);

    review.begin_turn_at(2).expect("first turn");
    review
        .write("generated/first.txt", b"first\n")
        .expect("first write");
    review.seal_turn().expect("first checkpoint");
    review.begin_turn_at(4).expect("second turn");
    review
        .edit(
            "main.txt",
            &[EditOperation {
                old_text: "zero".to_owned(),
                new_text: "two".to_owned(),
                replace_all: false,
            }],
        )
        .expect("second edit");
    review
        .write("generated/later.txt", b"later\n")
        .expect("later write");
    review.seal_turn().expect("second checkpoint");

    // The plan is the log's, so its order is the order the dropped turns
    // first touched each path rather than an alphabetical one, and each is
    // named by the resolved absolute path the log keys it by.
    let canonical = crate::platform::canonicalize(directory.path()).expect("canonical");
    let key = |name: &str| {
        name.split('/')
            .fold(canonical.clone(), |path, part| path.join(part))
            .to_string_lossy()
            .into_owned()
    };
    assert_eq!(
        review.restorable_paths_at(4).expect("latest paths"),
        vec![key("main.txt"), key("generated/later.txt")]
    );
    assert_eq!(
        review.restorable_paths_at(2).expect("earlier paths"),
        vec![
            key("generated/first.txt"),
            key("main.txt"),
            key("generated/later.txt")
        ]
    );

    let staged = review
        .stage_restore_to_message(4)
        .expect("staged restoration");
    assert!(staged.errors.is_empty(), "every path was writable");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("main.txt")).expect("staged main"),
        "zero\n"
    );
    assert!(!directory.path().join("generated/later.txt").exists());
    assert!(directory.path().join("generated/first.txt").exists());
    staged.transaction.rollback().expect("explicit rollback");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("main.txt")).expect("rolled back main"),
        "two\n"
    );
    assert!(directory.path().join("generated/later.txt").exists());

    let restored = review
        .stage_restore_to_message(2)
        .expect("earlier restoration")
        .transaction
        .commit();
    assert_eq!(
        restored,
        vec![
            key("generated/first.txt"),
            key("main.txt"),
            key("generated/later.txt")
        ]
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("main.txt")).expect("restored main"),
        "zero\n"
    );
    assert!(!directory.path().join("generated/first.txt").exists());
    assert!(!directory.path().join("generated/later.txt").exists());

    review.drop_turns_from(4).expect("rewind truncation");
    assert_eq!(
        review
            .with_log(|log| (1..=4)
                .filter(|turn| log.history().has_turn(*turn))
                .collect::<Vec<_>>())
            .expect("truncated log"),
        vec![2],
        "a rewind keeps the turns before its point and drops the rest"
    );
}

#[tokio::test]
async fn agents_hierarchy_is_injected_once() {
    let directory = tempdir().expect("tempdir");
    let nested = directory.path().join("nested");
    std::fs::create_dir(&nested).expect("nested");
    std::fs::write(directory.path().join("AGENTS.md"), "root").expect("root agents");
    std::fs::write(nested.join("AGENTS.md"), "nested").expect("nested agents");
    let workspace = Workspace::open(directory.path()).expect("workspace");
    let first = workspace
        .project_context("nested", None)
        .await
        .expect("context");
    assert_eq!(first.instruction_files.len(), 2);
    let second = workspace
        .project_context("nested", None)
        .await
        .expect("context");
    assert!(second.instruction_files.is_empty());
}

#[tokio::test]
async fn registered_workspace_tools_revalidate_trust_before_filesystem_access() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("visible.txt"), "safe\n").expect("file");
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
    WorkspaceTools::new(workspace, review)
        .register(
            &registry,
            &ToolGuard::new(policy.clone(), Arc::new(RejectApproval)),
        )
        .expect("register");
    let result = registry
        .invoke(
            "read_file",
            ToolInvocation {
                call_id: "read-1".to_owned(),
                arguments: json!({"file_path": "visible.txt"}),
            },
        )
        .await
        .expect("trusted read");
    assert_eq!(result.typed_result["content"], "        1\u{2192}safe");

    policy.revoke_trust(directory.path()).await.expect("revoke");
    let denied = registry
        .invoke(
            "read_file",
            ToolInvocation {
                call_id: "read-2".to_owned(),
                arguments: json!({"file_path": "visible.txt"}),
            },
        )
        .await
        .expect("revoked trust settles as a refusal");
    assert_eq!(
        denied.skip,
        Some(crate::tools::ToolSkip { cancelled: true }),
        "a revoked root reaches the operator, who refuses: {denied:?}"
    );
}

/// The three registered names and the argument keys each one reads, which
/// is the contract a model prompted for reference behavior relies on.
#[tokio::test]
async fn the_file_tools_publish_the_reference_names_and_argument_keys() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("visible.txt"), "alpha\nbeta\n").expect("file");
    let registry = registered_workspace_tools(directory.path()).await;

    let names = registry
        .list()
        .expect("list")
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    // Registration order, which is the order the surface carries.
    assert_eq!(names, ["read_file", "grep", "edit", "write_file"]);

    let read = registry
        .invoke(
            "read_file",
            ToolInvocation {
                call_id: "read-1".to_owned(),
                arguments: json!({"file_path": "visible.txt", "offset": 2}),
            },
        )
        .await
        .expect("read_file accepts the reference keys");
    assert_eq!(read.typed_result["content"], "        2\u{2192}beta");
    assert_eq!(read.typed_result["requested_offset"], 2);

    let grep = registry
        .invoke(
            "grep",
            ToolInvocation {
                call_id: "grep-1".to_owned(),
                arguments: json!({"pattern": "al.ha"}),
            },
        )
        .await
        .expect("grep treats its pattern as a regular expression");
    assert_eq!(grep.typed_result["matches"], "./visible.txt:1:alpha");
    assert_eq!(grep.typed_result["match_count"], 1);
}

/// An offset past the last line is an explicit out-of-range answer, never
/// an empty success the model would read as an empty file.
#[tokio::test]
async fn read_file_reports_an_offset_past_the_end_of_the_file() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("short.txt"), "only\n").expect("file");
    let registry = registered_workspace_tools(directory.path()).await;

    let beyond = registry
        .invoke(
            "read_file",
            ToolInvocation {
                call_id: "read-1".to_owned(),
                arguments: json!({"file_path": "short.txt", "offset": 9}),
            },
        )
        .await
        .expect("an out-of-range offset still answers");

    assert!(
        beyond.model_text.contains("1 lines") && beyond.model_text.contains("offset 9"),
        "the answer must name the file length and the offset: {}",
        beyond.model_text
    );
}

/// The out-of-range answer keys on the selected line count, not on whether
/// those lines carry text, so a file holding one blank line reads back as
/// that blank line rather than as a file shorter than the offset.
#[tokio::test]
async fn read_file_returns_a_blank_line_rather_than_an_out_of_range_warning() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("blank.txt"), "\n").expect("file");
    let registry = registered_workspace_tools(directory.path()).await;

    let blank = registry
        .invoke(
            "read_file",
            ToolInvocation {
                call_id: "read-1".to_owned(),
                arguments: json!({"file_path": "blank.txt"}),
            },
        )
        .await
        .expect("a blank line is content");

    assert_eq!(blank.typed_result["content"], "        1\u{2192}");
    assert_eq!(blank.typed_result["num_lines"], 1);
}

/// The reference resolves `max_matches` with `or`, so a zero is not a cap
/// of zero: it falls back to the configured default like an absent value.
#[tokio::test]
async fn grep_treats_a_zero_max_matches_as_the_default_cap() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("visible.txt"), "a\na\na\n").expect("file");
    let registry = registered_workspace_tools(directory.path()).await;

    let matched = registry
        .invoke(
            "grep",
            ToolInvocation {
                call_id: "grep-1".to_owned(),
                arguments: json!({"pattern": "a", "max_matches": 0}),
            },
        )
        .await
        .expect("grep answers");

    assert_eq!(matched.typed_result["match_count"], 3);
}

#[tokio::test]
async fn grep_reports_an_invalid_pattern_instead_of_searching() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("visible.txt"), "alpha\n").expect("file");
    let registry = registered_workspace_tools(directory.path()).await;

    let error = registry
        .invoke(
            "grep",
            ToolInvocation {
                call_id: "grep-1".to_owned(),
                arguments: json!({"pattern": "alpha("}),
            },
        )
        .await
        .expect_err("an unparsable pattern cannot search");

    assert!(
        error.to_string().contains("invalid search pattern"),
        "the failure must name the pattern error: {error}"
    );
}

/// The camelCase keys the port used to publish are gone: a call written
/// against them fails naming the reference key it left out.
#[tokio::test]
async fn edit_rejects_the_previous_camel_case_keys() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("file.txt"), "before\n").expect("file");
    let registry = registered_workspace_tools(directory.path()).await;

    let error = registry
        .invoke(
            "edit",
            ToolInvocation {
                call_id: "edit-1".to_owned(),
                arguments: json!({
                    "path": "file.txt",
                    "oldText": "before",
                    "newText": "after",
                }),
            },
        )
        .await
        .expect_err("the camelCase keys are no longer the contract");

    assert!(
        error.to_string().contains("$.file_path")
            && error.to_string().contains("required property is missing"),
        "the failure must name the missing reference key: {error}"
    );
}

/// The stale-edit and ambiguity failures survive the key rename, and the
/// write permission is still derived from the renamed path argument.
#[tokio::test]
async fn edit_keeps_its_failure_modes_and_its_permission_scope_under_the_new_keys() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("file.txt"), "one\none\n").expect("file");
    let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
    let review = Arc::new(ReviewManager::new(workspace.clone()));
    review.begin_turn_at(0).expect("turn");
    let policy = PermissionStore::default();
    // The reference declares `edit` and `write_file` as `ask`, so a harness
    // exercising their bodies grants them the way an operator's "allow
    // always" does rather than answering a prompt per call.
    for tool in ["edit", "write_file"] {
        policy.set_tool_permission(tool, PermissionMode::Always);
    }
    policy
        .set_trust(
            directory.path(),
            TrustDecision::Trusted,
            TrustRootKind::Workspace,
        )
        .await
        .expect("trust");
    let registry = ToolRegistry::default();
    WorkspaceTools::new(workspace, review)
        .register(
            &registry,
            &ToolGuard::new(policy.clone(), Arc::new(RejectApproval)),
        )
        .expect("register");

    let ambiguous = registry
        .invoke(
            "edit",
            ToolInvocation {
                call_id: "edit-1".to_owned(),
                arguments: json!({
                    "file_path": "file.txt",
                    "old_string": "one",
                    "new_string": "two",
                }),
            },
        )
        .await
        .expect_err("two matches without replace_all stay ambiguous");
    assert!(
        ambiguous.to_string().contains("matches 2 locations"),
        "{ambiguous}"
    );

    let stale = registry
        .invoke(
            "edit",
            ToolInvocation {
                call_id: "edit-2".to_owned(),
                arguments: json!({
                    "file_path": "file.txt",
                    "old_string": "absent",
                    "new_string": "two",
                }),
            },
        )
        .await
        .expect_err("a needle that is not there is a stale edit");
    assert!(stale.to_string().contains("is stale"), "{stale}");

    let replaced = registry
        .invoke(
            "edit",
            ToolInvocation {
                call_id: "edit-3".to_owned(),
                arguments: json!({
                    "file_path": "file.txt",
                    "old_string": "one",
                    "new_string": "two",
                    "replace_all": true,
                }),
            },
        )
        .await
        .expect("replace_all resolves the ambiguity");
    // The diff moved to the display payload: the model reads the reference
    // fields, and the transcript renders the hunk.
    assert!(
        replaced.display["diff"]
            .as_str()
            .is_some_and(|diff| diff.contains("+two")),
        "{replaced:?}"
    );
    assert!(
        replaced
            .model_text
            .contains("every occurrence was replaced")
    );
}

/// A trusted workspace with the file tools registered and every approval
/// refused, so anything reaching the approval path fails loudly.
async fn registered_workspace_tools(root: &Path) -> ToolRegistry {
    registered_with_settings(root, "").await.0
}

/// US-246: `grep` publishes a second document for the UI, one parsed entry
/// per printed match. The separator rule is the reference's: the printed
/// line splits on its first two colons only, so a colon inside the matched
/// text stays in the text and never shifts the line number.
#[tokio::test]
async fn grep_projects_one_parsed_entry_per_match() {
    let root = tempdir().expect("workspace");
    std::fs::write(
        root.path().join("notes.md"),
        "intro
needle: a colon inside the text
",
    )
    .expect("file");
    let registry = registered_workspace_tools(root.path()).await;

    let found = registry
        .invoke(
            "grep",
            ToolInvocation {
                call_id: "grep-1".to_owned(),
                arguments: json!({"pattern": "needle", "path": "."}),
            },
        )
        .await
        .expect("the search answers");
    // The reference resolves a printed match against the search root, so
    // the entry carries the absolute path the corpus normalizes away.
    assert_eq!(
        found.projected_result["parsed_matches"],
        json!([{
            "path": crate::platform::canonicalize(root.path())
                .expect("the root resolves")
                .join("notes.md")
                .display()
                .to_string(),
            "line": 2,
        }]),
    );
    assert!(
        found.projected_result.get("pattern").is_none(),
        "the projection drops the pattern the typed result keeps: {found:?}"
    );

    let empty = registry
        .invoke(
            "grep",
            ToolInvocation {
                call_id: "grep-2".to_owned(),
                arguments: json!({"pattern": "haystack", "path": "."}),
            },
        )
        .await
        .expect("the empty search answers");
    assert_eq!(empty.projected_result["parsed_matches"], json!([]));
}

/// US-247: `edit` publishes one occurrence per replacement, each carrying
/// the line it started on in the file as it stood before the edit and the
/// whole lines the replacement covered, and drops the `message` the typed
/// result carries.
#[tokio::test]
async fn edit_projects_one_occurrence_per_replacement() {
    let root = tempdir().expect("workspace");
    std::fs::write(
        root.path().join("main.rs"),
        "let old = 1;
keep
let old = 2;
",
    )
    .expect("file");
    let registry = registered_workspace_tools(root.path()).await;

    let edited = registry
        .invoke(
            "edit",
            ToolInvocation {
                call_id: "edit-1".to_owned(),
                arguments: json!({
                    "file_path": "main.rs",
                    "old_string": "old",
                    "new_string": "new",
                    "replace_all": true,
                }),
            },
        )
        .await
        .expect("the edit applies");
    assert_eq!(
        edited.projected_result["occurrences"],
        json!([
            {"start_line": 1, "old_text": "let old = 1;", "new_text": "let new = 1;"},
            {"start_line": 3, "old_text": "let old = 2;", "new_text": "let new = 2;"},
        ]),
    );
    assert!(
        edited.projected_result.get("message").is_none(),
        "the projection drops the message the typed result carries: {edited:?}"
    );
    assert!(edited.typed_result["message"].is_string());
}

/// US-108: a registered file tool passes the guard for the session
/// scratchpad without an approval, even for a name its own
/// `sensitive_patterns` would otherwise stop, and the same name inside the
/// workspace still asks.
///
/// The scratchpad sits outside the workspace root, and reference
/// `resolve_file_tool_permission` grants it before anything else is read,
/// so the read lands rather than stopping at the root.
#[tokio::test]
async fn a_registered_file_tool_reaches_the_scratchpad_without_asking() {
    let root = tempdir().expect("workspace");
    let scratchpad =
        crate::scratchpad::init_scratchpad("workspace-scratchpad-probe").expect("scratchpad");
    std::fs::write(scratchpad.join(".env"), "SECRET=1\n").expect("scratchpad file");
    std::fs::write(root.path().join(".env"), "SECRET=2\n").expect("workspace file");

    let workspace = Arc::new(Workspace::open(root.path()).expect("workspace"));
    let review = Arc::new(ReviewManager::new(workspace.clone()));
    let policy = PermissionStore::default();
    policy
        .set_trust(
            root.path(),
            TrustDecision::Trusted,
            TrustRootKind::Workspace,
        )
        .await
        .expect("trust");
    let registry = ToolRegistry::default();
    WorkspaceTools::new(workspace, review)
        .register(
            &registry,
            &ToolGuard::new(policy, Arc::new(RejectApproval))
                .with_scratchpad(Some(scratchpad.clone())),
        )
        .expect("register");

    let granted = registry
        .invoke(
            "read_file",
            ToolInvocation {
                call_id: "scratchpad-read".to_owned(),
                arguments: json!({"file_path": scratchpad.join(".env").to_string_lossy()}),
            },
        )
        .await
        .expect("the scratchpad is read without asking");
    assert!(
        granted.model_text.contains("SECRET=1"),
        "the scratchpad read: {granted:?}"
    );

    let asked = registry
        .invoke(
            "read_file",
            ToolInvocation {
                call_id: "workspace-read".to_owned(),
                arguments: json!({"file_path": ".env"}),
            },
        )
        .await
        .expect("a sensitive name inside the workspace asks");
    assert_eq!(
        asked.skip,
        Some(crate::tools::ToolSkip { cancelled: true }),
        "the same name inside the workspace reaches the operator: {asked:?}"
    );

    crate::scratchpad::cleanup_scratchpad(Some(&scratchpad));
}

/// US-011 (Windows support): through the published `read_file`, a
/// dotenv file asks in any letter case, a denylist entry written with `/`
/// refuses a target spelled with `\`, and an allowlist entry differing
/// only in case grants a file outside the workspace.
#[cfg(windows)]
#[tokio::test]
async fn the_published_read_file_applies_the_lists_to_windows_paths() {
    let root = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let root_path = crate::platform::canonicalize(root.path()).expect("canonical root");
    let outside_path = crate::platform::canonicalize(outside.path()).expect("canonical");
    std::fs::write(root_path.join(".env"), "SECRET=1\n").expect("dotenv");
    std::fs::create_dir(root_path.join("notes")).expect("notes");
    std::fs::write(root_path.join("notes").join("a.txt"), "denied\n").expect("note");
    std::fs::write(outside_path.join("secret.txt"), "granted\n").expect("outside");
    let denied = format!("{}/notes/*", root_path.display()).replace('\\', "/");
    let granted = outside_path
        .join("secret.txt")
        .display()
        .to_string()
        .to_uppercase();
    let (registry, _config) = registered_with_settings(
        &root_path,
        &format!("[read_file]\ndenylist = ['{denied}']\nallowlist = ['{granted}']\n"),
    )
    .await;
    let read = |path: String| {
        registry.invoke(
            "read_file",
            ToolInvocation {
                call_id: "windows-read".to_owned(),
                arguments: json!({ "file_path": path }),
            },
        )
    };

    for spelling in [".env", ".ENV"] {
        let asked = read(root_path.join(spelling).display().to_string())
            .await
            .expect("a sensitive read reaches the operator");
        assert_eq!(
            asked.skip,
            Some(crate::tools::ToolSkip { cancelled: true }),
            "`{spelling}` is sensitive at permission always: {asked:?}"
        );
    }
    let refused = read(format!(r"{}\notes\a.txt", root_path.display())).await;
    assert!(
        matches!(&refused, Err(crate::tools::ToolError::Execution(message)) if message.contains("denylist")),
        "the denylist refuses the read: {refused:?}"
    );
    let landed = read(outside_path.join("secret.txt").display().to_string())
        .await
        .expect("the allowlist grants the outside read");
    assert!(
        landed.model_text.contains("granted"),
        "the outside read: {landed:?}"
    );
}

/// The file tools of a trusted workspace, published against a resolver the
/// caller can move afterward.
///
/// The resolver is returned rather than consumed, which is what proves the
/// registration hands the families a resolver and not a snapshot: a test
/// changes `settings` after the surface is published and the next call
/// obeys it.
async fn registered_with_settings(
    root: &Path,
    settings: &str,
) -> (ToolRegistry, ToolConfigResolver) {
    let workspace = Arc::new(Workspace::open(root).expect("workspace"));
    let review = Arc::new(ReviewManager::new(workspace.clone()));
    // A mutating tool snapshots what it is about to change, which this port
    // only allows inside a turn, so the helper opens one.
    review.begin_turn_at(0).expect("a turn opens");
    let policy = PermissionStore::default();
    // The reference declares `edit` and `write_file` as `ask`, so a harness
    // exercising their bodies grants them the way an operator's "allow
    // always" does rather than answering a prompt per call.
    for tool in ["edit", "write_file"] {
        policy.set_tool_permission(tool, PermissionMode::Always);
    }
    policy
        .set_trust(root, TrustDecision::Trusted, TrustRootKind::Workspace)
        .await
        .expect("trust");
    let config = policy.tool_config();
    config.update(settings.parse::<toml::Table>().expect("settings parse"));
    let registry = ToolRegistry::default();
    WorkspaceTools::new(workspace, review)
        .register(
            &registry,
            &ToolGuard {
                policy,
                approval: Arc::new(RejectApproval),
                config: config.clone(),
                scratchpad: None,
            },
        )
        .expect("register");
    (registry, config)
}

/// US-103: `read_file` reads its budget from the configuration, and a
/// render past it fails naming both sizes rather than truncating.
#[tokio::test]
async fn a_configured_read_budget_refuses_the_render_naming_both_sizes() {
    let directory = tempdir().expect("tempdir");
    let content = (1..=200)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(directory.path().join("long.txt"), &content).expect("file");
    let (registry, config) =
        registered_with_settings(directory.path(), "[read_file]\nmax_read_bytes = 120\n").await;

    let refused = registry
        .invoke(
            "read_file",
            ToolInvocation {
                call_id: "read-1".to_owned(),
                arguments: json!({"file_path": "long.txt"}),
            },
        )
        .await
        .expect_err("a render past the budget is refused");
    let message = refused.to_string();
    assert!(message.contains("120-byte budget"), "{message}");
    assert!(message.contains("offset and limit"), "{message}");

    // The surface was published against the resolver, not a snapshot of it,
    // so raising the budget between two calls is obeyed on the second.
    config.update(
        "[read_file]\nmax_read_bytes = 51200\n"
            .parse::<toml::Table>()
            .expect("settings parse"),
    );
    let read = registry
        .invoke(
            "read_file",
            ToolInvocation {
                call_id: "read-2".to_owned(),
                arguments: json!({"file_path": "long.txt"}),
            },
        )
        .await
        .expect("the raised budget carries the same file");
    assert_eq!(read.typed_result["total_lines"], json!(200));
}

/// US-103: `grep` reads its cap, its exclusion globs and its codeignore file
/// from the configuration.
#[tokio::test]
async fn grep_reads_its_cap_and_its_exclusions_from_the_configuration() {
    let directory = tempdir().expect("tempdir");
    std::fs::create_dir_all(directory.path().join("build")).expect("build");
    std::fs::create_dir_all(directory.path().join("generated")).expect("generated");
    std::fs::write(
        directory.path().join("kept.txt"),
        "needle\nneedle\nneedle\n",
    )
    .expect("kept");
    std::fs::write(directory.path().join("build/out.txt"), "needle\n").expect("built");
    std::fs::write(directory.path().join("generated/out.txt"), "needle\n").expect("generated");
    std::fs::write(
        directory.path().join(".vibeignore"),
        "# comment\n\ngenerated/\n",
    )
    .expect("codeignore");
    let (registry, config) = registered_with_settings(directory.path(), "").await;

    let matched = registry
        .invoke(
            "grep",
            ToolInvocation {
                call_id: "grep-1".to_owned(),
                arguments: json!({"pattern": "needle"}),
            },
        )
        .await
        .expect("search");
    let paths = matched.typed_result["match_count"]
        .as_u64()
        .expect("a match count");
    assert_eq!(
        paths, 3,
        "the declared exclusions drop `build/` and the codeignore file drops `generated/`: {}",
        matched.model_text
    );

    // The cap is the operator's: two matches out of the three.
    config.update(
        "[grep]\ndefault_max_matches = 2\n"
            .parse::<toml::Table>()
            .expect("settings parse"),
    );
    let capped = registry
        .invoke(
            "grep",
            ToolInvocation {
                call_id: "grep-2".to_owned(),
                arguments: json!({"pattern": "needle"}),
            },
        )
        .await
        .expect("search");
    assert_eq!(
        capped.typed_result["match_count"], 2,
        "{}",
        capped.model_text
    );

    // And so is the byte budget the model-facing text is clipped at.
    config.update(
        "[grep]\nmax_output_bytes = 12\n"
            .parse::<toml::Table>()
            .expect("settings parse"),
    );
    let clipped = registry
        .invoke(
            "grep",
            ToolInvocation {
                call_id: "grep-3".to_owned(),
                arguments: json!({"pattern": "needle"}),
            },
        )
        .await
        .expect("search");
    // The budget clips the match list rather than the rendered result, and
    // the flag is what tells the model the answer was cut short.
    assert_eq!(
        clipped.typed_result["matches"].as_str().map(str::len),
        Some(12),
        "{}",
        clipped.model_text
    );
    assert_eq!(clipped.typed_result["was_truncated"], true);
}

/// US-103: `write_file` reads its byte budget and its parent-creation flag
/// from the configuration, and refuses before touching the filesystem.
#[tokio::test]
async fn write_file_reads_its_budget_and_parent_creation_from_the_configuration() {
    let directory = tempdir().expect("tempdir");
    let (registry, config) = registered_with_settings(
        directory.path(),
        "[write_file]\nmax_write_bytes = 16\ncreate_parent_dirs = false\n",
    )
    .await;
    let oversized = registry
        .invoke(
            "write_file",
            ToolInvocation {
                call_id: "write-1".to_owned(),
                arguments: json!({"file_path": "big.txt", "content": "x".repeat(64)}),
            },
        )
        .await
        .expect_err("a write past the configured budget is refused");
    assert!(
        oversized.to_string().contains("16-byte write budget"),
        "{oversized}"
    );
    assert!(!directory.path().join("big.txt").exists());

    let missing = registry
        .invoke(
            "write_file",
            ToolInvocation {
                call_id: "write-2".to_owned(),
                arguments: json!({"file_path": "nested/child.txt", "content": "hi"}),
            },
        )
        .await
        .expect_err("a missing parent is refused when the flag is off");
    assert!(missing.to_string().contains("nested"), "{missing}");
    assert!(!directory.path().join("nested").exists());

    // The reference default creates the directory instead.
    config.update(
        "[write_file]\ncreate_parent_dirs = true\n"
            .parse::<toml::Table>()
            .expect("settings parse"),
    );
    registry
        .invoke(
            "write_file",
            ToolInvocation {
                call_id: "write-3".to_owned(),
                arguments: json!({"file_path": "nested/child.txt", "content": "hi"}),
            },
        )
        .await
        .expect("the parent is created");
    assert!(directory.path().join("nested/child.txt").exists());
}

/// A client hosting an editor: it answers reads from a buffer that differs
/// from what is on disk, so a delegated read is distinguishable from a local
/// one by its content alone.
#[derive(Default)]
struct EditorClient {
    requests: Mutex<Vec<crate::process::ClientToolRequest>>,
    buffer: Mutex<String>,
    capabilities: Mutex<BTreeSet<crate::process::ClientToolCapability>>,
}

impl EditorClient {
    fn hosting(
        buffer: &str,
        capabilities: impl IntoIterator<Item = crate::process::ClientToolCapability>,
    ) -> Arc<Self> {
        let client = Self::default();
        *client.buffer.lock().expect("buffer") = buffer.to_owned();
        *client.capabilities.lock().expect("capabilities") = capabilities.into_iter().collect();
        Arc::new(client)
    }

    fn methods(&self) -> Vec<&'static str> {
        self.requests
            .lock()
            .expect("requests")
            .iter()
            .map(crate::process::ClientToolRequest::method)
            .collect()
    }
}

impl crate::process::ClientToolPort for EditorClient {
    fn request<'a>(
        &'a self,
        request: crate::process::ClientToolRequest,
    ) -> crate::process::ToolIoFuture<'a> {
        Box::pin(async move {
            self.requests
                .lock()
                .map_err(|_| {
                    crate::process::ToolIoError::Request("client lock poisoned".to_owned())
                })?
                .push(request.clone());
            match request {
                crate::process::ClientToolRequest::ReadTextFile { .. } => {
                    Ok(json!({"content": self.buffer.lock().expect("buffer").clone()}))
                }
                crate::process::ClientToolRequest::WriteTextFile { content, .. } => {
                    *self.buffer.lock().expect("buffer") = content;
                    Ok(json!({}))
                }
                _ => Ok(json!({})),
            }
        })
    }

    fn supports(&self, capability: crate::process::ClientToolCapability) -> bool {
        self.capabilities
            .lock()
            .expect("capabilities")
            .contains(&capability)
    }
}

async fn registered_workspace_tools_with_client(
    root: &Path,
    client: Option<ClientToolIo>,
) -> ToolRegistry {
    let workspace = Arc::new(Workspace::open(root).expect("workspace"));
    let review = Arc::new(ReviewManager::new(workspace.clone()));
    review.begin_turn_at(0).expect("turn");
    let policy = PermissionStore::default();
    // The reference declares `edit` and `write_file` as `ask`, so a harness
    // exercising their bodies grants them the way an operator's "allow
    // always" does rather than answering a prompt per call.
    for tool in ["edit", "write_file"] {
        policy.set_tool_permission(tool, PermissionMode::Always);
    }
    policy
        .set_trust(root, TrustDecision::Trusted, TrustRootKind::Workspace)
        .await
        .expect("trust");
    let registry = ToolRegistry::default();
    WorkspaceTools::new(workspace, review)
        .with_client_io(client)
        .register(
            &registry,
            &ToolGuard::new(policy.clone(), Arc::new(RejectApproval)),
        )
        .expect("register");
    registry
}

/// The agent reads the buffer the user is looking at rather than the file
/// the workspace last saw.
#[tokio::test]
async fn a_client_hosting_reads_answers_read_file_from_its_buffer() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("main.rs"), "on disk\n").expect("file");
    let client = EditorClient::hosting(
        "unsaved one\nunsaved two\n",
        [crate::process::ClientToolCapability::FilesystemRead],
    );
    let registry = registered_workspace_tools_with_client(
        directory.path(),
        Some(ClientToolIo::new("session-1", client.clone())),
    )
    .await;

    let read = registry
        .invoke(
            "read_file",
            ToolInvocation {
                call_id: "read-1".to_owned(),
                arguments: json!({"file_path": "main.rs"}),
            },
        )
        .await
        .expect("the client answers the read");
    assert_eq!(
        read.typed_result["content"],
        "        1\u{2192}unsaved one\n        2\u{2192}unsaved two"
    );
    assert_eq!(client.methods(), ["clientTool/readTextFile"]);
    // The request carries the confined absolute path, which is the only
    // form a client can resolve, and the result keeps the workspace-
    // relative display the local read reports.
    let expected = crate::platform::canonicalize(directory.path())
        .expect("canonical root")
        .join("main.rs")
        .to_string_lossy()
        .into_owned();
    let requests = client.requests.lock().expect("requests");
    assert!(
        matches!(
            &requests[0],
            crate::process::ClientToolRequest::ReadTextFile { session_id, path, .. }
                if session_id == "session-1" && *path == expected
        ),
        "{requests:?} does not carry {expected}"
    );
}

/// A write the client hosts never touches the workspace, so the file the
/// user has open is the only copy that changed.
#[tokio::test]
async fn a_client_hosting_writes_answers_write_file_and_edit() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("main.rs"), "on disk\n").expect("file");
    let client = EditorClient::hosting(
        "alpha\n",
        [
            crate::process::ClientToolCapability::FilesystemRead,
            crate::process::ClientToolCapability::FilesystemWrite,
        ],
    );
    let registry = registered_workspace_tools_with_client(
        directory.path(),
        Some(ClientToolIo::new("session-1", client.clone())),
    )
    .await;

    registry
        .invoke(
            "write_file",
            ToolInvocation {
                call_id: "write-1".to_owned(),
                arguments: json!({"file_path": "created.rs", "content": "beta\n"}),
            },
        )
        .await
        .expect("the client answers the write");
    assert!(
        !directory.path().join("created.rs").exists(),
        "a delegated write reached the workspace filesystem"
    );

    let edited = registry
        .invoke(
            "edit",
            ToolInvocation {
                call_id: "edit-1".to_owned(),
                arguments: json!({
                    "file_path": "main.rs",
                    "old_string": "beta",
                    "new_string": "gamma",
                }),
            },
        )
        .await
        .expect("the client answers the edit");
    assert_eq!(edited.typed_result["new_string"], "gamma");
    assert!(
        edited.display["diff"]
            .as_str()
            .is_some_and(|diff| diff.contains("+gamma")),
        "{edited:?}"
    );
    assert_eq!(
        *client.buffer.lock().expect("buffer"),
        "gamma\n",
        "the edit was applied to the client's buffer"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("main.rs")).expect("on disk"),
        "on disk\n",
        "a delegated edit rewrote the workspace file"
    );
    assert_eq!(
        client.methods(),
        [
            "clientTool/writeTextFile",
            "clientTool/readTextFile",
            "clientTool/writeTextFile",
        ]
    );
}

/// A client that declared nothing leaves every tool on the workspace, which
/// is what keeps a terminal client from paying for a delegation it cannot
/// answer.
#[tokio::test]
async fn a_client_declaring_no_filesystem_leaves_the_tools_on_the_workspace() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("main.rs"), "on disk\n").expect("file");
    let client = EditorClient::hosting("unsaved\n", []);
    let registry = registered_workspace_tools_with_client(
        directory.path(),
        Some(ClientToolIo::new("session-1", client.clone())),
    )
    .await;

    let read = registry
        .invoke(
            "read_file",
            ToolInvocation {
                call_id: "read-1".to_owned(),
                arguments: json!({"file_path": "main.rs"}),
            },
        )
        .await
        .expect("the workspace answers the read");
    assert_eq!(read.typed_result["content"], "        1\u{2192}on disk");
    registry
        .invoke(
            "write_file",
            ToolInvocation {
                call_id: "write-1".to_owned(),
                arguments: json!({"file_path": "created.rs", "content": "beta\n"}),
            },
        )
        .await
        .expect("the workspace answers the write");
    assert!(
        directory.path().join("created.rs").exists(),
        "the write did not reach the workspace filesystem"
    );
    assert!(
        client.methods().is_empty(),
        "an undeclared capability still reached the client: {:?}",
        client.methods()
    );
}

/// The same, plus the review manager the caller needs to inspect what the
/// write captured for rewind.
async fn registered_workspace_tools_with_review(root: &Path) -> (ToolRegistry, Arc<ReviewManager>) {
    let workspace = Arc::new(Workspace::open(root).expect("workspace"));
    let review = Arc::new(ReviewManager::new(workspace.clone()));
    review.begin_turn_at(0).expect("turn");
    let policy = PermissionStore::default();
    // The reference declares `edit` and `write_file` as `ask`, so a harness
    // exercising their bodies grants them the way an operator's "allow
    // always" does rather than answering a prompt per call.
    for tool in ["edit", "write_file"] {
        policy.set_tool_permission(tool, PermissionMode::Always);
    }
    policy
        .set_trust(root, TrustDecision::Trusted, TrustRootKind::Workspace)
        .await
        .expect("trust");
    let registry = ToolRegistry::default();
    WorkspaceTools::new(workspace, review.clone())
        .register(
            &registry,
            &ToolGuard::new(policy.clone(), Arc::new(RejectApproval)),
        )
        .expect("register");
    (registry, review)
}

/// `write_file` creates a file, creates the parents it needs, and captures
/// what it replaced so the turn stays rewindable.
#[tokio::test]
async fn write_file_creates_missing_parents_and_captures_the_turn_baseline() {
    let directory = tempdir().expect("tempdir");
    let (registry, review) = registered_workspace_tools_with_review(directory.path()).await;

    let created = registry
        .invoke(
            "write_file",
            ToolInvocation {
                call_id: "write-1".to_owned(),
                arguments: json!({"file_path": "nested/deep/new.txt", "content": "alpha\n"}),
            },
        )
        .await
        .expect("a missing parent directory is created");
    assert!(
        created.model_text.contains("nested/deep/new.txt"),
        "{created:?}"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("nested/deep/new.txt")).expect("file"),
        "alpha\n"
    );
    // A file is reviewable only once its pre-write state was captured, so
    // this is the rewind capture observed from outside.
    let state = review.review_state().expect("review state");
    assert_eq!(
        state
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>(),
        [log_key(directory.path(), "nested/deep/new.txt")]
    );
}

/// The reference refuses to overwrite and names `edit` instead, and the
/// refusal still runs after the baseline capture, so the turn can rewind.
#[tokio::test]
async fn write_file_refuses_an_existing_file_and_names_the_edit_tool() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("held.txt"), "original\n").expect("file");
    let (registry, _review) = registered_workspace_tools_with_review(directory.path()).await;

    let refused = registry
        .invoke(
            "write_file",
            ToolInvocation {
                call_id: "write-1".to_owned(),
                arguments: json!({"file_path": "held.txt", "content": "replacement\n"}),
            },
        )
        .await
        .expect_err("an existing file is not overwritten");
    assert!(refused.to_string().contains("already exists"), "{refused}");
    assert!(refused.to_string().contains("edit"), "{refused}");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("held.txt")).expect("file"),
        "original\n"
    );
}

/// The path policy and the write limit both bind `write_file`, which is
/// what keeps a shell-free tool from reaching outside the workspace.
#[tokio::test]
async fn write_file_is_bounded_by_the_workspace_root_and_the_write_limit() {
    let directory = tempdir().expect("tempdir");
    let (registry, _review) = registered_workspace_tools_with_review(directory.path()).await;

    let escaped = registry
        .invoke(
            "write_file",
            ToolInvocation {
                call_id: "write-1".to_owned(),
                arguments: json!({"file_path": "../outside.txt", "content": "no"}),
            },
        )
        .await
        .expect("a path outside the root is asked about and refused");
    assert_eq!(
        escaped.skip,
        Some(crate::tools::ToolSkip { cancelled: true }),
        "{escaped:?}"
    );
    assert!(!directory.path().join("../outside.txt").exists());

    let oversized = registry
        .invoke(
            "write_file",
            ToolInvocation {
                call_id: "write-2".to_owned(),
                arguments: json!({
                    "file_path": "big.txt",
                    "content": "x".repeat(DEFAULT_MAX_READ_BYTES + 1),
                }),
            },
        )
        .await
        .expect_err("a write past the limit is refused");
    assert!(
        oversized.to_string().contains("exceeding the"),
        "{oversized}"
    );
    assert!(!directory.path().join("big.txt").exists());
}

struct ApproveApproval;

impl ApprovalAgent for ApproveApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
    }
}

/// Reference `resolve_tool_path`: once the operator approves the
/// `outside_directory` requirement, every file tool reaches a path outside
/// the root, written absolute, relative or under `~`-free whitespace, and
/// the edit it makes is checkpointed under its absolute path.
#[tokio::test]
async fn an_approved_path_outside_the_root_is_reached_by_every_file_tool() {
    let parent = tempdir().expect("parent");
    let parent_path = crate::platform::canonicalize(parent.path()).expect("canonical");
    let root = parent_path.join("project");
    let outside = parent_path.join("elsewhere");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(outside.join("notes.txt"), "alpha needle\n").expect("seed");

    let workspace = Arc::new(Workspace::open(&root).expect("workspace"));
    let review = Arc::new(ReviewManager::new(workspace.clone()));
    review.begin_turn_at(0).expect("turn");
    let policy = PermissionStore::default();
    policy
        .set_trust(&root, TrustDecision::Trusted, TrustRootKind::Workspace)
        .await
        .expect("trust");
    let registry = ToolRegistry::default();
    WorkspaceTools::new(workspace, review.clone())
        .register(
            &registry,
            &ToolGuard::new(policy, Arc::new(ApproveApproval)),
        )
        .expect("register");
    let invoke = |name: &'static str, arguments: serde_json::Value| {
        let registry = &registry;
        async move {
            registry
                .invoke(
                    name,
                    ToolInvocation {
                        call_id: format!("{name}-call"),
                        arguments,
                    },
                )
                .await
        }
    };
    let notes = outside.join("notes.txt").to_string_lossy().into_owned();

    let read = invoke("read_file", json!({"file_path": format!("  {notes} ")}))
        .await
        .expect("an approved outside read lands");
    assert_eq!(read.typed_result["file_path"], json!(notes));
    assert!(read.model_text.contains("alpha needle"), "{read:?}");

    let found = invoke("grep", json!({"pattern": "needle", "path": "../elsewhere"}))
        .await
        .expect("an approved outside search lands");
    assert_eq!(found.typed_result["match_count"], json!(1));

    invoke(
        "edit",
        json!({"file_path": notes, "old_string": "alpha", "new_string": "beta"}),
    )
    .await
    .expect("an approved outside edit lands");
    assert_eq!(
        std::fs::read_to_string(outside.join("notes.txt")).expect("edited"),
        "beta needle\n"
    );
    invoke(
        "write_file",
        json!({"file_path": "../elsewhere/new/created.txt", "content": "made\n"}),
    )
    .await
    .expect("an approved outside write lands");
    assert_eq!(
        std::fs::read_to_string(outside.join("new/created.txt")).expect("written"),
        "made\n"
    );

    let tracked = review
        .with_log(|log| log.history().tracked_paths())
        .expect("log");
    assert!(
        tracked.contains(&notes),
        "the outside edit is tracked under its absolute path: {tracked:?}"
    );
}

// ----------------------------------------------------------------
// EP-034: the builtin bodies
// ----------------------------------------------------------------

/// US-112: the walk honors `.gitignore` and `.ignore` inside a repository
/// and drops both when the call asks, while the configured exclusion globs
/// apply either way.
#[tokio::test]
async fn grep_separates_the_ignore_files_from_the_configured_exclusions() {
    let directory = tempdir().expect("tempdir");
    std::fs::create_dir(directory.path().join(".git")).expect("git marker");
    std::fs::create_dir(directory.path().join("build")).expect("build");
    std::fs::write(directory.path().join(".gitignore"), "gitignored.txt\n").expect("gitignore");
    std::fs::write(directory.path().join(".ignore"), "plainignored.txt\n").expect("ignore");
    for name in [
        "kept.txt",
        "gitignored.txt",
        "plainignored.txt",
        "build/out.txt",
    ] {
        std::fs::write(directory.path().join(name), "needle\n").expect("seed");
    }
    let (registry, _) = registered_with_settings(directory.path(), "").await;

    let honored = registry
        .invoke(
            "grep",
            ToolInvocation {
                call_id: "grep-1".to_owned(),
                arguments: json!({"pattern": "needle"}),
            },
        )
        .await
        .expect("search");
    assert_eq!(
        honored.typed_result["matches"], "./kept.txt:1:needle",
        "both ignore files and the declared `build/` exclusion apply"
    );

    let unfiltered = registry
        .invoke(
            "grep",
            ToolInvocation {
                call_id: "grep-2".to_owned(),
                arguments: json!({"pattern": "needle", "use_default_ignore": false}),
            },
        )
        .await
        .expect("search");
    let matches = unfiltered.typed_result["matches"]
        .as_str()
        .expect("a match list");
    assert!(matches.contains("./gitignored.txt:1:needle"), "{matches}");
    assert!(matches.contains("./plainignored.txt:1:needle"), "{matches}");
    assert!(
        !matches.contains("build/out.txt"),
        "the configured exclusions survive `use_default_ignore: false`: {matches}"
    );
}

/// US-112: a binary file is walked past rather than matched or raised on,
/// and a hidden file is skipped the way `rg` skips one.
#[tokio::test]
async fn grep_skips_a_binary_file_without_failing_the_call() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("text.txt"), "needle\n").expect("text");
    std::fs::write(directory.path().join("blob.bin"), b"needle\0needle\n").expect("binary");
    let (registry, _) = registered_with_settings(directory.path(), "").await;

    let matched = registry
        .invoke(
            "grep",
            ToolInvocation {
                call_id: "grep-1".to_owned(),
                arguments: json!({"pattern": "needle"}),
            },
        )
        .await
        .expect("a binary file is not an error");

    assert_eq!(matched.typed_result["matches"], "./text.txt:1:needle");
}

/// US-112: a search that runs out of time answers nothing rather than
/// returning what it had as though it were complete.
#[test]
fn a_search_past_its_timeout_discards_the_partial_answer() {
    let directory = tempdir().expect("tempdir");
    for index in 0..64 {
        std::fs::write(directory.path().join(format!("f{index}.txt")), "needle\n").expect("seed");
    }
    let workspace = Workspace::open(directory.path()).expect("workspace");
    let options = SearchOptions {
        timeout: Some(Duration::ZERO),
        ..SearchOptions::default()
    };

    let error = workspace
        .search("needle", ".", &options)
        .expect_err("an expired budget answers nothing");

    assert!(error.to_string().contains("did not finish"), "{error}");
}

/// Reference `Grep._drop_sensitive_matches`: a match found in a file a
/// sensitive pattern names is left out, whether the call names the file
/// itself or walks a directory holding it, and the case of the name does
/// not matter.
#[test]
fn grep_drops_the_matches_a_sensitive_pattern_names() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join(".env"), "TOKEN=needle\n").expect("seed");
    std::fs::write(directory.path().join("key.PEM"), "needle\n").expect("seed");
    std::fs::write(directory.path().join("notes.txt"), "needle\n").expect("seed");
    let workspace = Workspace::open(directory.path()).expect("workspace");

    let named = workspace
        .search("needle", ".env", &SearchOptions::default())
        .expect("search");
    assert_eq!(named.matches, "");
    assert_eq!(named.match_count, 0);

    let options = SearchOptions {
        sensitive_patterns: vec!["**/*.pem".to_owned()],
        ..SearchOptions::default()
    };
    let walked = workspace.search("needle", ".", &options).expect("search");
    assert_eq!(walked.matches, "./notes.txt:1:needle");
    assert_eq!(walked.match_count, 1);

    let unfiltered = SearchOptions {
        sensitive_patterns: Vec::new(),
        ..SearchOptions::default()
    };
    let named = workspace
        .search("needle", ".env", &unfiltered)
        .expect("search");
    assert_eq!(named.matches, ".env:1:TOKEN=needle");
}

/// US-113: a subdirectory's `AGENTS.md` reaches the model once, and the
/// second read of the same directory pays for it again in neither the text
/// nor the progress stream.
#[tokio::test]
async fn a_subdirectory_instruction_file_is_injected_once_per_session() {
    let directory = tempdir().expect("tempdir");
    std::fs::create_dir_all(directory.path().join("nested/deeper")).expect("nested");
    std::fs::write(directory.path().join("AGENTS.md"), "the root rules\n").expect("root");
    std::fs::write(
        directory.path().join("nested/AGENTS.md"),
        "the nested rules\n",
    )
    .expect("nested rules");
    std::fs::write(directory.path().join("nested/deeper/main.rs"), "body\n").expect("file");
    let (registry, _) = registered_with_settings(directory.path(), "").await;

    let invocation = || ToolInvocation {
        call_id: "read-1".to_owned(),
        arguments: json!({"file_path": "nested/deeper/main.rs"}),
    };
    let first = registry
        .invoke("read_file", invocation())
        .await
        .expect("read");
    assert!(
        first.model_text.contains("the nested rules"),
        "{}",
        first.model_text
    );
    assert!(
        !first.model_text.contains("the root rules"),
        "the root's own instructions travel with the session prompt, not with a read: {}",
        first.model_text
    );
    assert_eq!(
        first.display["discovered"],
        json!([crate::platform::canonicalize(directory.path())
            .expect("canonical")
            .join("nested")
            .display()
            .to_string()])
    );

    let second = registry
        .invoke("read_file", invocation())
        .await
        .expect("read");
    assert!(
        !second.model_text.contains("the nested rules"),
        "{}",
        second.model_text
    );
    assert_eq!(second.display["discovered"], json!([]));
}

/// US-113: the three ways a path can be unreadable each name their own
/// cause, so a model can tell a typo from a directory.
#[tokio::test]
async fn read_file_names_each_unreadable_path_for_what_it_is() {
    let directory = tempdir().expect("tempdir");
    std::fs::create_dir(directory.path().join("nested")).expect("nested");
    let (registry, _) = registered_with_settings(directory.path(), "").await;

    let mut messages = Vec::new();
    for (index, path) in ["", "nowhere.txt", "nested"].iter().enumerate() {
        let error = registry
            .invoke(
                "read_file",
                ToolInvocation {
                    call_id: format!("read-{index}"),
                    arguments: json!({"file_path": path}),
                },
            )
            .await
            .expect_err("an unreadable path is refused");
        messages.push(error.to_string());
    }

    assert!(messages[0].contains("file_path"), "{:?}", messages[0]);
    assert!(messages[1].contains("no file exists"), "{:?}", messages[1]);
    assert!(messages[2].contains("is a directory"), "{:?}", messages[2]);
    assert_eq!(
        messages.iter().collect::<BTreeSet<_>>().len(),
        3,
        "each cause needs its own message: {messages:?}"
    );
}

/// US-114: `write_file` refuses an existing file before it opens anything,
/// and the create-new open refuses it again for a file that appeared in
/// between.
#[tokio::test]
async fn write_file_refuses_an_existing_file_before_and_during_the_write() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("taken.txt"), "original\n").expect("seed");
    let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
    let review = ReviewManager::new(workspace.clone());
    review.begin_turn_at(0).expect("turn");
    let (registry, _) = registered_with_settings(directory.path(), "").await;

    let refused = registry
        .invoke(
            "write_file",
            ToolInvocation {
                call_id: "write-1".to_owned(),
                arguments: json!({"file_path": "taken.txt", "content": "replacement\n"}),
            },
        )
        .await
        .expect_err("an existing file is refused");
    assert!(refused.to_string().contains("already exists"), "{refused}");

    // The same refusal without the pre-check, which is the path a file
    // created between the check and the open would take.
    let raced = review
        .write("taken.txt", b"replacement\n")
        .expect_err("the create-new open refuses it too");
    assert!(raced.to_string().contains("already exists"), "{raced}");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("taken.txt")).expect("unchanged"),
        "original\n"
    );
}

/// US-114: a file written in another codec with another line ending keeps
/// both, so a one-line change does not rewrite the whole file.
#[tokio::test]
async fn edit_preserves_the_encoding_and_the_line_ending_it_found() {
    let directory = tempdir().expect("tempdir");
    // "café\r\nlatin\r\n" in Latin-1: the accented byte is 0xE9, which is
    // not valid UTF-8 on its own.
    std::fs::write(
        directory.path().join("legacy.txt"),
        b"caf\xe9\r\nlatin\r\n".as_slice(),
    )
    .expect("seed");
    let (registry, _) = registered_with_settings(directory.path(), "").await;

    registry
        .invoke(
            "edit",
            ToolInvocation {
                call_id: "edit-1".to_owned(),
                arguments: json!({
                    "file_path": "legacy.txt",
                    "old_string": "latin",
                    "new_string": "changed",
                }),
            },
        )
        .await
        .expect("the edit applies");

    assert_eq!(
        std::fs::read(directory.path().join("legacy.txt")).expect("read back"),
        b"caf\xe9\r\nchanged\r\n".as_slice()
    );
}

/// US-114: the four ways an edit can be refused each name their own cause.
#[tokio::test]
async fn edit_names_each_refusal_for_what_it_is() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("file.txt"), "one\none\n").expect("seed");
    let (registry, _) = registered_with_settings(directory.path(), "").await;

    let cases = [
        (
            json!({"file_path": "file.txt", "old_string": "", "new_string": "x"}),
            "empty",
        ),
        (
            json!({"file_path": "file.txt", "old_string": "one", "new_string": "one"}),
            "identical",
        ),
        (
            json!({"file_path": "file.txt", "old_string": "absent", "new_string": "x"}),
            "is stale",
        ),
        (
            json!({"file_path": "file.txt", "old_string": "one", "new_string": "two"}),
            "matches 2 locations",
        ),
    ];
    for (index, (arguments, expected)) in cases.into_iter().enumerate() {
        let error = registry
            .invoke(
                "edit",
                ToolInvocation {
                    call_id: format!("edit-{index}"),
                    arguments,
                },
            )
            .await
            .expect_err("the edit is refused");
        assert!(error.to_string().contains(expected), "{error}");
    }
}

/// US-114: two edits of one file serialize, so the second reads what the
/// first wrote rather than the original, and neither sees a half-written
/// file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_edits_of_one_file_serialize_rather_than_losing_one() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("file.txt"), "alpha beta\n").expect("seed");
    let (registry, _) = registered_with_settings(directory.path(), "").await;
    let registry = Arc::new(registry);

    let first = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .invoke(
                    "edit",
                    ToolInvocation {
                        call_id: "edit-1".to_owned(),
                        arguments: json!({
                            "file_path": "file.txt",
                            "old_string": "alpha",
                            "new_string": "ALPHA",
                        }),
                    },
                )
                .await
        })
    };
    let second = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .invoke(
                    "edit",
                    ToolInvocation {
                        call_id: "edit-2".to_owned(),
                        arguments: json!({
                            "file_path": "file.txt",
                            "old_string": "beta",
                            "new_string": "BETA",
                        }),
                    },
                )
                .await
        })
    };
    first.await.expect("join").expect("the first edit applies");
    second
        .await
        .expect("join")
        .expect("the second edit applies");

    assert_eq!(
        std::fs::read_to_string(directory.path().join("file.txt")).expect("read back"),
        "ALPHA BETA\n",
        "one of the two edits was written over the other"
    );
}

/// US-114: the file an edit is about to change is snapshotted before the
/// handler runs, so a revert restores what was there.
#[tokio::test]
async fn a_mutating_call_snapshots_the_file_before_it_changes_it() {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join("file.txt"), "before\n").expect("seed");
    let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
    let review = Arc::new(ReviewManager::new(workspace.clone()));
    review.begin_turn_at(0).expect("turn");
    let policy = PermissionStore::default();
    policy.set_tool_permission("edit", PermissionMode::Always);
    policy
        .set_trust(
            directory.path(),
            TrustDecision::Trusted,
            TrustRootKind::Workspace,
        )
        .await
        .expect("trust");
    let registry = ToolRegistry::default();
    WorkspaceTools::new(workspace, review.clone())
        .register(
            &registry,
            &ToolGuard {
                policy: policy.clone(),
                approval: Arc::new(RejectApproval),
                config: policy.tool_config(),
                scratchpad: None,
            },
        )
        .expect("register");

    registry
        .invoke(
            "edit",
            ToolInvocation {
                call_id: "edit-1".to_owned(),
                arguments: json!({
                    "file_path": "file.txt",
                    "old_string": "before",
                    "new_string": "after",
                }),
            },
        )
        .await
        .expect("the edit applies");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("file.txt")).expect("changed"),
        "after\n"
    );

    review.seal_turn().expect("the turn seals");
    review.revert_review(&FIRST_TURN).expect("the turn reverts");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("file.txt")).expect("restored"),
        "before\n"
    );
}
