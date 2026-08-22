//! The local-workspace half of the session contract, driven through the
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
use vibe_core::worktree::{PreparedWorktree, WorktreeError};

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

// --------------------------------------------------------------------------
// US-281: workspace/worktrees/list
// --------------------------------------------------------------------------

#[test]
fn the_listing_answers_one_entry_per_linked_worktree() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let linked = linked_worktree(&checkout, &root, "review", "topic");
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    let answer = answer(
        &mut connection,
        2,
        "workspace/worktrees/list",
        json!({"cwd": text(&checkout)}),
    );
    assert_eq!(
        answer["worktrees"],
        json!([{
            "name": "review",
            "branch": "topic",
            "cwd": text(&linked),
            "root": text(&linked),
            "repoRoot": text(&checkout),
        }]),
    );
}

/// The method is a host method: it takes no `sessionId` and answers before any
/// session has been opened, which is what makes a worktree picker reachable at
/// the moment a client is deciding where to open one.
#[test]
fn the_listing_answers_before_any_session_exists() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    let answer = answer(
        &mut connection,
        2,
        "workspace/worktrees/list",
        json!({"cwd": text(&checkout)}),
    );
    assert_eq!(answer["worktrees"], json!([]));
}

#[test]
fn a_path_outside_a_repository_lists_nothing_rather_than_refusing() {
    let (_scratch, root) = case_root();
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    let answer = answer(
        &mut connection,
        2,
        "workspace/worktrees/list",
        json!({"cwd": text(&root)}),
    );
    assert_eq!(answer["worktrees"], json!([]));
}

/// A host without git is the second reason the listing answers empty, and it
/// cannot be scripted from a test that must not touch the process environment:
/// the spawn failure is asserted on the arm that swallows it instead.
#[test]
fn a_host_without_git_lists_nothing_rather_than_refusing() {
    let unavailable = Err(WorktreeError::GitUnavailable(
        "git is not on PATH".to_owned(),
    ));
    assert_eq!(
        worktrees::swallow_missing_checkout(unavailable).expect("git being absent is not an error"),
        Vec::new(),
    );
    let refused = Err(WorktreeError::ListFailed {
        message: "git refused".to_owned(),
    });
    assert!(worktrees::swallow_missing_checkout(refused).is_err());
}

#[test]
fn the_listing_is_advertised_in_the_handshake() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    let response = initialize_with(&mut connection, json!({}));
    let methods = response["capabilities"]["methods"]
        .as_array()
        .expect("the handshake advertises its methods");
    assert!(methods.contains(&json!("workspace/worktrees/list")));
}

// --------------------------------------------------------------------------
// US-282: localWorkspaceSelection on session/start
// --------------------------------------------------------------------------

#[test]
fn an_existing_selection_opens_the_session_in_the_linked_worktree() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let linked = linked_worktree(&checkout, &root, "review", "topic");
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    let answer = answer(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "localWorkspaceSelection": {"kind": "existing", "cwd": text(&linked)},
        }),
    );
    assert_eq!(answer["state"]["session"]["cwd"], json!(text(&linked)));
    assert_eq!(
        answer["state"]["session"]["workspaceRoots"],
        json!([text(&linked)]),
    );
}

#[test]
fn an_existing_selection_outside_the_checkout_is_refused_by_its_path() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let stranger = root.join("stranger");
    fs::create_dir_all(&stranger).expect("the stranger is writable");
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    let error = refusal(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "localWorkspaceSelection": {"kind": "existing", "cwd": text(&stranger)},
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
fn a_create_selection_mints_the_named_worktree_on_the_named_branch() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let home = root.join("vibe-home");
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    let answer = answer(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "localWorkspaceSelection": {"kind": "create", "name": "review", "branch": "topic"},
        }),
    );
    let minted = PathBuf::from(
        answer["state"]["session"]["cwd"]
            .as_str()
            .expect("the session names its directory"),
    );
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

