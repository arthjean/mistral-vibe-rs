//! The worktree half of the app-server contract, driven through the
//! connection a client speaks to.
//!
//! Every case here scripts a real checkout, so the worktrees the listing
//! answers with and the ones a session start mints are git's own rather than a
//! fixture's idea of them.

use super::*;
use std::path::PathBuf;
use std::process::Command;

use crate::workspace::WorkspacePaths;
use crate::worktrees;
use vibe_core::worktree::lifecycle::SessionWorktrees;
use vibe_core::worktree::{ManagedRoot, ManagedWorktree, PreparedWorktree, WorktreeError};

/// The home a scripted case reads and writes under, so no test reaches the
/// operator's own worktrees or their `~/.vibe`.
fn service(root: &Path) -> WorkspaceService {
    WorkspaceService::new(
        WorkspacePaths {
            vibe_home: root.join("vibe-home"),
            working_directory: root.to_path_buf(),
            session_root: root.join("vibe-home/sessions"),
        },
        false,
    )
    .expect("the workspace service is built")
}

fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .expect("git is on PATH");
    assert!(
        output.status.success(),
        "git {arguments:?} failed in {}: {}",
        directory.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn text(path: &Path) -> &str {
    path.to_str().expect("a scripted path is UTF-8")
}

/// A case root with no symbolic links in it, so a canonicalized checkout root
/// compares equal to the path a response carries.
fn case_root() -> (tempfile::TempDir, PathBuf) {
    let scratch = tempfile::tempdir().expect("tempdir");
    let root = scratch
        .path()
        .canonicalize()
        .expect("the case root resolves");
    (scratch, root)
}

/// One committed checkout at `root/repo`.
fn checkout(root: &Path) -> PathBuf {
    let checkout = root.join("repo");
    fs::create_dir_all(&checkout).expect("the checkout is writable");
    git(
        &checkout,
        &["init", "--quiet", "--initial-branch", "main", "."],
    );
    git(&checkout, &["config", "user.name", "Vibe Test"]);
    git(&checkout, &["config", "user.email", "vibe@example.test"]);
    git(&checkout, &["config", "commit.gpgsign", "false"]);
    fs::write(checkout.join("README.md"), "fixture\n").expect("the fixture is writable");
    git(&checkout, &["add", "--all"]);
    git(
        &checkout,
        &["commit", "--quiet", "--no-gpg-sign", "-m", "fixture"],
    );
    checkout
}

fn linked_worktree(checkout: &Path, root: &Path, name: &str, branch: &str) -> PathBuf {
    let target = root.join(name);
    git(checkout, &["worktree", "add", "-b", branch, text(&target)]);
    target
}

fn branch_is_present(checkout: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .expect("git is on PATH")
        .status
        .success()
}

/// How many worktrees the checkout has, its own included.
///
/// Where a created worktree lands is the managed root's business, one layer
/// down; what a case here asserts is whether the checkout still knows about one,
/// which is what a client would see next.
fn worktree_count(checkout: &Path) -> usize {
    git(checkout, &["worktree", "list", "--porcelain"])
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count()
}

fn answer(connection: &mut ServerConnection, id: i64, method: &str, params: Value) -> Value {
    let batch = connection.dispatch(&request(id, method, params));
    match decode_frame(&batch.outbound[0]).expect("an answer") {
        Envelope::Success(SuccessResponse { result, .. }) => {
            Value::Object(result.into_iter().collect())
        }
        other => unreachable!("{method} did not answer: {other:?}"),
    }
}

fn refusal(
    connection: &mut ServerConnection,
    id: i64,
    method: &str,
    params: Value,
) -> ProtocolError {
    let batch = connection.dispatch(&request(id, method, params));
    match decode_frame(&batch.outbound[0]).expect("an answer") {
        Envelope::Error(ErrorResponse { error, .. }) => error,
        other => unreachable!("{method} was not refused: {other:?}"),
    }
}

fn managed(root: &Path) -> ManagedRoot {
    ManagedRoot::for_vibe_home(&root.join("vibe-home"))
}

/// A managed worktree prepared and then let go, as one whose session ended.
fn abandoned_worktree(root: &Path, checkout: &Path, name: &str, branch: &str) -> PreparedWorktree {
    let prepared = vibe_core::worktree::WorktreeRepository::open(checkout, &managed(root))
        .and_then(|repository| repository.prepare(name, Some(branch)))
        .expect("the worktree is prepared");
    let held = ManagedWorktree::at(&managed(root), &prepared.path).expect("managed");
    held.hold("finished", prepared.pending_hold.as_ref())
        .expect("held");
    held.release_holder("finished").expect("released");
    prepared
}

fn started_cwd(answer: &Value) -> PathBuf {
    PathBuf::from(
        answer["state"]["session"]["cwd"]
            .as_str()
            .expect("the session names its directory"),
    )
}

fn connected(root: &Path) -> ServerConnection {
    let server = AppServer::with_workspace_service(service(root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    connection
}

// --------------------------------------------------------------------------
// workspace/git/worktrees/list
// --------------------------------------------------------------------------

#[test]
fn the_listing_answers_one_entry_per_linked_worktree() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let linked = linked_worktree(&checkout, &root, "review", "topic");
    let mut connection = connected(&root);

    let answer = answer(
        &mut connection,
        2,
        "workspace/git/worktrees/list",
        json!({"cwd": text(&checkout)}),
    );
    assert_eq!(
        answer,
        json!({
            "worktrees": [{
                "name": "review",
                "branch": "topic",
                "cwd": text(&linked),
                "root": text(&linked),
                "repoRoot": text(&checkout),
                "branchChanges": null,
            }],
            "repositoryBranch": null,
            "repositoryCwd": text(&checkout),
            "repositoryMappedCwd": text(&checkout),
            "repositoryRoot": text(&checkout),
        }),
    );
}

/// The details cost a merge base per branch, so they are paid only when a
/// caller asks: the lines each branch changed and the main checkout's branch.
#[test]
fn the_listing_reports_details_only_when_asked() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let linked = linked_worktree(&checkout, &root, "review", "topic");
    fs::write(linked.join("notes.txt"), "one\ntwo\n").expect("the worktree is writable");
    git(&linked, &["add", "notes.txt"]);
    git(
        &linked,
        &["commit", "--quiet", "--no-gpg-sign", "-m", "notes"],
    );
    let mut connection = connected(&root);

    let answer = answer(
        &mut connection,
        2,
        "workspace/git/worktrees/list",
        json!({"cwd": text(&checkout), "includeDetails": true}),
    );
    assert_eq!(answer["repositoryBranch"], json!("main"));
    assert_eq!(
        answer["worktrees"][0]["branchChanges"],
        json!({"additions": 2, "deletions": 0}),
    );
}

/// The method is a host method: it takes no `sessionId` and answers before any
/// session has been opened, which is what makes a worktree picker reachable at
/// the moment a client is deciding where to open one.
#[test]
fn the_listing_answers_before_any_session_exists() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let mut connection = connected(&root);

    let answer = answer(
        &mut connection,
        2,
        "workspace/git/worktrees/list",
        json!({"cwd": text(&checkout)}),
    );
    assert_eq!(answer["worktrees"], json!([]));
}

