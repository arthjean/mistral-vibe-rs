//! The worktree listing, driven through the connection a client speaks to.
//!
//! Every case here scripts a real checkout, so the worktrees the listing
//! answers with are git's own rather than a fixture's idea of them.

use super::*;
use std::path::PathBuf;
use std::process::Command;

use crate::workspace::WorkspacePaths;
use crate::worktrees;
use vibe_core::worktree::WorktreeError;

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

fn answer(connection: &mut ServerConnection, id: i64, method: &str, params: Value) -> Value {
    let batch = connection.dispatch(&request(id, method, params));
    match decode_frame(&batch.outbound[0]).expect("an answer") {
        Envelope::Success(SuccessResponse { result, .. }) => {
            Value::Object(result.into_iter().collect())
        }
        other => unreachable!("{method} did not answer: {other:?}"),
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
