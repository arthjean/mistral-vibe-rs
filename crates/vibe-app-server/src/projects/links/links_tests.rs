//! The local half of the `projectLinks/*` surface: what reads and writes the
//! store without reaching Vibe Code, and the shapes it answers.
//!
//! `tests/project_links_parity_tests.rs` replays every method, the Vibe Code
//! ones included, against the reference's own answers; this file pins the
//! decisions behind those answers where a regression would be hard to read
//! from a corpus difference, and validates each local answer against the
//! reference's wire models.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use vibe_protocol::ProtocolErrorCode;

use super::{Failure, api_failure, candidate_page, dispatch};
use crate::app_server_surface_parity_tests::census_issues;
use crate::projects::store::{ProjectLink, ProjectsStore};
use crate::vibe_code::http::{Project, ProjectRepository};
use crate::workspace::WorkspaceService;

/// The remote `github_repository` publishes, in the form a caller sends back.
const REPO_URL: &str = "https://github.com/owner/Repo";

struct Fixture {
    home: TempDir,
    workspace: WorkspaceService,
}

impl Fixture {
    fn new() -> Self {
        let home = tempdir().expect("a vibe home");
        let workspace =
            WorkspaceService::for_runtime_session_root(home.path().join("sessions"), home.path());
        Self { home, workspace }
    }