#[test]
fn a_path_outside_a_repository_lists_nothing_rather_than_refusing() {
    let (_scratch, root) = case_root();
    let mut connection = connected(&root);

    let answer = answer(
        &mut connection,
        2,
        "workspace/git/worktrees/list",
        json!({"cwd": text(&root)}),
    );
    assert_eq!(
        answer,
        json!({
            "worktrees": [],
            "repositoryBranch": null,
            "repositoryCwd": null,
            "repositoryMappedCwd": null,
            "repositoryRoot": null,
        }),
    );
}

/// A host without git is the second reason the listing answers empty, and it
/// cannot be scripted from a test that must not touch the process environment:
/// the spawn failure is asserted on the arm that swallows it instead.
#[test]
fn a_host_without_git_lists_nothing_rather_than_refusing() {
    let unavailable: Result<Option<()>, _> = Err(WorktreeError::GitUnavailable(
        "git is not on PATH".to_owned(),
    ));
    assert_eq!(
        worktrees::swallow_missing_checkout(unavailable).expect("git being absent is not an error"),
        None,
    );
    let refused: Result<Option<()>, _> = Err(WorktreeError::ListFailed {
        message: "git refused".to_owned(),
    });
    assert!(worktrees::swallow_missing_checkout(refused).is_err());
}

