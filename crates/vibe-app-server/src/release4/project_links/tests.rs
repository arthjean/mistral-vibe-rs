//! The session-less project-link surface, driven through a real connection.
//!
//! Every case here dispatches a JSON-RPC frame at the server rather than
//! calling a handler, because the surface exists to be reached without a
//! session and that is the only path that proves it: `release4_request` reads
//! no `sessionId`, defers the eight root-resolving methods and answers
//! `projectLinks/list` inline.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use vibe_protocol::{Envelope, TransportKind, decode_frame};

use crate::app_server_surface_parity_tests::census_issues;
use crate::release4::{
    CloudError, CommandGitProbe, Project, ProjectCloud, ProjectPage, ProjectRepository,
    Release4Service, SavedProjectLink, TeleportCloud, TeleportStartFailure, TeleportStartRequest,
};
use crate::server::{AppServer, DeferredWork, ServerConnection};

// --------------------------------------------------------------------------
// Fixtures
// --------------------------------------------------------------------------

/// A project catalog keyed by the cursor that reaches it, so a test declares
/// its pagination rather than a call count.
struct FixtureProjects {
    pages: BTreeMap<Option<String>, ProjectPage>,
    failure: Option<CloudError>,
    created: Mutex<Vec<(String, String, String)>>,
}

impl FixtureProjects {
    fn new(pages: impl IntoIterator<Item = (Option<&'static str>, ProjectPage)>) -> Self {
        Self {
            pages: pages
                .into_iter()
                .map(|(cursor, page)| (cursor.map(ToOwned::to_owned), page))
                .collect(),
            failure: None,
            created: Mutex::new(Vec::new()),
        }
    }

    fn failing(failure: CloudError) -> Self {
        Self {
            pages: BTreeMap::new(),
            failure: Some(failure),
            created: Mutex::new(Vec::new()),
        }
    }
}

impl ProjectCloud for FixtureProjects {
    fn create(
        &self,
        name: &str,
        repo_url: &str,
        default_branch: &str,
    ) -> Result<Project, CloudError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        self.created
            .lock()
            .map_err(|_| CloudError::Unavailable("fixture lock was poisoned".to_owned()))?
            .push((
                name.to_owned(),
                repo_url.to_owned(),
                default_branch.to_owned(),
            ));
        Ok(Project {
            project_id: "created-project".to_owned(),
            name: name.to_owned(),
            repositories: vec![ProjectRepository {
                repo_url: repo_url.to_owned(),
                default_branch: Some(default_branch.to_owned()),
            }],
            is_read_only: false,
        })
    }

    fn list(&self, cursor: Option<&str>) -> Result<ProjectPage, CloudError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        Ok(self
            .pages
            .get(&cursor.map(ToOwned::to_owned))
            .cloned()
            .unwrap_or(ProjectPage {
                projects: Vec::new(),
                next_cursor: None,
            }))
    }
}

struct UnusedTeleport;

impl TeleportCloud for UnusedTeleport {
    fn start(&self, _request: &TeleportStartRequest) -> Result<String, TeleportStartFailure> {
        Err(CloudError::Unavailable("Teleport is not exercised here".to_owned()).into())
    }
}

fn project(project_id: &str, name: &str, repo_urls: &[&str], is_read_only: bool) -> Project {
    Project {
        project_id: project_id.to_owned(),
        name: name.to_owned(),
        repositories: repo_urls
            .iter()
            .map(|repo_url| ProjectRepository {
                repo_url: (*repo_url).to_owned(),
                default_branch: None,
            })
            .collect(),
        is_read_only,
    }
}

fn run_git(working_directory: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(working_directory)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .expect("Git test command starts");
    assert!(status.success(), "Git test command failed: {args:?}");
}

fn init_repository(directory: &Path) {
    run_git(directory, &["init", "--quiet"]);
    run_git(directory, &["config", "user.name", "Vibe Test"]);
    run_git(directory, &["config", "user.email", "vibe@example.test"]);
    run_git(directory, &["branch", "-M", "work"]);
}