#[test]
fn a_create_selection_git_refuses_creates_nothing() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    let error = refusal(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "localWorkspaceSelection": {"kind": "create", "name": "review", "branch": "..bad"},
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
            "localWorkspaceSelection": {"kind": "create", "name": "nested/name", "branch": "topic"},
        }),
    );
    assert_eq!(unportable.code, ProtocolErrorCode::InvalidParams);
    assert!(!branch_is_present(&checkout, "topic"));
}

#[test]
fn a_base_that_is_not_a_directory_is_refused_by_its_path() {
    let (_scratch, root) = case_root();
    let absent = root.join("absent");
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    let error = refusal(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&absent),
            "localWorkspaceSelection": {"kind": "create", "name": "review", "branch": "topic"},
        }),
    );
    assert_eq!(error.code, ProtocolErrorCode::InvalidParams);
    assert!(
        error.message.contains(text(&absent)),
        "the refusal names the base: {}",
        error.message
    );
}

/// A session start with no selection resolves nothing, which is what keeps
/// every other case in this suite reading the directory it asked for.
#[test]
fn a_start_without_a_selection_keeps_the_directory_it_was_given() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    let answer = answer(
        &mut connection,
        2,
        "session/start",
        json!({"sessionId": "session-1", "cwd": text(&checkout)}),
    );
    assert_eq!(answer["state"]["session"]["cwd"], json!(text(&checkout)));
}

#[test]
fn a_resolved_selection_is_cleared_from_the_options() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let linked = linked_worktree(&checkout, &root, "review", "topic");
    let server = AppServer::with_workspace_service(service(&root));
    let connection = server.connect(TransportKind::InProcess);

    let mut params: SessionStartParams = serde_json::from_value(json!({
        "sessionId": "session-1",
        "cwd": text(&checkout),
        "localWorkspaceSelection": {"kind": "existing", "cwd": text(&linked)},
    }))
    .expect("the parameters deserialize");
    let Ok(prepared) = connection.resolve_local_workspace(&mut params) else {
        unreachable!("the selection resolves")
    };

    assert!(prepared.is_none(), "an existing worktree was not created");
    assert!(params.local_workspace_selection.is_none());
    assert_eq!(params.working_directory.as_deref(), Some(text(&linked)));
    assert_eq!(params.add_directories, vec![text(&linked).to_owned()]);
}

/// An absent `cwd` means the app-server's own directory, which is the runtime
/// default a non-desktop client relies on. Both spellings of it resolve to the
/// same base, which is what the resolution asserts rather than naming a
/// directory the test process does not control.
#[test]
fn an_absent_cwd_resolves_the_selection_against_the_process_directory() {
    let (_scratch, root) = case_root();
    let home = root.join("vibe-home");
    let selection = serde_json::from_value(json!({
        "kind": "existing",
        "cwd": "worktree-no-checkout-links",
    }))
    .expect("the selection deserializes");

    let absent =
        worktrees::resolve(&selection, None, &home).expect_err("no checkout links that directory");
    let dot = worktrees::resolve(&selection, Some("."), &home)
        .expect_err("no checkout links that directory");
    assert_eq!(absent.to_string(), dot.to_string());
}

#[test]
fn a_selection_is_refused_on_resume_and_on_continue() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    for (id, method) in [(2, "session/resume"), (3, "session/continue")] {
        let error = refusal(
            &mut connection,
            id,
            method,
            json!({
                "sessionId": "saved",
                "cwd": text(&checkout),
                "localWorkspaceSelection": {"kind": "create", "name": "review", "branch": "topic"},
            }),
        );
        assert_eq!(error.code, ProtocolErrorCode::InvalidParams, "{method}");
        assert!(
            error.message.contains("localWorkspaceSelection"),
            "{method} names the field it refused: {}",
            error.message
        );
    }
}