#[test]
fn the_worktree_methods_are_advertised_and_the_retired_listing_is_not() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    let response = initialize_with(&mut connection, json!({}));
    let methods = response["capabilities"]["methods"]
        .as_array()
        .expect("the handshake advertises its methods");
    for method in [
        "workspace/git/worktrees/limit/update",
        "workspace/git/worktrees/list",
        "workspace/git/worktrees/prune",
        "workspace/git/worktrees/remove",
    ] {
        assert!(methods.contains(&json!(method)), "{method}");
    }
    assert!(!methods.contains(&json!("workspace/worktrees/list")));
}

// --------------------------------------------------------------------------
// worktree on session/start
// --------------------------------------------------------------------------

#[test]
fn an_existing_request_opens_the_session_in_the_linked_worktree() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let linked = linked_worktree(&checkout, &root, "review", "topic");
    let mut connection = connected(&root);

    let answer = answer(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "worktree": {"kind": "existing", "cwd": text(&linked)},
        }),
    );
    assert_eq!(answer["state"]["session"]["cwd"], json!(text(&linked)));
    assert_eq!(
        answer["state"]["session"]["workspaceRoots"],
        json!([text(&linked)]),
    );
}

/// Moving into a worktree replaces the checkout root, not the other
/// directories the client authorized.
#[test]
fn a_moved_session_keeps_the_extra_roots_it_was_given() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let linked = linked_worktree(&checkout, &root, "review", "topic");
    let extra = root.join("attachments");
    fs::create_dir_all(&extra).expect("the extra root is writable");
    let mut connection = connected(&root);

    let answer = answer(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "workspaceRoots": [text(&checkout), text(&extra)],
            "worktree": {"kind": "existing", "cwd": text(&linked)},
        }),
    );
    assert_eq!(
        answer["state"]["session"]["workspaceRoots"],
        json!([text(&linked), text(&extra)]),
    );
}

#[test]
fn an_existing_request_outside_the_checkout_is_refused_by_its_path() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let stranger = root.join("stranger");
    fs::create_dir_all(&stranger).expect("the stranger is writable");
    let mut connection = connected(&root);

    let error = refusal(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "worktree": {"kind": "existing", "cwd": text(&stranger)},
        }),
    );
    assert_eq!(error.code, ProtocolErrorCode::InvalidParams);
    assert!(
        error.message.contains(text(&stranger)),
        "the refusal names the path: {}",
        error.message
    );
}

#[test]
fn an_empty_request_field_is_refused() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let mut connection = connected(&root);

    let error = refusal(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "worktree": {"kind": "create", "name": "", "branch": "topic"},
        }),
    );
    assert_eq!(error.code, ProtocolErrorCode::InvalidParams);
    assert_eq!(worktree_count(&checkout), 1);
}

#[test]
fn a_create_request_mints_the_named_worktree_on_the_named_branch() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let home = root.join("vibe-home");
    let mut connection = connected(&root);

    let answer = answer(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "worktree": {"kind": "create", "name": "review", "branch": "topic"},
        }),
    );
    let minted = started_cwd(&answer);
    assert_eq!(
        minted.file_name().and_then(|name| name.to_str()),
        Some("review")
    );
    assert!(
        minted.starts_with(&home),
        "the worktree lands under the vibe home"
    );
    assert!(minted.is_dir(), "the worktree is on disk");
    assert_eq!(
        git(&minted, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "topic"
    );
}

/// An `auto` request with no model to ask names the worktree after its prompt,
/// on a `vibe/` branch.
#[test]
fn an_auto_request_names_the_worktree_after_its_prompt() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let mut connection = connected(&root);

    let answer = answer(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "worktree": {"kind": "auto", "prompt": "Fix the login redirect"},
        }),
    );
    let minted = started_cwd(&answer);
    let name = minted
        .file_name()
        .and_then(|name| name.to_str())
        .expect("the worktree has a name")
        .to_owned();
    assert!(name.starts_with("fix-the-login-redirect"), "{name}");
    assert_eq!(
        git(&minted, &["rev-parse", "--abbrev-ref", "HEAD"]),
        format!("vibe/{name}")
    );
}

