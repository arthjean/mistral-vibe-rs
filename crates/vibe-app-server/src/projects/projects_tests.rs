use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use tempfile::tempdir;

use super::git::encode_working_tree_diff;
use super::git::git_tests::{committed_github_repository, run_test_git};
use super::selection::project_view;
use super::teleport::TeleportState;
use super::*;

struct FixtureProjects;

impl ProjectCloud for FixtureProjects {
    fn create(
        &self,
        name: &str,
        repo_url: &str,
        default_branch: &str,
    ) -> Result<Project, CloudError> {
        Ok(Project {
            project_id: format!("project-{name}"),
            name: name.to_owned(),
            repositories: vec![ProjectRepository {
                repo_url: repo_url.to_owned(),
                default_branch: Some(default_branch.to_owned()),
            }],
            is_read_only: false,
        })
    }

    fn list(&self, cursor: Option<&str>) -> Result<ProjectPage, CloudError> {
        Ok(ProjectPage {
            projects: vec![
                Project {
                    project_id: format!("page-{}", cursor.unwrap_or("first")),
                    name: "page".to_owned(),
                    repositories: vec![
                        ProjectRepository {
                            repo_url: "fixture".to_owned(),
                            default_branch: Some("main".to_owned()),
                        },
                        ProjectRepository {
                            repo_url: "https://github.com/owner/repo.git".to_owned(),
                            default_branch: Some("main".to_owned()),
                        },
                    ],
                    is_read_only: false,
                },
                Project {
                    project_id: format!("read-only-{}", cursor.unwrap_or("first")),
                    name: "read only".to_owned(),
                    repositories: Vec::new(),
                    is_read_only: true,
                },
            ],
            next_cursor: cursor.is_none().then(|| "next".to_owned()),
        })
    }
}

struct HeadlessProjects {
    pages: BTreeMap<Option<String>, ProjectPage>,
    list_calls: Mutex<Vec<Option<String>>>,
    create_calls: Mutex<Vec<(String, String, String)>>,
}

impl HeadlessProjects {
    fn new(pages: impl IntoIterator<Item = (Option<&'static str>, ProjectPage)>) -> Self {
        Self {
            pages: pages
                .into_iter()
                .map(|(cursor, page)| (cursor.map(ToOwned::to_owned), page))
                .collect(),
            list_calls: Mutex::new(Vec::new()),
            create_calls: Mutex::new(Vec::new()),
        }
    }
}

impl ProjectCloud for HeadlessProjects {
    fn create(
        &self,
        name: &str,
        repo_url: &str,
        default_branch: &str,
    ) -> Result<Project, CloudError> {
        self.create_calls
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
        let cursor = cursor.map(ToOwned::to_owned);
        self.list_calls
            .lock()
            .map_err(|_| CloudError::Unavailable("fixture lock was poisoned".to_owned()))?
            .push(cursor.clone());
        self.pages.get(&cursor).cloned().ok_or_else(|| {
            CloudError::Unavailable(format!(
                "fixture omitted project page for cursor {cursor:?}"
            ))
        })
    }
}

#[derive(Clone)]
struct HeadlessGit {
    snapshot: ProjectGitSnapshot,
}

impl GitProbe for HeadlessGit {
    fn inspect(&self, _working_directory: &Path) -> Result<GitSnapshot, CloudError> {
        Ok(self.snapshot.snapshot.clone())
    }

    fn inspect_project(&self, _working_directory: &Path) -> Result<ProjectGitSnapshot, CloudError> {
        Ok(self.snapshot.clone())
    }

    fn push(&self, _working_directory: &Path) -> Result<(), CloudError> {
        Ok(())
    }
}

struct FixtureTeleport {
    fail: AtomicBool,
}

impl TeleportCloud for FixtureTeleport {
    fn start(&self, request: &TeleportStartRequest) -> Result<String, TeleportStartFailure> {
        if self.fail.load(AtomicOrdering::Relaxed) {
            // The service refuses the project rather than the credential,
            // which is the failure a saved link is cleared for.
            Err(TeleportStartFailure {
                error: CloudError::Unauthorized("sign in again".to_owned()),
                http_status_code: Some(403),
            })
        } else {
            Ok(format!(
                "https://cloud.example/teleport/{}",
                request.idempotency_key
            ))
        }
    }
}

struct FixtureGit {
    snapshot: GitSnapshot,
    pushed: AtomicBool,
}

impl GitProbe for FixtureGit {
    fn inspect(&self, _working_directory: &Path) -> Result<GitSnapshot, CloudError> {
        Ok(self.snapshot.clone())
    }

    fn push(&self, _working_directory: &Path) -> Result<(), CloudError> {
        self.pushed.store(true, AtomicOrdering::Relaxed);
        Ok(())
    }
}

struct DirtyOnlyGit {
    pushed: AtomicBool,
}

impl GitProbe for DirtyOnlyGit {
    fn inspect(&self, _working_directory: &Path) -> Result<GitSnapshot, CloudError> {
        Ok(GitSnapshot {
            repository: "https://github.com/owner/repo.git".to_owned(),
            dirty: true,
            unpushed: false,
        })
    }

    fn inspect_for_teleport(
        &self,
        _working_directory: &Path,
    ) -> Result<(GitSnapshot, TeleportRepository, GitPushStatus), CloudError> {
        let diff = encode_working_tree_diff(b"diff --git a/file b/file\n")?;
        Ok((
            self.inspect(Path::new("."))?,
            TeleportRepository {
                repo_url: "https://github.com/owner/repo.git".to_owned(),
                branch: Some("main".to_owned()),
                commit_sha: Some("0123456789abcdef".to_owned()),
                diff: Some(diff),
            },
            GitPushStatus {
                unpushed_count: 0,
                branch_not_pushed: false,
            },
        ))
    }