// --------------------------------------------------------------------------
// US-283: what a failed start takes back
// --------------------------------------------------------------------------

/// A start that fails after the worktree exists leaves neither the worktree nor
/// the branch it minted. `historyLimit` is the refusal used because it is
/// resolved after the selection and before anything else, so the failure lands
/// exactly in the span the cleanup covers.
#[test]
fn a_failed_start_takes_back_the_worktree_and_the_branch_it_created() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    let error = refusal(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "historyLimit": 0,
            "localWorkspaceSelection": {"kind": "create", "name": "review", "branch": "topic"},
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
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    refusal(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "historyLimit": 0,
            "localWorkspaceSelection": {"kind": "create", "name": "review", "branch": "topic"},
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
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    refusal(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "historyLimit": 0,
            "localWorkspaceSelection": {"kind": "existing", "cwd": text(&linked)},
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
    let orphan = PreparedWorktree {
        name: "review".to_owned(),
        branch: "topic".to_owned(),
        root: root.join("review"),
        path: root.join("review"),
        repo_root: root.join("no-checkout-here"),
        base_commit: "0".repeat(40),
        created: true,
        branch_created: true,
    };
    let note = worktrees::discard(&orphan).expect("the removal failed");
    assert!(
        note.contains("review"),
        "the note names the worktree: {note}"
    );

    let selected = PreparedWorktree {
        created: false,
        ..orphan
    };
    assert!(
        worktrees::discard(&selected).is_none(),
        "a worktree this start did not create is never removed"
    );
}

/// Closing a session removes nothing: worktree cleanup on exit is the terminal
/// client's contract, and an app-server that took one back here would discard
/// work its client never asked it to.
#[test]
fn closing_a_session_leaves_the_worktree_it_ran_in() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    let started = answer(
        &mut connection,
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "cwd": text(&checkout),
            "localWorkspaceSelection": {"kind": "create", "name": "review", "branch": "topic"},
        }),
    );
    let minted = PathBuf::from(
        started["state"]["session"]["cwd"]
            .as_str()
            .expect("the session names its directory"),
    );
    assert!(minted.is_dir());

    answer(
        &mut connection,
        3,
        "session/close",
        json!({"sessionId": "session-1"}),
    );
    assert!(minted.is_dir(), "the worktree outlives the session");
    assert!(branch_is_present(&checkout, "topic"));
}

/// `session/start` also carries the two reopening intents, and each one hands
/// the session the directory its recorded session was written against. A
/// selection resolved on the way to one would mint a worktree the session never
/// opens and no failure path takes back, so the refusal covers the flags as
/// well as the two methods above.
#[test]
fn a_selection_is_refused_on_a_start_that_reopens_a_recorded_session() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root);
    let elsewhere = root.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("the recorded directory is writable");
    vibe_core::storage::SessionStore::new(root.join("vibe-home/sessions"))
        .create("saved", &elsewhere.to_string_lossy(), None, 10)
        .expect("the recorded session is written");
    let server = AppServer::with_workspace_service(service(&root));
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    for (id, reopening) in [
        (2, json!({"resume": "saved"})),
        (3, json!({"continue": true})),
    ] {
        let mut params = json!({
            "sessionId": "saved",
            "cwd": text(&checkout),
            "localWorkspaceSelection": {"kind": "create", "name": "review", "branch": "topic"},
        });
        for (key, value) in reopening.as_object().expect("the reopening flag") {
            params[key] = value.clone();
        }
        let error = refusal(&mut connection, id, "session/start", params);
        assert_eq!(error.code, ProtocolErrorCode::InvalidParams, "{reopening}");
        assert!(
            error.message.contains("localWorkspaceSelection"),
            "{reopening} names the field it refused: {}",
            error.message
        );
        assert_eq!(worktree_count(&checkout), 1, "{reopening} minted nothing");
        assert!(!branch_is_present(&checkout, "topic"), "{reopening}");
    }
}