#[test]
fn a_session_started_in_a_managed_worktree_holds_it() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let mut connection = connected(&root);

    let answer = answer(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "worktree": {"kind": "create", "name": "review", "branch": "topic"},
        }),
    );
    let minted = started_cwd(&answer);
    let held = ManagedWorktree::at(&managed(&root), &minted).expect("the worktree is managed");
    assert_eq!(
        held.holders().into_iter().collect::<Vec<_>>(),
        vec!["session-1".to_owned()]
    );
}

#[test]
fn a_create_request_git_refuses_creates_nothing() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let mut connection = connected(&root);

    let error = refusal(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "worktree": {"kind": "create", "name": "review", "branch": "..bad"},
        }),
    );
    assert_eq!(error.code, ProtocolErrorCode::InvalidParams);
    assert_eq!(worktree_count(&checkout), 1);
    assert!(!branch_is_present(&checkout, "..bad"));

    let unportable = refusal(
        &mut connection,
        3,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "worktree": {"kind": "create", "name": "nested/name", "branch": "topic"},
        }),
    );
    assert_eq!(unportable.code, ProtocolErrorCode::InvalidParams);
    assert!(!branch_is_present(&checkout, "topic"));
}

#[test]
fn a_base_that_is_not_a_directory_is_refused_by_its_path() {
    let (_scratch, root) = case_root();
    let absent = root.join("absent");
    let mut connection = connected(&root);

    let error = refusal(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&absent),
            "worktree": {"kind": "create", "name": "review", "branch": "topic"},
        }),
    );
    assert_eq!(error.code, ProtocolErrorCode::InvalidParams);
    assert!(
        error.message.contains(text(&absent)),
        "the refusal names the base: {}",
        error.message
    );
}

/// A session start with no request resolves nothing, which is what keeps
/// every other case in this suite reading the directory it asked for.
#[test]
fn a_start_without_a_request_keeps_the_directory_it_was_given() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let mut connection = connected(&root);

    let answer = answer(
        &mut connection,
        2,
        "session/start",
        json!({"sessionId": "session-1", "cwd": text(&checkout)}),
    );
    assert_eq!(answer["state"]["session"]["cwd"], json!(text(&checkout)));
}

#[test]
fn a_resolved_request_is_cleared_from_the_options() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let linked = linked_worktree(&checkout, &root, "review", "topic");
    let server = AppServer::with_workspace_service(service(&root));
    let connection = server.connect(TransportKind::InProcess);

    let mut params: SessionStartParams = serde_json::from_value(json!({
        "sessionId": "session-1",
        "cwd": text(&checkout),
        "worktree": {"kind": "existing", "cwd": text(&linked)},
    }))
    .expect("the parameters deserialize");
    let Ok(resolution) = connection.resolve_worktree(&mut params) else {
        unreachable!("the request resolves")
    };

    assert!(
        resolution.created().is_none(),
        "an existing worktree was not created"
    );
    assert!(params.worktree.is_none());
    assert_eq!(params.working_directory.as_deref(), Some(text(&linked)));
    assert_eq!(params.add_directories, vec![text(&linked).to_owned()]);
}

/// An absent `cwd` means the app-server's own directory, which is the runtime
/// default a non-desktop client relies on. Both spellings of it resolve to the
/// same base, which is what the resolution asserts rather than naming a
/// directory the test process does not control.
#[test]
fn an_absent_cwd_resolves_the_request_against_the_process_directory() {
    let (_scratch, root) = case_root();
    let lifecycle = SessionWorktrees::new(managed(&root));
    let input = serde_json::from_value(json!({
        "kind": "existing",
        "cwd": "worktree-no-checkout-links",
    }))
    .expect("the request deserializes");

    let absent = worktrees::resolve(Some(&input), None, &lifecycle, |_| None)
        .expect_err("no checkout links that directory");
    let dot = worktrees::resolve(Some(&input), Some("."), &lifecycle, |_| None)
        .expect_err("no checkout links that directory");
    assert_eq!(absent.to_string(), dot.to_string());
}