    fn push(&self, _working_directory: &Path) -> Result<(), CloudError> {
        self.pushed.store(true, AtomicOrdering::Relaxed);
        Ok(())
    }
}

#[derive(Default)]
struct CapturingTeleport {
    requests: Mutex<Vec<TeleportStartRequest>>,
}

impl TeleportCloud for CapturingTeleport {
    fn start(&self, request: &TeleportStartRequest) -> Result<String, TeleportStartFailure> {
        self.requests
            .lock()
            .map_err(|_| {
                TeleportStartFailure::from(CloudError::Unavailable(
                    "capture lock failed".to_owned(),
                ))
            })?
            .push(request.clone());
        Ok("https://cloud.example/teleport/dirty-only".to_owned())
    }
}

#[tokio::test]
async fn dirty_only_teleport_starts_with_a_diff_and_never_pushes() {
    let git = Arc::new(DirtyOnlyGit {
        pushed: AtomicBool::new(false),
    });
    let teleport = Arc::new(CapturingTeleport::default());
    let service =
        ProjectsService::with_backends(Arc::new(FixtureProjects), teleport.clone(), git.clone());
    let picker_id = open_picker(&service, "session-dirty").await;
    select_project(&service, "session-dirty", &picker_id, "page-first");

    let started = service
        .dispatch_deferred(
            "vibeCode/teleport/start",
            &params(json!({
                "sessionId": "session-dirty",
                "pickerId": picker_id,
                "operationId": "operation-dirty",
                "projectId": "page-first",
                "workingDirectory": "/workspace",
                "prompt": "continue",
            })),
        )
        .await
        .expect("dirty-only Teleport starts");

    assert!(
        started
            .notifications
            .iter()
            .all(|notification| { notification.params["event"]["kind"] != "push_required" })
    );
    assert_eq!(
        started
            .notifications
            .last()
            .and_then(|notification| notification.params["event"]["kind"].as_str()),
        Some("complete")
    );
    assert!(!git.pushed.load(AtomicOrdering::Relaxed));
    let requests = teleport.requests.lock().expect("captured requests");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].repository.diff.is_some());
}