/// A checkout with one commit, a GitHub remote and a remote HEAD, which is what
/// the default branch is read from.
fn github_repository() -> TempDir {
    let repository = tempdir().expect("temporary Git repository");
    init_repository(repository.path());
    fs::write(repository.path().join("tracked.txt"), "base\n").expect("tracked fixture");
    run_git(repository.path(), &["add", "--", "tracked.txt"]);
    run_git(repository.path(), &["commit", "--quiet", "-m", "base"]);
    run_git(
        repository.path(),
        &["remote", "add", "origin", "git@github.com:owner/Repo.git"],
    );
    run_git(
        repository.path(),
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    run_git(
        repository.path(),
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    repository
}

/// The remote URL `github_repository` publishes, once sanitized.
const REPO_URL: &str = "https://github.com/owner/Repo.git";

fn service(cloud: Arc<dyn ProjectCloud>, store: &Path) -> Release4Service {
    Release4Service::with_backends(
        cloud,
        Arc::new(UnusedTeleport),
        Arc::new(CommandGitProbe::default()),
    )
    .with_project_link_store(store.to_path_buf())
    .expect("the project-link store loads")
}

fn connected(service: Release4Service) -> (AppServer, ServerConnection) {
    let server = AppServer::default().using_release4_service(service);
    let mut connection = server.connect(TransportKind::InProcess);
    connection.dispatch(&frame(
        1,
        "initialize",
        &json!({
            "clientInfo": {"name": "project-links", "version": "1"},
            "capabilities": {"callbackKinds": [], "clientTools": [], "disabledNotifications": []}
        }),
    ));
    connection.dispatch(
        &serde_json::to_vec(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
            .expect("initialized frame"),
    );
    (server, connection)
}

fn frame(id: i64, method: &str, params: &Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .expect("request frame")
}

/// The answer to one request, running the deferred cloud work when the dispatch
/// returned some, which is how the eight root-resolving methods answer.
fn call(
    server: &AppServer,
    connection: &mut ServerConnection,
    method: &str,
    params: &Value,
) -> Envelope {
    let batch = connection.dispatch(&frame(9, method, params));
    let batch = if batch.outbound.is_empty() {
        let Some(DeferredWork::CloudRequest {
            request_id,
            method,
            params,
        }) = batch.deferred.into_iter().next()
        else {
            unreachable!("{method} answered with neither a frame nor cloud work");
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a deferred-work runtime");
        runtime.block_on(server.execute_cloud_request(request_id, method, params))
    } else {
        batch
    };
    decode_frame(&batch.outbound[0]).expect("a decodable answer")
}

fn result(envelope: Envelope) -> Value {
    match envelope {
        Envelope::Success(success) => Value::Object(success.result.into_iter().collect()),
        other => unreachable!("expected a result: {other:?}"),
    }
}

/// The answer, checked against the reference model the corpus names for the
/// method before it is read.
fn conforming(method: &str, envelope: Envelope) -> Value {
    let value = result(envelope);
    let issues = census_issues(method, &value);
    assert!(
        issues.is_empty(),
        "{method} diverges from the reference census: {issues:?}"
    );
    value
}

fn error_code(envelope: Envelope) -> String {
    match envelope {
        Envelope::Error(error) => serde_json::to_value(error.error.code)
            .ok()
            .and_then(|code| code.as_str().map(ToOwned::to_owned))
            .unwrap_or_default(),
        other => unreachable!("expected an error: {other:?}"),
    }
}

/// Puts a link in memory without writing it, so a test can pair a loaded link
/// with a store that will not accept the write that drops it.
fn seed_link(service: Release4Service, repo_root: &str, repo_url: &str) -> Release4Service {
    service
        .projects
        .lock()
        .expect("the project state locks")
        .linked_projects
        .insert(
            repo_root.to_owned(),
            SavedProjectLink {
                repo_url: repo_url.to_owned(),
                project_id: "project-a".to_owned(),
                project_name: "A".to_owned(),
            },
        );
    service
}

/// Turns the directory holding the store into a file, so the next atomic write
/// cannot create the parent it needs. The store still loaded, so this fails a
/// write without failing construction, and it does not depend on file modes a
/// root-owned CI container would ignore.
fn block_store(directory: &Path) {
    fs::remove_dir_all(directory).expect("the store directory is removed");
    fs::write(directory, "not a directory").expect("the blocking file writes");
}

/// Writes the saved-link store directly, which is what a link saved by an
/// earlier run or by `vibeCode/projects/*` leaves behind.
fn write_store(path: &Path, entries: &[(&str, &str, &str, &str)]) {
    let stored = entries
        .iter()
        .map(|(repo_root, repo_url, project_id, project_name)| {
            (
                (*repo_root).to_owned(),
                json!({
                    "repoUrl": repo_url,
                    "projectId": project_id,
                    "projectName": project_name,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    fs::write(path, Value::Object(stored).to_string()).expect("the store fixture writes");
}

// --------------------------------------------------------------------------
// The read surface
// --------------------------------------------------------------------------

#[test]
fn saved_links_are_listed_grouped_by_project_without_a_session() {
    let home = tempdir().expect("store directory");
    let store = home.path().join("links.json");
    write_store(
        &store,
        &[
            ("/repos/one", REPO_URL, "project-a", "A"),
            ("/repos/two", REPO_URL, "project-a", "A"),
            ("/repos/three", REPO_URL, "project-b", "B"),
        ],
    );
    let (server, mut connection) = connected(service(Arc::new(FixtureProjects::new([])), &store));

    let answer = result(call(
        &server,
        &mut connection,
        "projectLinks/list",
        &json!({}),
    ));

    assert_eq!(
        answer["projects"],
        json!([
            {"projectId": "project-a", "repoLocalPaths": ["/repos/one", "/repos/two"]},
            {"projectId": "project-b", "repoLocalPaths": ["/repos/three"]},
        ]),
        "a saved link is published under the project it points at"
    );
}

#[test]
fn a_repository_root_resolves_with_the_names_a_picker_renders() {
    let repository = github_repository();
    let home = tempdir().expect("store directory");
    let (server, mut connection) = connected(service(
        Arc::new(FixtureProjects::new([])),
        &home.path().join("links.json"),
    ));

    let answer = result(call(
        &server,
        &mut connection,
        "projectLinks/resolveRoot",
        &json!({"rootPath": repository.path().to_string_lossy()}),
    ));

    assert_eq!(answer["eligible"], json!(true));
    assert_eq!(answer["rejectReason"], Value::Null);
    assert_eq!(
        answer["root"]["repoName"], "Repo",
        "the repository name comes from the remote, spelled as its owner spelled it"
    );
    assert_eq!(answer["root"]["currentBranch"], "work");
    assert_eq!(answer["root"]["defaultBranch"], "main");
    assert!(
        !answer["root"]["repoLocalPath"]
            .as_str()
            .unwrap_or_default()
            .is_empty()
    );
    assert!(
        answer["root"].get("repoUrl").is_none(),
        "a resolved root carries no remote URL: {}",
        answer["root"]
    );
}

#[test]
fn an_ineligible_root_reports_a_vocabulary_reason_rather_than_an_internal_error() {
    let plain = tempdir().expect("plain directory");
    let remoteless = tempdir().expect("remoteless repository");
    init_repository(remoteless.path());
    fs::write(remoteless.path().join("a.txt"), "a\n").expect("tracked fixture");
    run_git(remoteless.path(), &["add", "--", "a.txt"]);
    run_git(remoteless.path(), &["commit", "--quiet", "-m", "base"]);
    let uncommitted = tempdir().expect("uncommitted repository");
    init_repository(uncommitted.path());
    run_git(
        uncommitted.path(),
        &["remote", "add", "origin", "git@github.com:owner/repo.git"],
    );
    let home = tempdir().expect("store directory");
    let (server, mut connection) = connected(service(
        Arc::new(FixtureProjects::new([])),
        &home.path().join("links.json"),
    ));

    for (root, expected) in [
        (plain.path(), "not_git"),
        (remoteless.path(), "unsupported_remote"),
        (uncommitted.path(), "no_commits"),
    ] {
        let answer = result(call(
            &server,
            &mut connection,
            "projectLinks/resolveRoot",
            &json!({"rootPath": root.to_string_lossy()}),
        ));
        assert_eq!(answer["eligible"], json!(false), "{}", root.display());
        assert_eq!(
            answer["rejectReason"],
            json!(expected),
            "{}",
            root.display()
        );
        assert_eq!(answer["root"], Value::Null, "{}", root.display());
    }
}

#[test]
fn inspecting_a_root_clears_a_saved_link_that_names_another_repository() {
    let repository = github_repository();
    let root = fs::canonicalize(repository.path()).expect("a canonical root");
    let home = tempdir().expect("store directory");
    let store = home.path().join("links.json");
    write_store(
        &store,
        &[(
            &root.to_string_lossy(),
            "https://github.com/owner/moved.git",
            "project-a",
            "A",
        )],
    );
    let (server, mut connection) = connected(service(Arc::new(FixtureProjects::new([])), &store));

    let answer = result(call(
        &server,
        &mut connection,
        "projectLinks/inspectRoot",
        &json!({"rootPath": repository.path().to_string_lossy()}),
    ));

    assert_eq!(answer["eligible"], json!(true));
    assert_eq!(answer["savedLink"], Value::Null);
    assert_eq!(answer["staleLinkCleared"], json!(true));
    assert_eq!(answer["staleLinkClearFailed"], json!(false));
    assert_eq!(
        answer["root"]["repoUrl"], REPO_URL,
        "an inspected root carries the remote the saved link is compared against"
    );

    // The clear landed, so the next inspection has nothing left to drop.
    let again = result(call(
        &server,
        &mut connection,
        "projectLinks/inspectRoot",
        &json!({"rootPath": repository.path().to_string_lossy()}),
    ));
    assert_eq!(again["staleLinkCleared"], json!(false));
    assert_eq!(again["savedLink"], Value::Null);
}

#[test]
fn a_matching_saved_link_survives_inspection() {
    let repository = github_repository();
    let root = fs::canonicalize(repository.path()).expect("a canonical root");
    let home = tempdir().expect("store directory");
    let store = home.path().join("links.json");
    write_store(
        &store,
        &[(&root.to_string_lossy(), REPO_URL, "project-a", "A")],
    );
    let (server, mut connection) = connected(service(Arc::new(FixtureProjects::new([])), &store));

    let answer = result(call(
        &server,
        &mut connection,
        "projectLinks/inspectRoot",
        &json!({"rootPath": repository.path().to_string_lossy()}),
    ));

    assert_eq!(
        answer["savedLink"],
        json!({"projectId": "project-a", "projectName": "A"})
    );
    assert_eq!(answer["staleLinkCleared"], json!(false));
}

#[test]
fn a_failed_stale_clear_is_reported_rather_than_answered_as_cleared() {
    let repository = github_repository();
    let root = fs::canonicalize(repository.path()).expect("a canonical root");
    let home = tempdir().expect("store directory");
    let state = home.path().join("state");
    fs::create_dir_all(&state).expect("the store directory is created");
    let service = seed_link(
        service(
            Arc::new(FixtureProjects::new([])),
            &state.join("links.json"),
        ),
        &root.to_string_lossy(),
        "https://github.com/owner/moved.git",
    );
    block_store(&state);
    let (server, mut connection) = connected(service);

    let answer = result(call(
        &server,
        &mut connection,
        "projectLinks/inspectRoot",
        &json!({"rootPath": repository.path().to_string_lossy()}),
    ));

    assert_eq!(answer["staleLinkClearFailed"], json!(true));
    assert_eq!(answer["staleLinkCleared"], json!(false));
    assert_eq!(answer["savedLink"], Value::Null);
}

#[test]
fn the_picker_ranks_the_saved_project_first_and_pages_with_its_cursor() {
    let repository = github_repository();
    let root = fs::canonicalize(repository.path()).expect("a canonical root");
    let home = tempdir().expect("store directory");
    let store = home.path().join("links.json");
    write_store(
        &store,
        &[(&root.to_string_lossy(), REPO_URL, "saved", "Saved")],
    );
    let cloud = FixtureProjects::new([
        (
            None,
            ProjectPage {
                projects: vec![
                    project(
                        "alpha",
                        "Alpha",
                        &[REPO_URL, "https://github.com/o/other"],
                        false,
                    ),
                    project("saved", "Saved", &[REPO_URL], false),
                    project("hidden", "Hidden", &["https://github.com/o/other"], false),
                    project("locked", "Locked", &[REPO_URL], true),
                ],
                next_cursor: Some("page-2".to_owned()),
            },
        ),
        (
            Some("page-2"),
            ProjectPage {
                projects: vec![project("beta", "Beta", &[REPO_URL], false)],
                next_cursor: None,
            },
        ),
    ]);
    let (server, mut connection) = connected(service(Arc::new(cloud), &store));

    let loaded = result(call(
        &server,
        &mut connection,
        "projectLinks/picker/load",
        &json!({"rootPath": repository.path().to_string_lossy()}),
    ));

    assert_eq!(
        loaded["candidates"]["items"],
        json!([
            {"projectId": "saved", "name": "Saved", "matchKind": "exact_repo", "recommended": true},
            {"projectId": "alpha", "name": "Alpha", "matchKind": "multi_repo", "recommended": false},
        ]),
        "a read-only project and one linked to another repository are not candidates"
    );
    assert_eq!(loaded["candidates"]["nextCursor"], "page-2");
    assert_eq!(
        loaded["savedLink"],
        json!({"projectId": "saved", "projectName": "Saved"})
    );
    assert_eq!(loaded["staleLinkCleared"], json!(false));
    assert_eq!(loaded["root"]["repoName"], "Repo");

    let more = result(call(
        &server,
        &mut connection,
        "projectLinks/picker/loadMore",
        &json!({"rootPath": repository.path().to_string_lossy(), "cursor": "page-2"}),
    ));

    assert_eq!(
        more["candidates"]["items"],
        json!([
            {"projectId": "beta", "name": "Beta", "matchKind": "exact_repo", "recommended": false},
        ]),
        "a page past the first recommends nothing"
    );
    assert_eq!(more["candidates"]["nextCursor"], Value::Null);
    assert_eq!(more["focusProjectId"], "beta");
}

#[test]
fn an_absent_credential_is_unauthorized_and_a_reachable_failure_is_internal() {
    let repository = github_repository();
    let home = tempdir().expect("store directory");
    let root_path = json!({"rootPath": repository.path().to_string_lossy()});

    // No Vibe Code backend attached, but a usable Git probe, so the root
    // resolves and the credential is what is missing. The reference classifies
    // that as an authorization failure.
    let unconfigured = Release4Service {
        git: Arc::new(CommandGitProbe::default()),
        ..Release4Service::default()
    }
    .with_project_link_store(home.path().join("unconfigured.json"))
    .expect("the project-link store loads");
    let (server, mut connection) = connected(unconfigured);
    assert_eq!(
        error_code(call(
            &server,
            &mut connection,
            "projectLinks/picker/load",
            &root_path,
        )),
        "unauthorized"
    );

    // A backend that is attached and failing is not the user's credential.
    let (server, mut connection) = connected(service(
        Arc::new(FixtureProjects::failing(CloudError::Unavailable(
            "Vibe Code answered HTTP 503".to_owned(),
        ))),
        &home.path().join("failing.json"),
    ));
    assert_eq!(
        error_code(call(
            &server,
            &mut connection,
            "projectLinks/picker/load",
            &root_path,
        )),
        "internal_error"
    );

    // A rejected credential stays an authorization failure.
    let (server, mut connection) = connected(service(
        Arc::new(FixtureProjects::failing(CloudError::Unauthorized(
            "MISTRAL_API_KEY was rejected".to_owned(),
        ))),
        &home.path().join("rejected.json"),
    ));
    assert_eq!(
        error_code(call(
            &server,
            &mut connection,
            "projectLinks/picker/load",
            &root_path,
        )),
        "unauthorized"
    );
}

// --------------------------------------------------------------------------
// The mutation surface
// --------------------------------------------------------------------------

#[test]
fn creating_a_project_links_the_root_to_it() {
    let repository = github_repository();
    let root = fs::canonicalize(repository.path()).expect("a canonical root");
    let home = tempdir().expect("store directory");
    let store = home.path().join("links.json");
    let (server, mut connection) = connected(service(Arc::new(FixtureProjects::new([])), &store));

    let created = result(call(
        &server,
        &mut connection,
        "projectLinks/create",
        &json!({
            "rootPath": repository.path().to_string_lossy(),
            "name": "New Project",
            "defaultBranch": "main",
        }),
    ));

    assert_eq!(
        created["link"],
        json!({
            "projectId": "created-project",
            "projectName": "New Project",
            "repoLocalPath": root.to_string_lossy(),
        })
    );
    let listed = result(call(
        &server,
        &mut connection,
        "projectLinks/list",
        &json!({}),
    ));
    assert_eq!(listed["projects"][0]["projectId"], "created-project");
    assert!(
        store.is_file(),
        "the link is persisted where the next run reads it"
    );
}

#[test]
fn linking_validates_the_target_and_persists_the_server_side_name() {
    let repository = github_repository();
    let home = tempdir().expect("store directory");
    let cloud = FixtureProjects::new([(
        None,
        ProjectPage {
            projects: vec![
                project("linkable", "Server Name", &[REPO_URL], false),
                project("locked", "Locked", &[REPO_URL], true),
                project(
                    "elsewhere",
                    "Elsewhere",
                    &["https://github.com/o/other"],
                    false,
                ),
            ],
            next_cursor: None,
        },
    )]);
    let (server, mut connection) =
        connected(service(Arc::new(cloud), &home.path().join("links.json")));
    let root_path = repository.path().to_string_lossy().into_owned();

    let linked = result(call(
        &server,
        &mut connection,
        "projectLinks/link",
        &json!({"rootPath": root_path, "projectId": "linkable", "projectName": "Client Guess"}),
    ));
    assert_eq!(linked["link"]["projectId"], "linkable");
    assert_eq!(
        linked["link"]["projectName"], "Server Name",
        "the validated project's own name is persisted, not the caller's"
    );

    for (project_id, why) in [
        ("missing", "an unknown project"),
        ("locked", "a read-only project"),
        ("elsewhere", "a project linked to another repository"),
    ] {
        assert_eq!(
            error_code(call(
                &server,
                &mut connection,
                "projectLinks/link",
                &json!({
                    "rootPath": root_path,
                    "projectId": project_id,
                    "projectName": "Client Guess",
                }),
            )),
            "invalid_params",
            "{why} is refused"
        );
    }
}

#[test]
fn saving_requires_the_remote_the_caller_last_saw() {
    let repository = github_repository();
    let home = tempdir().expect("store directory");
    let (server, mut connection) = connected(service(
        Arc::new(FixtureProjects::new([])),
        &home.path().join("links.json"),
    ));
    let root_path = repository.path().to_string_lossy().into_owned();

    assert_eq!(
        error_code(call(
            &server,
            &mut connection,
            "projectLinks/save",
            &json!({
                "rootPath": root_path,
                "projectId": "project-a",
                "projectName": "A",
                "expectedRepoUrl": "https://github.com/owner/moved.git",
            }),
        )),
        "invalid_params",
        "a remote that moved since the caller read it refuses the save"
    );

    let saved = result(call(
        &server,
        &mut connection,
        "projectLinks/save",
        &json!({
            "rootPath": root_path,
            "projectId": "project-a",
            "projectName": "A",
            "expectedRepoUrl": "git@github.com:owner/Repo.git",
        }),
    ));
    assert_eq!(
        saved["link"]["projectId"], "project-a",
        "the remote is compared normalized, so an SSH spelling matches its HTTPS one"
    );
}

#[test]
fn unlinking_removes_the_link_and_stays_successful_when_there_is_none() {
    let repository = github_repository();
    let root = fs::canonicalize(repository.path()).expect("a canonical root");
    let home = tempdir().expect("store directory");
    let store = home.path().join("links.json");
    write_store(
        &store,
        &[(&root.to_string_lossy(), REPO_URL, "project-a", "A")],
    );
    let (server, mut connection) = connected(service(Arc::new(FixtureProjects::new([])), &store));
    let root_path = json!({"rootPath": repository.path().to_string_lossy()});

    let unlinked = result(call(
        &server,
        &mut connection,
        "projectLinks/unlink",
        &root_path,
    ));
    assert_eq!(unlinked, json!({"unlinked": true}));
    let listed = result(call(
        &server,
        &mut connection,
        "projectLinks/list",
        &json!({}),
    ));
    assert_eq!(listed["projects"], json!([]));

    let again = result(call(
        &server,
        &mut connection,
        "projectLinks/unlink",
        &root_path,
    ));
    assert_eq!(
        again,
        json!({"unlinked": true}),
        "unlinking a root that carries no link is not a failure"
    );
}

#[test]
fn unlinking_a_moved_checkout_still_removes_the_stranded_link() {
    let home = tempdir().expect("store directory");
    let store = home.path().join("links.json");
    let stranded = home.path().join("checkout");
    let nested = stranded.join("packages").join("app");
    fs::create_dir_all(&nested).expect("the nested directory is created");
    write_store(
        &store,
        &[
            (&stranded.to_string_lossy(), REPO_URL, "project-a", "A"),
            (
                &stranded.join("packages").to_string_lossy(),
                REPO_URL,
                "project-b",
                "B",
            ),
        ],
    );
    let (server, mut connection) = connected(service(Arc::new(FixtureProjects::new([])), &store));

    // The path is not a repository, so Git resolves nothing and the closest
    // stored ancestor is the link to drop.
    let answer = result(call(
        &server,
        &mut connection,
        "projectLinks/unlink",
        &json!({"rootPath": nested.to_string_lossy()}),
    ));

    assert_eq!(answer, json!({"unlinked": true}));
    let listed = result(call(
        &server,
        &mut connection,
        "projectLinks/list",
        &json!({}),
    ));
    assert_eq!(
        listed["projects"],
        json!([{"projectId": "project-a", "repoLocalPaths": [stranded.to_string_lossy()]}]),
        "the deepest ancestor wins when links are nested"
    );
}

// --------------------------------------------------------------------------
// Conformance
// --------------------------------------------------------------------------

/// Every answer of this surface, validated against the committed census.
///
/// The surface replay probes a bare server, where no path resolves to a
/// repository and no backend answers, so it only ever sees the ineligible form
/// of three methods. The seven models the eligible and mutation answers carry
/// (`ProjectLinksResolvedRoot`, `ProjectLinksInspectedRoot`,
/// `ProjectLinksSavedLink`, `ProjectLinksPickerCandidate(s)`, `ProjectLink` and
/// `ProjectLinkMutationResponse`) are reached from here instead, so a field
/// renamed on either side fails naming its JSON pointer rather than passing.
#[test]
fn every_answer_validates_against_the_reference_census() {
    let repository = github_repository();
    let root_path = repository.path().to_string_lossy().into_owned();
    let root = fs::canonicalize(repository.path()).expect("a canonical root");
    let home = tempdir().expect("store directory");
    let store = home.path().join("links.json");
    write_store(
        &store,
        &[(&root.to_string_lossy(), REPO_URL, "saved", "Saved")],
    );
    let cloud = FixtureProjects::new([
        (
            None,
            ProjectPage {
                projects: vec![
                    project("saved", "Saved", &[REPO_URL], false),
                    project(
                        "alpha",
                        "Alpha",
                        &[REPO_URL, "https://github.com/o/x"],
                        false,
                    ),
                ],
                next_cursor: Some("page-2".to_owned()),
            },
        ),
        (
            Some("page-2"),
            ProjectPage {
                projects: vec![project("beta", "Beta", &[REPO_URL], false)],
                next_cursor: None,
            },
        ),
    ]);
    let (server, mut connection) = connected(service(Arc::new(cloud), &store));

    // Ordered so each answer carries something: the store holds a link before it
    // is listed and inspected, and the root is unlinked only at the end.
    for (method, params) in [
        ("projectLinks/list", json!({})),
        ("projectLinks/resolveRoot", json!({"rootPath": root_path})),
        ("projectLinks/inspectRoot", json!({"rootPath": root_path})),
        ("projectLinks/picker/load", json!({"rootPath": root_path})),
        (
            "projectLinks/picker/loadMore",
            json!({"rootPath": root_path, "cursor": "page-2"}),
        ),
        (
            "projectLinks/link",
            json!({"rootPath": root_path, "projectId": "beta", "projectName": "Beta"}),
        ),
        (
            "projectLinks/save",
            json!({
                "rootPath": root_path,
                "projectId": "saved",
                "projectName": "Saved",
                "expectedRepoUrl": REPO_URL,
            }),
        ),
        (
            "projectLinks/create",
            json!({"rootPath": root_path, "name": "New", "defaultBranch": "main"}),
        ),
        ("projectLinks/unlink", json!({"rootPath": root_path})),
    ] {
        let answer = conforming(method, call(&server, &mut connection, method, &params));
        assert!(
            !answer.as_object().is_some_and(serde_json::Map::is_empty),
            "{method} answered with nothing to validate"
        );
    }
}

#[test]
fn a_failed_removal_is_reported_rather_than_answered_as_unlinked() {
    let home = tempdir().expect("store directory");
    let state = home.path().join("state");
    fs::create_dir_all(&state).expect("the store directory is created");
    let stranded = home.path().join("checkout");
    fs::create_dir_all(&stranded).expect("the checkout directory is created");
    let stranded = fs::canonicalize(&stranded).expect("a canonical checkout");
    let service = seed_link(
        service(
            Arc::new(FixtureProjects::new([])),
            &state.join("links.json"),
        ),
        &stranded.to_string_lossy(),
        REPO_URL,
    );
    block_store(&state);
    let (server, mut connection) = connected(service);

    assert_eq!(
        error_code(call(
            &server,
            &mut connection,
            "projectLinks/unlink",
            &json!({"rootPath": stranded.to_string_lossy()}),
        )),
        "internal_error",
        "a removal that did not land is not answered as though it had"
    );
}