#[test]
fn a_request_is_refused_on_resume_and_on_continue() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let mut connection = connected(&root);

    for (id, method) in [(2, "session/resume"), (3, "session/continue")] {
        let error = refusal(
            &mut connection,
            id,
            method,
            json!({
                "sessionId": "saved",
                "cwd": text(&checkout),
                "worktree": {"kind": "create", "name": "review", "branch": "topic"},
            }),
        );
        assert_eq!(error.code, ProtocolErrorCode::InvalidParams, "{method}");
        // Neither method declares a worktree, so the issues name it as a key
        // the request may not carry.
        let detail = serde_json::to_string(&error).expect("the refusal serializes");
        assert!(
            detail.contains("worktree"),
            "{method} names what it refused: {detail}"
        );
    }
}

// --------------------------------------------------------------------------
// What a failed start and a close take back
// --------------------------------------------------------------------------

/// A start that fails after the worktree exists leaves neither the worktree nor
/// the branch it minted. `historyLimit` is the refusal used because it is
/// resolved after the request and before anything else, so the failure lands
/// exactly in the span the cleanup covers.
#[test]
fn a_failed_start_takes_back_the_worktree_and_the_branch_it_created() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let mut connection = connected(&root);

    let error = refusal(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "historyLimit": 0,
            "worktree": {"kind": "create", "name": "review", "branch": "topic"},
        }),
    );
    assert_eq!(error.code, ProtocolErrorCode::InvalidParams);
    assert!(error.message.contains("historyLimit"));
    assert_eq!(worktree_count(&checkout), 1);
    assert!(!branch_is_present(&checkout, "topic"));
}

#[test]
fn a_failed_start_leaves_a_branch_it_did_not_create() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    git(&checkout, &["branch", "topic"]);
    let mut connection = connected(&root);

    refusal(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "historyLimit": 0,
            "worktree": {"kind": "create", "name": "review", "branch": "topic"},
        }),
    );
    assert_eq!(worktree_count(&checkout), 1);
    assert!(branch_is_present(&checkout, "topic"));
}

#[test]
fn a_failed_start_leaves_a_worktree_it_only_selected() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let linked = linked_worktree(&checkout, &root, "review", "topic");
    let mut connection = connected(&root);

    refusal(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "historyLimit": 0,
            "worktree": {"kind": "existing", "cwd": text(&linked)},
        }),
    );
    assert!(
        linked.is_dir(),
        "a worktree this start did not create survives"
    );
    assert!(branch_is_present(&checkout, "topic"));
}

/// A removal that fails is reported rather than raised: the client is owed the
/// error that failed the start, and the removal failure becomes a diagnostic.
#[test]
fn a_removal_that_fails_is_reported_rather_than_raised() {
    let (_scratch, root) = case_root();
    let lifecycle = SessionWorktrees::new(managed(&root));
    let orphan = PreparedWorktree {
        name: "review".to_owned(),
        branch: "topic".to_owned(),
        root: root.join("review"),
        path: root.join("review"),
        repo_root: root.join("no-checkout-here"),
        base_commit: "0".repeat(40),
        created: true,
        branch_created: true,
        pending_hold: None,
    };
    let note = lifecycle
        .cleanup(Some(&orphan), None)
        .expect("the removal failed");
    assert!(
        note.contains("review"),
        "the note names the worktree: {note}"
    );

    let selected = PreparedWorktree {
        created: false,
        ..orphan
    };
    assert!(
        lifecycle.cleanup(Some(&selected), None).is_none(),
        "a worktree this start did not create is never removed"
    );
}

/// A session that created its worktree and closes before any turn ran takes
/// the worktree and its branch back, so an abandoned start leaves nothing.
#[test]
fn closing_an_unstarted_session_takes_back_the_worktree_it_created() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let mut connection = connected(&root);

    let started = answer(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "worktree": {"kind": "create", "name": "review", "branch": "topic"},
        }),
    );
    let minted = started_cwd(&started);
    assert!(minted.is_dir());

    answer(
        &mut connection,
        3,
        "session/close",
        json!({"sessionId": "session-1"}),
    );
    assert!(!minted.exists(), "the unused worktree is taken back");
    assert!(!branch_is_present(&checkout, "topic"));
}

/// Closing a session in a worktree it did not create only drops its holder.
#[test]
fn closing_a_session_releases_a_worktree_it_did_not_create() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let prepared = abandoned_worktree(&root, &checkout, "review", "topic");
    let mut connection = connected(&root);

    answer(
        &mut connection,
        2,
        "session/start",
        json!({"sessionId": "session-1", "cwd": text(&prepared.path)}),
    );
    let held = ManagedWorktree::at(&managed(&root), &prepared.path).expect("managed");
    assert!(held.holders().contains("session-1"));

    answer(
        &mut connection,
        3,
        "session/close",
        json!({"sessionId": "session-1"}),
    );
    assert!(prepared.path.is_dir(), "the worktree outlives the session");
    assert!(held.holders().is_empty());
}