    fn store(&self) -> ProjectsStore {
        ProjectsStore::in_home(self.home.path())
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Failure> {
        let params: BTreeMap<String, Value> =
            serde_json::from_value(params).expect("the parameters are an object");
        dispatch(&self.workspace, method, &params).await
    }

    /// An answer the reference would also give, checked against its wire model.
    async fn answer(&self, method: &str, params: Value) -> Value {
        let answer = self
            .call(method, params)
            .await
            .unwrap_or_else(|failure| unreachable!("{method} failed: {failure:?}"));
        let issues = census_issues(method, &answer);
        assert!(
            issues.is_empty(),
            "{method} departs from its model: {issues:?}"
        );
        answer
    }
}

fn run_git(directory: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .expect("Git starts");
    assert!(status.success(), "git {args:?} failed");
}

/// A checkout on `work` with one commit, a GitHub remote and a remote HEAD
/// naming `main`, which is what the default branch is read from.
fn github_repository() -> TempDir {
    let repository = tempdir().expect("a checkout");
    let path = repository.path();
    run_git(path, &["init", "--quiet"]);
    run_git(path, &["config", "user.name", "Vibe Test"]);
    run_git(path, &["config", "user.email", "vibe@example.test"]);
    run_git(path, &["config", "commit.gpgsign", "false"]);
    run_git(path, &["checkout", "--quiet", "-b", "work"]);
    fs::write(path.join("tracked.txt"), "base\n").expect("a tracked file");
    run_git(path, &["add", "--", "tracked.txt"]);
    run_git(path, &["commit", "--quiet", "-m", "base"]);
    run_git(
        path,
        &["remote", "add", "origin", "git@github.com:owner/Repo.git"],
    );
    run_git(path, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    run_git(
        path,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    repository
}

fn canonical(path: &Path) -> String {
    fs::canonicalize(path)
        .expect("the directory resolves")
        .to_string_lossy()
        .into_owned()
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

#[tokio::test]
async fn a_root_resolves_to_its_checkout_or_to_a_plain_directory() {
    let fixture = Fixture::new();
    let repository = github_repository();
    let nested = repository.path().join("src");
    fs::create_dir(&nested).expect("a nested directory");

    let resolved = fixture
        .answer(
            "projectLinks/resolveRoot",
            json!({"rootPath": nested.to_string_lossy()}),
        )
        .await;
    let root = &resolved["root"];
    assert_eq!(root["directoryPath"], json!(canonical(repository.path())));
    assert_eq!(root["git"]["currentBranch"], json!("work"));
    assert_eq!(root["git"]["defaultBranch"], json!("main"));
    assert_eq!(root["git"]["hasCommits"], json!(true));
    assert!(root["git"]["githubRepoUrl"].is_string());

    let plain = tempdir().expect("a plain directory");
    let resolved = fixture
        .answer(
            "projectLinks/resolveRoot",
            json!({"rootPath": plain.path().to_string_lossy()}),
        )
        .await;
    assert_eq!(resolved["eligible"], json!(true));
    assert_eq!(resolved["root"]["git"], Value::Null);

    let missing = plain.path().join("gone");
    let resolved = fixture
        .answer(
            "projectLinks/resolveRoot",
            json!({"rootPath": missing.to_string_lossy()}),
        )
        .await;
    assert_eq!(
        resolved,
        json!({"eligible": false, "rejectReason": "nested_unresolvable", "root": null})
    );
}

#[tokio::test]
async fn saving_writes_a_directory_link_or_a_checkout_link_for_the_expected_remote() {
    let fixture = Fixture::new();
    let plain = tempdir().expect("a plain directory");
    let saved = fixture
        .answer(
            "projectLinks/save",
            json!({
                "rootPath": plain.path().to_string_lossy(),
                "projectId": "local-project",
                "projectName": "Local",
                "expectedGithubRepoUrl": null,
            }),
        )
        .await;
    assert_eq!(
        saved["link"]["directoryPath"],
        json!(canonical(plain.path()))
    );

    let repository = github_repository();
    let refused = fixture
        .call(
            "projectLinks/save",
            json!({
                "rootPath": repository.path().to_string_lossy(),
                "projectId": "remote-project",
                "projectName": "Remote",
                "expectedGithubRepoUrl": "https://github.com/owner/Other",
            }),
        )
        .await;
    assert_eq!(
        refused.map_err(|failure| failure.code()),
        Err(ProtocolErrorCode::InvalidParams)
    );
    fixture
        .answer(
            "projectLinks/save",
            json!({
                "rootPath": repository.path().to_string_lossy(),
                "projectId": "remote-project",
                "projectName": "Remote",
                "expectedGithubRepoUrl": format!("{REPO_URL}.git"),
            }),
        )
        .await;

    let links = fixture.store().list_project_links();
    assert!(
        matches!(&links[0], ProjectLink::Local { project_id, .. } if project_id == "local-project")
    );
    assert!(
        matches!(&links[1], ProjectLink::Remote { project_id, .. } if project_id == "remote-project")
    );

    let listed = fixture.answer("projectLinks/list", json!({})).await;
    assert_eq!(
        listed["projects"],
        json!([
            {"projectId": "local-project", "localLinks": [
                {"directoryPath": canonical(plain.path()), "hasCommits": false},
            ]},
            {"projectId": "remote-project", "localLinks": [
                {"directoryPath": canonical(repository.path()), "hasCommits": true},
            ]},
        ])
    );
}

#[tokio::test]
async fn inspection_keeps_a_directory_link_and_clears_a_checkout_link_whose_remote_moved() {
    let fixture = Fixture::new();
    let repository = github_repository();
    let root = fs::canonicalize(repository.path()).expect("the checkout resolves");
    fixture
        .store()
        .upsert_project_link(&ProjectLink::Remote {
            repo_root: root.clone(),
            repo_url: "https://github.com/owner/Moved".to_owned(),
            project_id: "stale".to_owned(),
            project_name: "Stale".to_owned(),
        })
        .expect("the stale link saves");
    let inspected = fixture
        .answer(
            "projectLinks/inspectRoot",
            json!({"rootPath": root.to_string_lossy()}),
        )
        .await;
    assert_eq!(inspected["savedLink"], Value::Null);
    assert_eq!(inspected["staleLinkCleared"], json!(true));
    assert!(fixture.store().list_project_links().is_empty());

    fixture
        .store()
        .upsert_project_link(&ProjectLink::Local {
            directory_path: root.clone(),
            project_id: "kept".to_owned(),
            project_name: "Kept".to_owned(),
        })
        .expect("the directory link saves");
    let inspected = fixture
        .answer(
            "projectLinks/inspectRoot",
            json!({"rootPath": root.to_string_lossy()}),
        )
        .await;
    assert_eq!(
        inspected["savedLink"],
        json!({"projectId": "kept", "projectName": "Kept"})
    );
    assert_eq!(inspected["staleLinkCleared"], json!(false));
}

#[tokio::test]
async fn unlinking_a_root_that_no_longer_resolves_drops_its_closest_ancestor_link() {
    let fixture = Fixture::new();
    let parent = tempdir().expect("a parent directory");
    let outer = fs::canonicalize(parent.path()).expect("the parent resolves");
    let inner = outer.join("checkout");
    fs::create_dir(&inner).expect("the checkout directory");
    for (path, project_id) in [(&outer, "outer"), (&inner, "inner")] {
        fixture
            .store()
            .upsert_project_link(&ProjectLink::Local {
                directory_path: path.clone(),
                project_id: project_id.to_owned(),
                project_name: project_id.to_owned(),
            })
            .expect("the link saves");
    }
    fs::remove_dir(&inner).expect("the checkout is moved away");

    let unlinked = fixture
        .answer(
            "projectLinks/unlink",
            json!({"rootPath": inner.join("src").to_string_lossy()}),
        )
        .await;
    assert_eq!(unlinked, json!({"unlinked": true}));
    let remaining = fixture.store().list_project_links();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].project_id(), "outer");

    fixture
        .answer(
            "projectLinks/unlink",
            json!({"rootPath": outer.to_string_lossy()}),
        )
        .await;
    assert!(fixture.store().list_project_links().is_empty());
}

#[tokio::test]
async fn a_root_parameter_must_be_a_non_empty_string() {
    let fixture = Fixture::new();
    for params in [json!({}), json!({"rootPath": ""}), json!({"rootPath": 3})] {
        let failure = fixture
            .call("projectLinks/resolveRoot", params)
            .await
            .expect_err("the root is refused");
        assert_eq!(failure.code(), ProtocolErrorCode::InvalidParams);
    }
}

#[test]
fn a_vibe_code_failure_naming_the_credential_is_unauthorized_and_any_other_internal() {
    for message in [
        "Missing API key",
        "request failed with status 401",
        "request failed with Status 403",
    ] {
        assert_eq!(
            api_failure(message, "projectLinks/link").code(),
            ProtocolErrorCode::Unauthorized,
            "{message}"
        );
    }
    let failure = api_failure("request failed with status 500: secret detail", "m");
    assert_eq!(failure.code(), ProtocolErrorCode::InternalError);
    assert!(!failure.message().contains("secret"));
}

#[test]
fn candidates_rank_the_saved_project_then_single_repository_matches_by_name() {
    let projects = [
        project(
            "shared",
            "alpha",
            &[REPO_URL, "https://github.com/owner/x"],
            false,
        ),
        project("single-b", "Beta", &[REPO_URL], false),
        project("single-a", "able", &[REPO_URL], false),
        project("read-only", "Aardvark", &[REPO_URL], true),
        project("other", "Other", &["https://github.com/owner/x"], false),
        project(
            "saved",
            "zeta",
            &[REPO_URL, "https://github.com/owner/y"],
            false,
        ),
    ];
    let page = candidate_page(&projects, REPO_URL, Some("saved"), Some("next"), true);
    assert_eq!(
        page,
        json!({
            "items": [
                {"projectId": "saved", "name": "zeta", "recommended": true},
                {"projectId": "single-a", "name": "able", "recommended": false},
                {"projectId": "single-b", "name": "Beta", "recommended": false},
                {"projectId": "shared", "name": "alpha", "recommended": false},
            ],
            "nextCursor": "next",
        })
    );

    // A saved project missing from the page leaves nothing recommended.
    let page = candidate_page(&projects[..3], REPO_URL, Some("saved"), None, true);
    assert_eq!(page["items"][0]["recommended"], json!(false));
}