fn params(value: Value) -> BTreeMap<String, Value> {
    value
        .as_object()
        .expect("object")
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn fixture_service(git: Arc<FixtureGit>, teleport: Arc<FixtureTeleport>) -> ProjectsService {
    ProjectsService::with_backends(Arc::new(FixtureProjects), teleport, git)
}

fn headless_service(cloud: Arc<HeadlessProjects>) -> ProjectsService {
    ProjectsService::with_backends(
        cloud,
        Arc::new(FixtureTeleport {
            fail: AtomicBool::new(false),
        }),
        Arc::new(HeadlessGit {
            snapshot: ProjectGitSnapshot {
                snapshot: GitSnapshot {
                    repository: "https://github.com/mistralai/mistral-vibe.git".to_owned(),
                    dirty: false,
                    unpushed: false,
                },
                repo_root: "/repo/mistral-vibe".to_owned(),
                remote_name: "origin".to_owned(),
                branch: Some("feature/headless".to_owned()),
            },
        }),
    )
}

fn listed_project(project_id: &str, repo_urls: &[&str], is_read_only: bool) -> Project {
    Project {
        project_id: project_id.to_owned(),
        name: project_id.to_owned(),
        repositories: repo_urls
            .iter()
            .map(|repo_url| ProjectRepository {
                repo_url: (*repo_url).to_owned(),
                default_branch: Some("main".to_owned()),
            })
            .collect(),
        is_read_only,
    }
}

async fn open_headless_project(service: &ProjectsService, session_id: &str) -> ProjectsDispatch {
    service
        .dispatch_deferred(
            "vibeCode/projects/open",
            &params(json!({
                "sessionId": session_id,
                "workingDirectory": "/repo/mistral-vibe",
                "purpose": "teleport",
            })),
        )
        .await
        .expect("headless project opens")
}

async fn open_picker(service: &ProjectsService, session_id: &str) -> String {
    service
        .dispatch_deferred(
            "vibeCode/projects/open",
            &params(json!({"sessionId": session_id})),
        )
        .await
        .expect("picker opens")
        .result["pickerId"]
        .as_str()
        .expect("picker ID")
        .to_owned()
}

fn select_project(service: &ProjectsService, session_id: &str, picker_id: &str, project_id: &str) {
    service
        .dispatch(
            "vibeCode/projects/select",
            &params(json!({
                "sessionId": session_id,
                "pickerId": picker_id,
                "projectId": project_id
            })),
        )
        .expect("project selects");
}

#[tokio::test]
async fn session_rebind_transfers_project_teleport_and_loop_ownership() {
    let temporary = tempdir().expect("temporary directory");
    let git = Arc::new(FixtureGit {
        snapshot: GitSnapshot {
            repository: "fixture".to_owned(),
            dirty: true,
            unpushed: true,
        },
        pushed: AtomicBool::new(false),
    });
    let teleport = Arc::new(FixtureTeleport {
        fail: AtomicBool::new(false),
    });
    let service = fixture_service(git, teleport)
        .with_loop_store(temporary.path().join("loops.json"))
        .expect("loop store");
    let picker_id = open_picker(&service, "session-old").await;
    select_project(&service, "session-old", &picker_id, "page-first");
    let teleport = service
        .dispatch_deferred(
            "vibeCode/teleport/start",
            &params(json!({
                "sessionId": "session-old",
                "pickerId": picker_id,
                "operationId": "operation-rebind",
                "projectId": "page-first",
                "workingDirectory": "/workspace",
                "prompt": "context"
            })),
        )
        .await
        .expect("teleport starts");
    let operation_id = teleport.result["operationId"]
        .as_str()
        .expect("operation ID")
        .to_owned();
    service
        .dispatch(
            "loops/create",
            &params(json!({
                "sessionId": "session-old",
                "prompt": "review",
                "interval": "30s",
                "nowSeconds": 10
            })),
        )
        .expect("loop creates");

    service
        .rebind_session("session-old", "session-new")
        .expect("state rebinds");

    assert!(matches!(
        service.dispatch(
            "vibeCode/projects/select",
            &params(json!({
                "sessionId": "session-old",
                "pickerId": picker_id,
                "projectId": "page-first"
            }))
        ),
        Err(ProjectsServiceError::NotFound(_))
    ));
    service
        .dispatch(
            "vibeCode/projects/select",
            &params(json!({
                "sessionId": "session-new",
                "pickerId": picker_id,
                "projectId": "page-first"
            })),
        )
        .expect("new session owns picker");
    service
        .dispatch_deferred(
            "vibeCode/teleport/push/respond",
            &params(json!({
                "sessionId": "session-new",
                "operationId": operation_id,
                "approved": false
            })),
        )
        .await
        .expect("new session owns teleport");
    let listed = service
        .dispatch("loops/list", &params(json!({"sessionId": "session-new"})))
        .expect("new session owns loop");
    assert_eq!(listed.result["loops"].as_array().map(Vec::len), Some(1));
    let old = service
        .dispatch("loops/list", &params(json!({"sessionId": "session-old"})))
        .expect("old session has no loops");
    assert_eq!(old.result["loops"].as_array().map(Vec::len), Some(0));
    service
        .close_transient_session("session-new")
        .expect("transient state closes");
    assert!(matches!(
        service.dispatch(
            "vibeCode/projects/select",
            &params(json!({
                "sessionId": "session-new",
                "pickerId": picker_id,
                "projectId": "page-first"
            }))
        ),
        Err(ProjectsServiceError::NotFound(_))
    ));
    let durable = service
        .dispatch("loops/list", &params(json!({"sessionId": "session-new"})))
        .expect("durable loops survive close");
    assert_eq!(durable.result["loops"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn headless_project_resolution_paginates_and_selects_one_exact_writable_match() {
    let repo_url = "https://github.com/mistralai/mistral-vibe.git";
    let cloud = Arc::new(HeadlessProjects::new([
        (
            None,
            ProjectPage {
                projects: vec![
                    listed_project(
                        "wrong-repo",
                        &["https://github.com/mistralai/other.git"],
                        false,
                    ),
                    listed_project("read-only", &[repo_url], true),
                ],
                next_cursor: Some("next-page".to_owned()),
            },
        ),
        (
            Some("next-page"),
            ProjectPage {
                projects: vec![listed_project(
                    "exact-match",
                    &["git@github.com:MistralAI/mistral-vibe.git"],
                    false,
                )],
                next_cursor: None,
            },
        ),
    ]));
    let service = headless_service(cloud.clone());

    let opened = open_headless_project(&service, "headless-one").await;

    assert_eq!(opened.result["resolvedProjectId"], json!("exact-match"));
    assert_eq!(
        cloud.list_calls.lock().expect("list calls").as_slice(),
        &[None, Some("next-page".to_owned())]
    );
    assert!(cloud.create_calls.lock().expect("create calls").is_empty());

    let reopened = open_headless_project(&service, "headless-saved").await;
    assert_eq!(reopened.result["resolvedProjectId"], json!("exact-match"));
    assert_eq!(
        cloud.list_calls.lock().expect("list calls").len(),
        2,
        "a matching saved link must bypass cloud pagination"
    );
}

#[tokio::test]
async fn headless_project_resolution_creates_when_no_project_matches() {
    let cloud = Arc::new(HeadlessProjects::new([(
        None,
        ProjectPage {
            projects: Vec::new(),
            next_cursor: None,
        },
    )]));
    let service = headless_service(cloud.clone());

    let opened = open_headless_project(&service, "headless-zero").await;

    assert_eq!(opened.result["resolvedProjectId"], json!("created-project"));
    assert_eq!(
        cloud.create_calls.lock().expect("create calls").as_slice(),
        &[(
            "mistral-vibe".to_owned(),
            "https://github.com/mistralai/mistral-vibe.git".to_owned(),
            "feature/headless".to_owned(),
        )]
    );
}

#[tokio::test]
async fn headless_project_resolution_creates_for_ambiguous_exact_matches() {
    let repo_url = "https://github.com/mistralai/mistral-vibe.git";
    let cloud = Arc::new(HeadlessProjects::new([(
        None,
        ProjectPage {
            projects: vec![
                listed_project("match-one", &[repo_url], false),
                listed_project("match-two", &[repo_url], false),
            ],
            next_cursor: None,
        },
    )]));
    let service = headless_service(cloud.clone());

    let opened = open_headless_project(&service, "headless-ambiguous").await;

    assert_eq!(opened.result["resolvedProjectId"], json!("created-project"));
    assert_eq!(cloud.create_calls.lock().expect("create calls").len(), 1);
}

#[tokio::test]
async fn headless_project_resolution_ignores_read_only_and_multi_repo_matches() {
    let repo_url = "https://github.com/mistralai/mistral-vibe.git";
    let cloud = Arc::new(HeadlessProjects::new([(
        None,
        ProjectPage {
            projects: vec![
                listed_project("read-only", &[repo_url], true),
                listed_project(
                    "multi-repo",
                    &[repo_url, "https://github.com/mistralai/other.git"],
                    false,
                ),
            ],
            next_cursor: None,
        },
    )]));
    let service = headless_service(cloud.clone());

    let opened = open_headless_project(&service, "headless-ineligible").await;

    assert_eq!(opened.result["resolvedProjectId"], json!("created-project"));
    assert_eq!(cloud.create_calls.lock().expect("create calls").len(), 1);
}

#[tokio::test]
async fn project_selection_rejects_wrong_or_omitted_repository_metadata() {
    let cloud = Arc::new(HeadlessProjects::new([(
        None,
        ProjectPage {
            projects: vec![
                listed_project(
                    "wrong-repo",
                    &["https://github.com/mistralai/other.git"],
                    false,
                ),
                listed_project("repositories-omitted", &[], false),
            ],
            next_cursor: None,
        },
    )]));
    let service = headless_service(cloud);
    let opened = service
        .dispatch_deferred(
            "vibeCode/projects/open",
            &params(json!({
                "sessionId": "configure",
                "workingDirectory": "/repo/mistral-vibe",
                "purpose": "configure",
            })),
        )
        .await
        .expect("configure picker opens");
    let picker_id = opened.result["pickerId"].as_str().expect("picker ID");

    for project_id in ["wrong-repo", "repositories-omitted"] {
        assert!(matches!(
            service.dispatch(
                "vibeCode/projects/select",
                &params(json!({
                    "sessionId": "configure",
                    "pickerId": picker_id,
                    "projectId": project_id,
                })),
            ),
            Err(ProjectsServiceError::InvalidParams(message))
                if message.contains("not linked")
        ));
    }
}

#[tokio::test]
async fn project_load_more_skips_ineligible_pages_and_focuses_the_first_selectable_project() {
    let repo_url = "https://github.com/mistralai/mistral-vibe.git";
    let cloud = Arc::new(HeadlessProjects::new([
        (
            None,
            ProjectPage {
                projects: vec![listed_project(
                    "initial-wrong-repo",
                    &["https://github.com/mistralai/other.git"],
                    false,
                )],
                next_cursor: Some("ineligible".to_owned()),
            },
        ),
        (
            Some("ineligible"),
            ProjectPage {
                projects: vec![listed_project("read-only", &[repo_url], true)],
                next_cursor: Some("selectable".to_owned()),
            },
        ),
        (
            Some("selectable"),
            ProjectPage {
                projects: vec![listed_project("eligible", &[repo_url], false)],
                next_cursor: Some("tail".to_owned()),
            },
        ),
    ]));
    let service = headless_service(cloud.clone());
    let opened = service
        .dispatch_deferred(
            "vibeCode/projects/open",
            &params(json!({
                "sessionId": "configure",
                "workingDirectory": "/repo/mistral-vibe",
                "purpose": "configure",
            })),
        )
        .await
        .expect("configure picker opens");
    let picker_id = opened.result["pickerId"].as_str().expect("picker ID");

    let loaded = service
        .dispatch_deferred(
            "vibeCode/projects/loadMore",
            &params(json!({
                "sessionId": "configure",
                "pickerId": picker_id,
            })),
        )
        .await
        .expect("eligible page loads");

    assert_eq!(loaded.result["focusOptionId"], json!("project:eligible"));
    assert_eq!(loaded.result["view"]["state"]["nextCursor"], json!("tail"));
    assert_eq!(
        cloud.list_calls.lock().expect("list calls").as_slice(),
        &[
            None,
            Some("ineligible".to_owned()),
            Some("selectable".to_owned()),
        ]
    );
}

#[tokio::test]
async fn headless_project_resolution_rejects_repeated_pagination_cursors() {
    let cloud = Arc::new(HeadlessProjects::new([
        (
            None,
            ProjectPage {
                projects: Vec::new(),
                next_cursor: Some("repeat".to_owned()),
            },
        ),
        (
            Some("repeat"),
            ProjectPage {
                projects: Vec::new(),
                next_cursor: Some("repeat".to_owned()),
            },
        ),
    ]));
    let service = headless_service(cloud);

    assert!(matches!(
        service
            .dispatch_deferred(
                "vibeCode/projects/open",
                &params(json!({
                    "sessionId": "headless-repeat",
                    "workingDirectory": "/repo/mistral-vibe",
                    "purpose": "teleport",
                })),
            )
            .await,
        Err(ProjectsServiceError::Conflict(message)) if message.contains("repeated a cursor")
    ));
}

#[tokio::test]
async fn projects_mutate_local_selection_only_after_cloud_success() {
    let git = Arc::new(FixtureGit {
        snapshot: GitSnapshot {
            repository: "fixture".to_owned(),
            dirty: false,
            unpushed: false,
        },
        pushed: AtomicBool::new(false),
    });
    let teleport = Arc::new(FixtureTeleport {
        fail: AtomicBool::new(false),
    });
    let service = fixture_service(git, teleport);
    let opened = service
        .dispatch_deferred(
            "vibeCode/projects/open",
            &params(json!({"sessionId": "session-1"})),
        )
        .await
        .expect("picker opens");
    let picker_id = opened.result["pickerId"].as_str().expect("picker id");
    assert!(matches!(
        service.dispatch(
            "vibeCode/projects/select",
            &params(json!({
                "sessionId": "session-1",
                "pickerId": picker_id,
                "projectId": "read-only-first"
            }))
        ),
        Err(ProjectsServiceError::InvalidParams(message))
            if message.contains("read-only")
    ));
    let unchanged = service
        .dispatch(
            "vibeCode/projects/recover",
            &params(json!({
                "sessionId": "session-1",
                "pickerId": picker_id
            })),
        )
        .expect("picker remains valid");
    assert_eq!(
        unchanged.result["view"]["context"]["savedLink"],
        Value::Null
    );
    let exhausted = service
        .dispatch_deferred(
            "vibeCode/projects/loadMore",
            &params(json!({
                "sessionId": "session-1",
                "pickerId": picker_id
            })),
        )
        .await
        .expect("next page loads");
    assert_eq!(exhausted.result["view"]["state"]["nextCursor"], Value::Null);
    let repeated = service
        .dispatch_deferred(
            "vibeCode/projects/loadMore",
            &params(json!({
                "sessionId": "session-1",
                "pickerId": picker_id
            })),
        )
        .await
        .expect("exhausted picker is stable");
    assert_eq!(repeated.result["view"]["state"]["nextCursor"], Value::Null);
    let created = service
        .dispatch_deferred(
            "vibeCode/projects/create",
            &params(json!({
                "sessionId": "session-1",
                "pickerId": picker_id,
                "name": "alpha",
                "defaultBranch": "main"
            })),
        )
        .await
        .expect("project create");
    assert_eq!(
        created.result["project"]["projectId"],
        json!("project-alpha")
    );
    let unlinked = service
        .dispatch(
            "vibeCode/projects/unlink",
            &params(json!({"sessionId": "session-1", "pickerId": picker_id})),
        )
        .expect("unlink");
    assert_eq!(unlinked.result["view"]["context"]["savedLink"], Value::Null);

    let unavailable = ProjectsService::default();
    assert!(matches!(
        unavailable
            .dispatch_deferred(
                "vibeCode/projects/open",
                &params(json!({"sessionId": "session-2"}))
            )
            .await,
        Err(ProjectsServiceError::Cloud(CloudError::Git(_)))
    ));
    assert!(matches!(
        unavailable.dispatch(
            "vibeCode/projects/select",
            &params(json!({
                "sessionId": "session-2",
                "pickerId": "missing",
                "projectId": "project-beta"
            }))
        ),
        Err(ProjectsServiceError::NotFound(_))
    ));
}

#[tokio::test]
async fn project_links_resolve_by_repo_root_for_open_recover_and_teleport() {
    let git = Arc::new(FixtureGit {
        snapshot: GitSnapshot {
            repository: "fixture".to_owned(),
            dirty: true,
            unpushed: true,
        },
        pushed: AtomicBool::new(false),
    });
    let teleport = Arc::new(FixtureTeleport {
        fail: AtomicBool::new(false),
    });
    let service = fixture_service(git, teleport);
    let pending = service
        .dispatch_deferred(
            "vibeCode/projects/open",
            &params(json!({
                "sessionId": "session-pending",
                "workingDirectory": "/workspace/repo"
            })),
        )
        .await
        .expect("pending picker opens");
    let pending_picker_id = pending.result["pickerId"]
        .as_str()
        .expect("pending picker ID");
    let source = service
        .dispatch_deferred(
            "vibeCode/projects/open",
            &params(json!({
                "sessionId": "session-source",
                "workingDirectory": "/workspace/repo"
            })),
        )
        .await
        .expect("source picker opens");
    let source_picker_id = source.result["pickerId"]
        .as_str()
        .expect("source picker ID");
    select_project(&service, "session-source", source_picker_id, "page-first");

    let recovered = service
        .dispatch(
            "vibeCode/projects/recover",
            &params(json!({
                "sessionId": "session-pending",
                "pickerId": pending_picker_id
            })),
        )
        .expect("pending picker resolves link");
    assert_eq!(recovered.result["recovered"], json!(true));
    assert_eq!(
        recovered.result["view"]["context"]["savedLink"]["projectId"],
        json!("page-first")
    );

    let reopened = service
        .dispatch_deferred(
            "vibeCode/projects/open",
            &params(json!({
                "sessionId": "session-teleport",
                "workingDirectory": "/workspace/repo"
            })),
        )
        .await
        .expect("linked picker reopens");
    assert_eq!(reopened.result["resolvedProjectId"], json!("page-first"));
    let reopened_picker_id = reopened.result["pickerId"]
        .as_str()
        .expect("reopened picker ID");
    service
        .dispatch_deferred(
            "vibeCode/teleport/start",
            &params(json!({
                "sessionId": "session-teleport",
                "pickerId": reopened_picker_id,
                "operationId": "operation-linked",
                "projectId": "page-first"
            })),
        )
        .await
        .expect("resolved link satisfies Teleport validation");

    service
        .dispatch(
            "vibeCode/projects/unlink",
            &params(json!({
                "sessionId": "session-teleport",
                "pickerId": reopened_picker_id
            })),
        )
        .expect("project unlinks");
    let unlinked = service
        .dispatch_deferred(
            "vibeCode/projects/open",
            &params(json!({
                "sessionId": "session-unlinked",
                "workingDirectory": "/workspace/repo"
            })),
        )
        .await
        .expect("unlinked picker reopens");
    assert_eq!(unlinked.result["resolvedProjectId"], Value::Null);
}

#[tokio::test]
async fn project_links_survive_service_restart() {
    let temporary = tempdir().expect("project link store");
    let link_store = temporary.path().join("project-links.json");
    let git = Arc::new(FixtureGit {
        snapshot: GitSnapshot {
            repository: "https://github.com/owner/repo.git".to_owned(),
            dirty: false,
            unpushed: false,
        },
        pushed: AtomicBool::new(false),
    });
    let teleport = Arc::new(FixtureTeleport {
        fail: AtomicBool::new(false),
    });
    let first = fixture_service(git.clone(), teleport.clone())
        .with_project_link_store(link_store.clone())
        .expect("project link store");
    let picker_id = first
        .dispatch_deferred(
            "vibeCode/projects/open",
            &params(json!({
                "sessionId": "session-first",
                "workingDirectory": "/workspace/repo",
            })),
        )
        .await
        .expect("picker opens")
        .result["pickerId"]
        .as_str()
        .expect("picker ID")
        .to_owned();
    select_project(&first, "session-first", &picker_id, "page-first");
    drop(first);

    let restarted = fixture_service(git, teleport)
        .with_project_link_store(link_store)
        .expect("reloaded project link store");
    let opened = restarted
        .dispatch_deferred(
            "vibeCode/projects/open",
            &params(json!({
                "sessionId": "session-restarted",
                "workingDirectory": "/workspace/repo",
            })),
        )
        .await
        .expect("picker reopens");
    assert_eq!(opened.result["resolvedProjectId"], "page-first");
}

#[tokio::test]
async fn project_links_use_canonical_root_and_clear_changed_remotes() {
    let repository = committed_github_repository();
    let nested = repository.path().join("nested/deeper");
    fs::create_dir_all(&nested).expect("nested directory");
    let temporary = tempdir().expect("project link store");
    let link_store = temporary.path().join("project-links.json");
    let git = Arc::new(CommandGitProbe::default());
    let teleport = Arc::new(FixtureTeleport {
        fail: AtomicBool::new(false),
    });
    let service = ProjectsService::with_backends(Arc::new(FixtureProjects), teleport, git)
        .with_project_link_store(link_store)
        .expect("project link store");
    let root_picker = service
        .dispatch_deferred(
            "vibeCode/projects/open",
            &params(json!({
                "sessionId": "session-root",
                "workingDirectory": repository.path(),
            })),
        )
        .await
        .expect("root picker")
        .result["pickerId"]
        .as_str()
        .expect("root picker ID")
        .to_owned();
    select_project(&service, "session-root", &root_picker, "page-first");

    let nested_open = service
        .dispatch_deferred(
            "vibeCode/projects/open",
            &params(json!({
                "sessionId": "session-nested",
                "workingDirectory": nested,
            })),
        )
        .await
        .expect("nested picker");
    assert_eq!(nested_open.result["resolvedProjectId"], "page-first");
    assert_eq!(
        nested_open.result["view"]["context"]["repoRoot"],
        json!(
            repository
                .path()
                .canonicalize()
                .expect("canonical repository")
                .to_string_lossy()
        )
    );

    run_test_git(
        repository.path(),
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/other/repo.git",
        ],
    );
    let changed = service
        .dispatch_deferred(
            "vibeCode/projects/open",
            &params(json!({
                "sessionId": "session-changed",
                "workingDirectory": repository.path(),
            })),
        )
        .await
        .expect("changed remote picker");
    assert_eq!(changed.result["resolvedProjectId"], Value::Null);
    assert_eq!(
        changed.result["view"]["savedProjectLinkCleared"],
        json!(true)
    );
    assert_eq!(
        changed.result["view"]["projectRepoRemoteChanged"],
        json!(true)
    );
    assert_eq!(changed.result["view"]["context"]["savedLink"], Value::Null);
}

#[tokio::test]
async fn teleport_push_answer_is_idempotent_and_failures_are_actionable() {
    let git = Arc::new(FixtureGit {
        snapshot: GitSnapshot {
            repository: "fixture".to_owned(),
            dirty: true,
            unpushed: true,
        },
        pushed: AtomicBool::new(false),
    });
    let teleport = Arc::new(FixtureTeleport {
        fail: AtomicBool::new(false),
    });
    let service = fixture_service(git.clone(), teleport.clone());
    let picker_id = open_picker(&service, "session-1").await;
    select_project(&service, "session-1", &picker_id, "page-first");
    let started = service
        .dispatch_deferred(
            "vibeCode/teleport/start",
            &params(json!({
                "sessionId": "session-1",
                "pickerId": picker_id,
                "operationId": "operation-1",
                "projectId": "page-first",
                "workingDirectory": "/workspace",
                "prompt": "context"
            })),
        )
        .await
        .expect("start");
    let operation_id = started.result["operationId"]
        .as_str()
        .expect("operation id");
    assert_eq!(
        started.notifications.last().expect("push event").params["event"]["kind"],
        json!("push_required")
    );
    let completed = service
        .dispatch_deferred(
            "vibeCode/teleport/push/respond",
            &params(json!({
                "sessionId": "session-1",
                "operationId": operation_id,
                "approved": true
            })),
        )
        .await
        .expect("push response");
    assert_eq!(
        completed
            .notifications
            .last()
            .expect("complete event")
            .params["event"]["kind"],
        json!("complete")
    );
    assert!(git.pushed.load(AtomicOrdering::Relaxed));
    let duplicate = service
        .dispatch_deferred(
            "vibeCode/teleport/push/respond",
            &params(json!({
                "sessionId": "session-1",
                "operationId": operation_id,
                "approved": true
            })),
        )
        .await
        .expect("identical retry");
    assert!(duplicate.result.is_empty());
    assert!(matches!(
        service
            .dispatch_deferred(
                "vibeCode/teleport/push/respond",
                &params(json!({
                    "sessionId": "session-1",
                    "operationId": operation_id,
                    "approved": false
                }))
            )
            .await,
        Err(ProjectsServiceError::Conflict(_))
    ));

    teleport.fail.store(true, AtomicOrdering::Relaxed);
    let picker_id = open_picker(&service, "session-2").await;
    select_project(&service, "session-2", &picker_id, "page-first");
    let pending = service
        .dispatch_deferred(
            "vibeCode/teleport/start",
            &params(json!({
                "sessionId": "session-2",
                "pickerId": picker_id,
                "operationId": "operation-2",
                "projectId": "page-first",
                "workingDirectory": "/workspace",
                "prompt": "context"
            })),
        )
        .await
        .expect("Teleport waits for push approval");
    let failed = service
        .dispatch_deferred(
            "vibeCode/teleport/push/respond",
            &params(json!({
                "sessionId": "session-2",
                "operationId": pending.result["operationId"],
                "approved": true
            })),
        )
        .await
        .expect("typed cloud failure");
    let event = &failed.notifications.last().expect("failure event").params["event"];
    assert_eq!(event["kind"], json!("failed"));
    // US-011: the status the service answered with travels in the failure
    // details, because it is what tells a saved project link the service
    // refused from an ordinary outage.
    assert_eq!(event["error"]["details"]["httpStatusCode"], json!(403));
}

#[tokio::test]
async fn teleport_requires_an_owned_selected_project_and_cancel_is_silent() {
    let git = Arc::new(FixtureGit {
        snapshot: GitSnapshot {
            repository: "fixture".to_owned(),
            dirty: true,
            unpushed: true,
        },
        pushed: AtomicBool::new(false),
    });
    let teleport = Arc::new(FixtureTeleport {
        fail: AtomicBool::new(false),
    });
    let service = fixture_service(git, teleport);
    let picker_id = open_picker(&service, "session-1").await;

    assert!(matches!(
        service.dispatch_deferred(
            "vibeCode/teleport/start",
            &params(json!({
                "sessionId": "session-1",
                "pickerId": picker_id,
                "operationId": "operation-unselected",
                "projectId": "page-first"
            }))
        )
        .await,
        Err(ProjectsServiceError::Conflict(message))
            if message.contains("selected")
    ));
    select_project(&service, "session-1", &picker_id, "page-first");
    assert!(matches!(
        service
            .dispatch_deferred(
                "vibeCode/teleport/start",
                &params(json!({
                    "sessionId": "session-2",
                    "pickerId": picker_id,
                    "operationId": "operation-foreign",
                    "projectId": "page-first"
                }))
            )
            .await,
        Err(ProjectsServiceError::NotFound(_))
    ));
    assert!(matches!(
        service
            .dispatch_deferred(
                "vibeCode/teleport/start",
                &params(json!({
                    "sessionId": "session-1",
                    "pickerId": picker_id,
                    "operationId": "operation-missing-project",
                    "projectId": "missing"
                }))
            )
            .await,
        Err(ProjectsServiceError::NotFound(_))
    ));

    let started = service
        .dispatch_deferred(
            "vibeCode/teleport/start",
            &params(json!({
                "sessionId": "session-1",
                "pickerId": picker_id,
                "operationId": "operation-cancel",
                "projectId": "page-first"
            })),
        )
        .await
        .expect("valid Teleport starts");
    assert_eq!(
        started.notifications.last().expect("push event").params["event"]["kind"],
        json!("push_required")
    );
    let cancelled = service
        .dispatch(
            "vibeCode/teleport/cancel",
            &params(json!({
                "sessionId": "session-1",
                "operationId": "operation-cancel"
            })),
        )
        .expect("Teleport cancels");
    assert_eq!(cancelled.result["cancelled"], json!(true));
    assert!(cancelled.notifications.is_empty());
}

#[tokio::test]
async fn teleport_cancel_rejects_irreversible_work() {
    let git = Arc::new(FixtureGit {
        snapshot: GitSnapshot {
            repository: "fixture".to_owned(),
            dirty: true,
            unpushed: true,
        },
        pushed: AtomicBool::new(false),
    });
    let teleport = Arc::new(FixtureTeleport {
        fail: AtomicBool::new(false),
    });
    let service = fixture_service(git, teleport);
    let picker_id = open_picker(&service, "session-1").await;
    select_project(&service, "session-1", &picker_id, "page-first");
    service
        .dispatch_deferred(
            "vibeCode/teleport/start",
            &params(json!({
                "sessionId": "session-1",
                "pickerId": picker_id,
                "operationId": "operation-irreversible",
                "projectId": "page-first"
            })),
        )
        .await
        .expect("Teleport waits for push approval");

    for state in [TeleportState::Pushing, TeleportState::StartingWorkflow] {
        service
            .lock_teleports()
            .expect("teleport state")
            .get_mut("operation-irreversible")
            .expect("Teleport operation")
            .state = state;
        assert!(matches!(
            service.dispatch(
                "vibeCode/teleport/cancel",
                &params(json!({
                    "sessionId": "session-1",
                    "operationId": "operation-irreversible"
                })),
            ),
            Err(ProjectsServiceError::Conflict(message))
                if message.contains("irreversible")
        ));
    }
}

#[test]
fn scheduled_loops_are_owned_persistent_and_retry_safe() {
    let temporary = tempdir().expect("temporary directory");
    let path = temporary.path().join("loops.json");
    let service = ProjectsService::default()
        .with_loop_store(path.clone())
        .expect("loop store");
    let created = service
        .dispatch(
            "loops/create",
            &params(json!({
                "sessionId": "session-1",
                "prompt": "review",
                "interval": "30s",
                "nowSeconds": 10
            })),
        )
        .expect("create");
    let loop_id = created.result["loop"]["id"]
        .as_str()
        .expect("loop id")
        .to_owned();
    assert_eq!(loop_id.len(), 8);
    assert!(
        loop_id
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    );
    assert!(matches!(
        service.fire_loop(&loop_id, 39, true),
        Err(ProjectsServiceError::Conflict(_))
    ));
    let fired = service.fire_loop(&loop_id, 40, true).expect("due loop");
    assert_eq!(fired.prompt, "review");
    assert!(matches!(
        service.fire_loop(&loop_id, 40, true),
        Err(ProjectsServiceError::Conflict(_))
    ));
    drop(service);

    let reloaded = ProjectsService::default()
        .with_loop_store(path)
        .expect("reload");
    assert!(matches!(
        reloaded.fire_loop(&loop_id, 40, true),
        Err(ProjectsServiceError::Conflict(_))
    ));
    reloaded
        .fire_loop(&loop_id, 70, true)
        .expect("interrupted running state preserves cadence");
    reloaded.finish_loop_fire(&loop_id, 71).expect("finish");
    let listed = reloaded
        .dispatch("loops/list", &params(json!({"sessionId": "session-1"})))
        .expect("list");
    assert_eq!(
        listed.result["loops"][0]["nextFireAt"].as_f64(),
        Some(100.0)
    );
    assert!(matches!(
        reloaded.dispatch(
            "loops/delete",
            &params(json!({"sessionId": "another", "loopId": loop_id}))
        ),
        Err(ProjectsServiceError::NotFound(_))
    ));
}

#[test]
fn session_removal_deletes_owned_loops_transactionally_and_durably() {
    let temporary = tempdir().expect("loop store");
    let loop_path = temporary.path().join("loops.json");
    let mut service = ProjectsService::default()
        .with_loop_store(loop_path.clone())
        .expect("loop store");
    for (session_id, prompt) in [
        ("session-delete", "first"),
        ("session-delete", "second"),
        ("session-keep", "keep"),
    ] {
        service
            .dispatch(
                "loops/create",
                &params(json!({
                    "sessionId": session_id,
                    "prompt": prompt,
                    "interval": "30s",
                    "nowSeconds": 10,
                })),
            )
            .expect("loop creates");
    }

    service.loop_store = temporary.path().to_path_buf();
    assert!(matches!(
        service.remove_session("session-delete"),
        Err(ProjectsServiceError::Persistence(_))
    ));
    let unchanged = service
        .dispatch(
            "loops/list",
            &params(json!({"sessionId": "session-delete"})),
        )
        .expect("loops remain after failed persistence");
    assert_eq!(unchanged.result["loops"].as_array().map(Vec::len), Some(2));

    service.loop_store = loop_path.clone();
    assert_eq!(
        service
            .remove_session("session-delete")
            .expect("session removal"),
        2
    );
    drop(service);
    let reloaded = ProjectsService::default()
        .with_loop_store(loop_path)
        .expect("reloaded loop store");
    let deleted = reloaded
        .dispatch(
            "loops/list",
            &params(json!({"sessionId": "session-delete"})),
        )
        .expect("deleted session loops");
    assert_eq!(deleted.result["loops"].as_array().map(Vec::len), Some(0));
    let kept = reloaded
        .dispatch("loops/list", &params(json!({"sessionId": "session-keep"})))
        .expect("unrelated session loops");
    assert_eq!(kept.result["loops"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn session_removal_token_restores_exact_transient_state_and_loops_durably() {
    let temporary = tempdir().expect("session rollback store");
    let loop_path = temporary.path().join("loops.json");
    let git = Arc::new(FixtureGit {
        snapshot: GitSnapshot {
            repository: "fixture".to_owned(),
            dirty: false,
            unpushed: false,
        },
        pushed: AtomicBool::new(false),
    });
    let teleport = Arc::new(FixtureTeleport {
        fail: AtomicBool::new(false),
    });
    let mut service = fixture_service(git, teleport)
        .with_loop_store(loop_path.clone())
        .expect("loop store");
    let picker_id = open_picker(&service, "session-rollback").await;
    let original_picker_view = {
        let projects = service.lock_projects().expect("project state");
        project_view(projects.pickers.get(&picker_id).expect("original picker"))
    };
    let original_operation = TeleportOperation {
        id: "rollback-operation".to_owned(),
        session_id: "session-rollback".to_owned(),
        project_id: "page-first".to_owned(),
        working_directory: PathBuf::from("/repo"),
        summary: "continue".to_owned(),
        repository: TeleportRepository {
            repo_url: "fixture".to_owned(),
            branch: Some("main".to_owned()),
            commit_sha: Some("abcdef0".to_owned()),
            diff: None,
        },
        state: TeleportState::PushRequired,
        push_response: None,
        unpushed_count: 1,
        branch_not_pushed: false,
        url: None,
        error: None,
        error_status: None,
    };
    service
        .lock_teleports()
        .expect("teleport state")
        .insert(original_operation.id.clone(), original_operation.clone());
    service
        .dispatch(
            "loops/create",
            &params(json!({
                "sessionId": "session-rollback",
                "prompt": "durable",
                "interval": "30s",
                "nowSeconds": 10,
            })),
        )
        .expect("loop creates");

    let removal = service
        .remove_session_transactional("session-rollback")
        .expect("transactional removal");
    assert_eq!(removal.session_id(), "session-rollback");
    assert_eq!(removal.removed_loop_count(), 1);
    assert!(
        !service
            .lock_projects()
            .expect("project state")
            .pickers
            .contains_key(&picker_id)
    );
    assert!(
        !service
            .lock_teleports()
            .expect("teleport state")
            .contains_key(&original_operation.id)
    );

    let blocking_path = temporary.path().join("restore-blocked");
    fs::create_dir(&blocking_path).expect("blocking directory");
    service.loop_store = blocking_path;
    assert!(matches!(
        service.restore_session(&removal),
        Err(ProjectsServiceError::Persistence(_))
    ));
    assert!(
        !service
            .lock_projects()
            .expect("project state")
            .pickers
            .contains_key(&picker_id),
        "failed durable restoration must roll transient state back to removed"
    );
    assert!(
        !service
            .lock_teleports()
            .expect("teleport state")
            .contains_key(&original_operation.id)
    );

    service.loop_store = loop_path.clone();
    service
        .restore_session(&removal)
        .expect("session restoration");
    let restored_picker_view = {
        let projects = service.lock_projects().expect("project state");
        project_view(projects.pickers.get(&picker_id).expect("restored picker"))
    };
    assert_eq!(restored_picker_view, original_picker_view);
    assert_eq!(
        service
            .lock_teleports()
            .expect("teleport state")
            .get(&original_operation.id),
        Some(&original_operation)
    );
    let restored_loops = service
        .dispatch(
            "loops/list",
            &params(json!({"sessionId": "session-rollback"})),
        )
        .expect("restored loops");
    assert_eq!(
        restored_loops.result["loops"].as_array().map(Vec::len),
        Some(1)
    );

    drop(service);
    let reloaded = ProjectsService::default()
        .with_loop_store(loop_path)
        .expect("reloaded restored loops");
    let durable = reloaded
        .dispatch(
            "loops/list",
            &params(json!({"sessionId": "session-rollback"})),
        )
        .expect("durable restored loops");
    assert_eq!(durable.result["loops"].as_array().map(Vec::len), Some(1));
}