/// `session/start` also carries the two reopening intents, and each one hands
/// the session the directory its recorded session was written against. A
/// request resolved on the way to one would mint a worktree the session never
/// opens and no failure path takes back, so the refusal covers the flags as
/// well as the two methods above.
#[test]
fn a_request_is_refused_on_a_start_that_reopens_a_recorded_session() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let elsewhere = root.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("the recorded directory is writable");
    vibe_core::storage::SessionStore::new(root.join("vibe-home/sessions"))
        .create("saved", &elsewhere.to_string_lossy(), None, 10)
        .expect("the recorded session is written");
    let mut connection = connected(&root);

    for (id, reopening) in [
        (2, json!({"resume": "saved"})),
        (3, json!({"continue": true})),
    ] {
        let mut params = json!({
            "sessionId": "saved",
            "cwd": text(&checkout),
            "worktree": {"kind": "create", "name": "review", "branch": "topic"},
        });
        for (key, value) in reopening.as_object().expect("the reopening flag") {
            params[key] = value.clone();
        }
        let error = refusal(&mut connection, id, "session/start", params);
        assert_eq!(error.code, ProtocolErrorCode::InvalidParams, "{reopening}");
        assert!(
            error.message.contains("worktree"),
            "{reopening} names what it refused: {}",
            error.message
        );
        assert_eq!(worktree_count(&checkout), 1, "{reopening} minted nothing");
        assert!(!branch_is_present(&checkout, "topic"), "{reopening}");
    }
}

// --------------------------------------------------------------------------
// workspace/git/worktrees/{limit/update,prune,remove}
// --------------------------------------------------------------------------

#[test]
fn the_limit_update_writes_the_configuration_and_answers_the_new_limit() {
    let (_scratch, root) = case_root();
    let mut connection = connected(&root);

    let answer = answer(
        &mut connection,
        2,
        "workspace/git/worktrees/limit/update",
        json!({"limit": 7}),
    );
    assert_eq!(answer, json!({"limit": 7, "failures": []}));
    let written = fs::read_to_string(root.join("vibe-home/config.toml"))
        .expect("the configuration was written");
    assert!(written.contains("worktree_limit = 7"), "{written}");

    let error = refusal(
        &mut connection,
        3,
        "workspace/git/worktrees/limit/update",
        json!({"limit": 101}),
    );
    assert_eq!(error.code, ProtocolErrorCode::InvalidParams);
}

#[test]
fn the_prune_removes_inactive_worktrees_beyond_the_limit() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let old = abandoned_worktree(&root, &checkout, "old", "old");
    let mut connection = connected(&root);
    answer(
        &mut connection,
        2,
        "workspace/git/worktrees/limit/update",
        json!({"limit": 0}),
    );

    let answer = answer(
        &mut connection,
        3,
        "workspace/git/worktrees/prune",
        json!({}),
    );
    assert_eq!(answer, json!({"removed": 1}));
    assert!(!old.path.exists());
}

#[test]
fn the_remove_answers_every_outcome_as_a_result() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let mut connection = connected(&root);

    let unmanaged = answer(
        &mut connection,
        2,
        "workspace/git/worktrees/remove",
        json!({"cwd": text(&checkout)}),
    );
    assert_eq!(
        unmanaged,
        json!({
            "outcome": "kept_unmanaged",
            "root": null,
            "branch": null,
            "branchDeleted": false,
            "reasons": [],
        }),
    );

    let prepared = abandoned_worktree(&root, &checkout, "review", "topic");
    let removed = answer(
        &mut connection,
        3,
        "workspace/git/worktrees/remove",
        json!({"cwd": text(&prepared.path)}),
    );
    assert_eq!(removed["outcome"], json!("removed"));
    assert_eq!(removed["root"], json!(text(&prepared.root)));
    assert_eq!(removed["branch"], json!("topic"));
    assert_eq!(removed["branchDeleted"], json!(true));
    assert!(!prepared.path.exists());
}
