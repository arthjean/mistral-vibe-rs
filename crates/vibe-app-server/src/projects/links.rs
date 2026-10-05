//! The session-less `projectLinks/*` surface.
//!
//! Reference `ProjectLinksController` (`vibe/app_server/_project_links.py`):
//! every method is keyed on a `rootPath` the caller holds rather than on a
//! session, which is what lets a picker run before a session exists. It reads
//! and writes the store a session's Teleport picker shares
//! ([`ProjectsStore`]), so a link saved on either side is the one the other
//! sees.
//!
//! Two readings of a directory are in play, as in the reference. The
//! inspection behind `resolveRoot`, `inspectRoot`, `save` and `unlink` accepts
//! any directory and describes its Git checkout when it has one. The picker
//! and the two mutations that reach Vibe Code (`picker/*`, `create`, `link`)
//! need what a Teleport run needs, a checkout with a GitHub remote and a
//! commit, and refuse anything else as an invalid request.
//!
//! Failures are classified the way the reference classifies them: no Mistral
//! key, or a Vibe Code answer naming the key or a 401 or 403, is
//! `unauthorized`; any other Vibe Code failure is `internal_error` with a
//! message that carries nothing the service answered; a root that cannot be
//! linked is `invalid_params`; and a store write that fails where the
//! reference does not catch it is `internal_error` with the operating
//! system's own report. The sentences are this port's own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use vibe_protocol::ProtocolErrorCode;

use super::store::{ProjectLink, ProjectsStore, StoreError, resolve_path};
use crate::host::expand_home;
use crate::vibe_code::Service;
use crate::vibe_code::git::{FailureClass, GitRepoInfo, GitRepository, normalize_repo_url};
use crate::vibe_code::http::Project;
use crate::vibe_code::is_project_linked_to_repo;
use crate::workspace::WorkspaceService;

/// Why a `projectLinks/*` call was refused, under the code the reference
/// answers it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    Unauthorized(String),
    Invalid(String),
    Internal(String),
}

impl Failure {
    pub(crate) const fn code(&self) -> ProtocolErrorCode {
        match self {
            Self::Unauthorized(_) => ProtocolErrorCode::Unauthorized,
            Self::Invalid(_) => ProtocolErrorCode::InvalidParams,
            Self::Internal(_) => ProtocolErrorCode::InternalError,
        }
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Unauthorized(message) | Self::Invalid(message) | Self::Internal(message) => {
                message
            }
        }
    }
}

impl From<StoreError> for Failure {
    fn from(error: StoreError) -> Self {
        Self::Internal(error.to_string())
    }
}

const NO_KEY: &str = "No Mistral API key is set.";
const REMOTE_CHANGED: &str =
    "The repository's GitHub remote is not the one the link was prepared for.";

/// Serves one `projectLinks/*` call against the store and configuration of
/// `workspace`'s vibe home.
pub(crate) async fn dispatch(
    workspace: &WorkspaceService,
    method: &str,
    params: &BTreeMap<String, Value>,
) -> Result<Value, Failure> {
    let links = ProjectLinks {
        workspace,
        store: ProjectsStore::in_home(workspace.vibe_home()),
    };
    match method {
        "projectLinks/list" => Ok(links.list().await),
        "projectLinks/resolveRoot" => Ok(links.resolve_root(text(params, "rootPath")?).await),
        "projectLinks/inspectRoot" => Ok(links.inspect_root(text(params, "rootPath")?).await),
        "projectLinks/picker/load" => links.picker_load(text(params, "rootPath")?).await,
        "projectLinks/picker/loadMore" => {
            links
                .picker_load_more(text(params, "rootPath")?, text(params, "cursor")?)
                .await
        }
        "projectLinks/create" => {
            links
                .create(
                    text(params, "rootPath")?,
                    text(params, "name")?,
                    text(params, "defaultBranch")?,
                )
                .await
        }
        "projectLinks/link" => {
            // The name a caller sends is required and never written: the link
            // takes the validated project's own.
            text(params, "projectName")?;
            links
                .link(text(params, "rootPath")?, text(params, "projectId")?)
                .await
        }
        "projectLinks/save" => {
            let expected = match params.get("expectedGithubRepoUrl") {
                Some(Value::String(url)) => Some(url.as_str()),
                Some(Value::Null) => None,
                _ => {
                    return Err(Failure::Invalid(
                        "expectedGithubRepoUrl must be a string or null".to_owned(),
                    ));
                }
            };
            links
                .save(
                    text(params, "rootPath")?,
                    text(params, "projectId")?,
                    text(params, "projectName")?,
                    expected,
                )
                .await
        }
        "projectLinks/unlink" => Ok(links.unlink(text(params, "rootPath")?).await),
        _ => Err(Failure::Invalid(format!("Unknown method {method}"))),
    }
}

/// A parameter the reference declares as a string of at least one character.
/// Whitespace counts, as it does there: a blank name reaches the service.
fn text<'a>(params: &'a BTreeMap<String, Value>, key: &str) -> Result<&'a str, Failure> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Failure::Invalid(format!("{key} must be a non-empty string")))
}

struct ProjectLinks<'a> {
    workspace: &'a WorkspaceService,
    store: ProjectsStore,
}

impl ProjectLinks<'_> {
    /// Every saved link, grouped by project in the order the store first
    /// names each. Local state only, so no credential is needed.
    async fn list(&self) -> Value {
        let mut groups: Vec<(String, Vec<PathBuf>)> = Vec::new();
        for link in self.store.list_project_links() {
            let path = link.path().to_path_buf();
            match groups
                .iter_mut()
                .find(|(project_id, _)| project_id == link.project_id())
            {
                Some((_, paths)) => paths.push(path),
                None => groups.push((link.project_id().to_owned(), vec![path])),
            }
        }
        let mut projects = Vec::with_capacity(groups.len());
        for (project_id, paths) in groups {
            let mut local_links = Vec::with_capacity(paths.len());
            for path in paths {
                local_links.push(json!({
                    "directoryPath": path.to_string_lossy(),
                    "hasCommits": has_commits(&path).await,
                }));
            }
            projects.push(json!({"projectId": project_id, "localLinks": local_links}));
        }
        json!({"projects": projects})
    }

    async fn resolve_root(&self, root_path: &str) -> Value {
        match inspect_directory(&expand_home(Path::new(root_path))).await {
            Ok(inspected) => json!({
                "eligible": true,
                "rejectReason": null,
                "root": inspected.view(),
            }),
            Err(_) => json!({
                "eligible": false,
                "rejectReason": "nested_unresolvable",
                "root": null,
            }),
        }
    }

    /// The root and its saved link. A checkout link whose remote no longer
    /// matches is dropped; a failed drop is reported in its own field rather
    /// than failing the call.
    async fn inspect_root(&self, root_path: &str) -> Value {
        let inspected = match inspect_directory(&expand_home(Path::new(root_path))).await {
            Ok(inspected) => inspected,
            Err(_) => {
                return json!({
                    "eligible": false,
                    "rejectReason": "nested_unresolvable",
                    "root": null,
                    "savedLink": null,
                    "staleLinkCleared": false,
                    "staleLinkClearFailed": false,
                });
            }
        };
        let mut saved_link = Value::Null;
        let mut cleared = false;
        let mut clear_failed = false;
        if let Some(link) = self.store.get_project_link(&inspected.path) {
            let github = inspected
                .git
                .as_ref()
                .and_then(|git| git.github_repo_url.as_deref());
            let matches = match &link {
                ProjectLink::Local { .. } => true,
                ProjectLink::Remote { repo_url, .. } => github.is_some_and(|github| {
                    normalize_repo_url(repo_url) == normalize_repo_url(github)
                }),
            };
            if matches {
                saved_link = saved_summary(&link);
            } else if self.store.delete_project_link(&inspected.path).is_ok() {
                cleared = true;
            } else {
                clear_failed = true;
            }
        }
        json!({
            "eligible": true,
            "rejectReason": null,
            "root": inspected.view(),
            "savedLink": saved_link,
            "staleLinkCleared": cleared,
            "staleLinkClearFailed": clear_failed,
        })
    }

    /// The first candidate page, with the saved checkout link reconciled
    /// against the current remote.
    async fn picker_load(&self, root_path: &str) -> Result<Value, Failure> {
        const METHOD: &str = "projectLinks/picker/load";
        let (repo_root, git) = eligible_root(root_path).await?;
        let service = self.service()?;
        let (projects, next_cursor) = service
            .page(None)
            .await
            .map_err(|message| api_failure(&message, METHOD))?;
        let mut saved_link = Value::Null;
        let mut saved_project_id = None;
        let mut cleared = false;
        if let Some(link) = self.store.get_remote_project(&repo_root)
            && let ProjectLink::Remote { repo_url, .. } = &link
        {
            if normalize_repo_url(repo_url) == normalize_repo_url(&git.remote_url) {
                saved_link = saved_summary(&link);
                saved_project_id = Some(link.project_id().to_owned());
            } else {
                self.store.delete_remote_project(&repo_root)?;
                cleared = true;
            }
        }
        Ok(json!({
            "root": checkout_view(&repo_root, &git),
            "savedLink": saved_link,
            "staleLinkCleared": cleared,
            "candidates": candidate_page(
                &projects,
                &git.remote_url,
                saved_project_id.as_deref(),
                next_cursor.as_deref(),
                true,
            ),
        }))
    }

    /// The pages after `cursor`, read until one shows a project this
    /// repository can use, which is focused. A page past the first knows no
    /// saved link, so nothing on it is recommended.
    async fn picker_load_more(&self, root_path: &str, cursor: &str) -> Result<Value, Failure> {
        const METHOD: &str = "projectLinks/picker/loadMore";
        let (_, git) = eligible_root(root_path).await?;
        let service = self.service()?;
        let mut projects = Vec::new();
        let mut next_cursor = Some(cursor.to_owned());
        let mut focus = None;
        let mut cursor = Some(cursor.to_owned());
        while let Some(current) = cursor.take() {
            let (page, page_cursor) = service
                .page(Some(&current))
                .await
                .map_err(|message| api_failure(&message, METHOD))?;
            next_cursor.clone_from(&page_cursor);
            focus = page
                .iter()
                .find(|project| !project.is_read_only && visible(project, &git.remote_url))
                .map(|project| project.project_id.clone());
            projects.extend(page);
            if focus.is_some() {
                break;
            }
            cursor = page_cursor;
        }
        Ok(json!({
            "candidates": candidate_page(
                &projects,
                &git.remote_url,
                None,
                next_cursor.as_deref(),
                false,
            ),
            "focusProjectId": focus,
        }))
    }

    /// Creates a project for the checkout's GitHub remote and links the
    /// checkout to it. The first page is read before anything is created, as
    /// the picker the reference builds for it does.
    async fn create(
        &self,
        root_path: &str,
        name: &str,
        default_branch: &str,
    ) -> Result<Value, Failure> {
        const METHOD: &str = "projectLinks/create";
        let (repo_root, git) = eligible_root(root_path).await?;
        let service = self.service()?;
        service
            .page(None)
            .await
            .map_err(|message| api_failure(&message, METHOD))?;
        let (name, default_branch) = (name.trim(), default_branch.trim());
        if name.is_empty() || default_branch.is_empty() {
            return Err(api_failure(
                "a project needs a name and a default branch",
                METHOD,
            ));
        }
        let project = async {
            service
                .client()?
                .create(name, &git.remote_url, default_branch)
                .await
        }
        .await
        .map_err(|message| api_failure(&message, METHOD))?;
        self.save_checkout_link(&repo_root, &git, &project)
    }

    /// Links the checkout to a project the caller names, once the project is
    /// known to exist, to be writable and to list this repository.
    async fn link(&self, root_path: &str, project_id: &str) -> Result<Value, Failure> {
        const METHOD: &str = "projectLinks/link";
        let (repo_root, git) = eligible_root(root_path).await?;
        let service = self.service()?;
        let mut projects = Vec::new();
        let mut cursor = None;
        loop {
            let (page, next) = service
                .page(cursor.as_deref())
                .await
                .map_err(|message| api_failure(&message, METHOD))?;
            projects.extend(page);
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let project = projects
            .into_iter()
            .find(|project| project.project_id == project_id)
            .ok_or_else(|| {
                Failure::Invalid(format!(
                    "No Vibe Code project has the identifier {project_id}."
                ))
            })?;
        if project.is_read_only || !is_project_linked_to_repo(&project, &git.remote_url) {
            return Err(Failure::Invalid(
                "That Vibe Code project cannot be linked to this repository.".to_owned(),
            ));
        }
        self.save_checkout_link(&repo_root, &git, &project)
    }

    /// Saves a link the caller already chose, without asking Vibe Code. A
    /// directory with no GitHub remote gets a directory link; a checkout
    /// with one gets a checkout link, provided its remote is still the one
    /// the caller prepared the link for.
    async fn save(
        &self,
        root_path: &str,
        project_id: &str,
        project_name: &str,
        expected: Option<&str>,
    ) -> Result<Value, Failure> {
        let inspected = inspect_directory(&expand_home(Path::new(root_path)))
            .await
            .map_err(|detail| Failure::Invalid(ineligible(&detail)))?;
        let expected = expected.map(str::trim);
        let github = inspected
            .git
            .as_ref()
            .and_then(|git| git.github_repo_url.clone());
        let link = match github {
            None if expected.is_some() => {
                return Err(Failure::Invalid(REMOTE_CHANGED.to_owned()));
            }
            None => ProjectLink::Local {
                directory_path: inspected.path.clone(),
                project_id: project_id.to_owned(),
                project_name: project_name.to_owned(),
            },
            Some(github) => {
                if expected.is_none_or(|expected| {
                    normalize_repo_url(&github) != normalize_repo_url(expected)
                }) {
                    return Err(Failure::Invalid(REMOTE_CHANGED.to_owned()));
                }
                ProjectLink::Remote {
                    repo_root: inspected.path.clone(),
                    repo_url: github,
                    project_id: project_id.to_owned(),
                    project_name: project_name.to_owned(),
                }
            }
        };
        self.store.upsert_project_link(&link)?;
        Ok(link_view(&link, &inspected.path))
    }

    /// Drops the link saved for the root. A root that no longer resolves, a
    /// checkout moved or deleted, drops the link saved for its closest
    /// ancestor instead, so a stranded link stays removable. A drop that does
    /// not land is not reported: the answer is always that it is unlinked.
    async fn unlink(&self, root_path: &str) -> Value {
        match inspect_directory(&expand_home(Path::new(root_path))).await {
            Ok(inspected) => {
                let _ = self.store.delete_project_link(&inspected.path);
            }
            Err(_) => {
                let requested = resolve_path(&expand_home(Path::new(root_path)));
                let target = self
                    .store
                    .list_project_links()
                    .into_iter()
                    .filter(|link| requested.starts_with(link.path()))
                    .fold(None::<ProjectLink>, |closest, link| match closest {
                        Some(closest)
                            if closest.path().components().count()
                                >= link.path().components().count() =>
                        {
                            Some(closest)
                        }
                        _ => Some(link),
                    });
                if let Some(target) = target {
                    let _ = self.store.delete_project_link(target.path());
                }
            }
        }
        json!({"unlinked": true})
    }

    /// The Vibe Code endpoint, read from the configuration as it stands now.
    fn service(&self) -> Result<Service, Failure> {
        let config = self
            .workspace
            .layered_config()
            .load()
            .map(|snapshot| snapshot.effective.clone())
            .unwrap_or_default();
        Service::from_config(&config, |variable| {
            self.workspace.resolve_credential(variable)
        })
        .ok_or_else(|| Failure::Unauthorized(NO_KEY.to_owned()))
    }

    fn save_checkout_link(
        &self,
        repo_root: &Path,
        git: &GitRepoInfo,
        project: &Project,
    ) -> Result<Value, Failure> {
        let link = ProjectLink::Remote {
            repo_root: repo_root.to_path_buf(),
            repo_url: git.remote_url.clone(),
            project_id: project.project_id.clone(),
            project_name: project.name.clone(),
        };
        self.store.upsert_project_link(&link)?;
        Ok(link_view(&link, repo_root))
    }
}

/// Reference `_api_error`: a Vibe Code failure that names the key or a 401
/// or 403 is the caller's to fix; any other is reported without repeating
/// what the service answered.
fn api_failure(message: &str, method: &str) -> Failure {
    let lowered = message.to_lowercase();
    if lowered.contains("api key")
        || lowered.contains("status 401")
        || lowered.contains("status 403")
    {
        Failure::Unauthorized(format!(
            "Vibe Code did not accept the credential ({method})."
        ))
    } else {
        Failure::Internal(format!("A request to Vibe Code failed ({method})."))
    }
}

fn ineligible(detail: &str) -> String {
    format!("This directory cannot be linked to a Vibe Code project: {detail}")
}

/// Reference `_resolve_root`: the checkout a Teleport run would read, and
/// the root its link is keyed on.
async fn eligible_root(root_path: &str) -> Result<(PathBuf, GitRepoInfo), Failure> {
    let path = expand_home(Path::new(root_path));
    let refuse = |message: String| Failure::Invalid(ineligible(&message));
    let repository = GitRepository::open(&path)
        .await
        .map_err(|failure| refuse(failure.message))?;
    let git = repository
        .metadata()
        .await
        .map_err(|failure| refuse(failure.message))?;
    let repo_root = git.repo_root.clone().unwrap_or_else(|| resolve_path(&path));
    Ok((repo_root, git))
}

/// What `resolveRoot` reports of a directory.
struct Inspected {
    path: PathBuf,
    git: Option<DirectoryGit>,
}

struct DirectoryGit {
    github_repo_url: Option<String>,
    current_branch: Option<String>,
    default_branch: Option<String>,
    has_commits: bool,
}

impl Inspected {
    /// A `ProjectLinksInspectedDirectory`.
    fn view(&self) -> Value {
        json!({
            "directoryPath": self.path.to_string_lossy(),
            "directoryName": directory_name(&self.path),
            "git": self.git.as_ref().map(|git| json!({
                "currentBranch": git.current_branch,
                "defaultBranch": git.default_branch,
                "githubRepoUrl": git.github_repo_url,
                "hasCommits": git.has_commits,
            })),
        })
    }
}

/// Reference `_read_inspected_directory`: an existing directory, described
/// by the checkout it sits in when it sits in one. A directory in no
/// repository, or in a bare one, is a plain directory; one that does not
/// exist, is not a directory or whose repository cannot be read is refused.
async fn inspect_directory(path: &Path) -> Result<Inspected, String> {
    let directory = std::fs::canonicalize(path)
        .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
    if !directory.is_dir() {
        return Err(format!("{} is not a directory", directory.display()));
    }
    let repository = match GitRepository::open(&directory).await {
        Ok(repository) => repository,
        Err(failure) if failure.class == FailureClass::NotSupported => {
            return Ok(Inspected {
                path: directory,
                git: None,
            });
        }
        Err(failure) => return Err(failure.message),
    };
    let Some(root) = repository.working_tree().map(Path::to_path_buf) else {
        return Ok(Inspected {
            path: directory,
            git: None,
        });
    };
    let base_root = root.clone();
    let default_branch =
        tokio::task::spawn_blocking(move || vibe_core::worktree::checkout_base_branch(&base_root))
            .await
            .ok()
            .flatten();
    let git = DirectoryGit {
        github_repo_url: repository.github_remote_url().await,
        current_branch: repository.branch().await,
        default_branch,
        has_commits: repository.has_commits().await,
    };
    Ok(Inspected {
        path: root,
        git: Some(git),
    })
}

/// Whether the checkout `path` sits in has a commit; `false` outside one.
async fn has_commits(path: &Path) -> bool {
    match GitRepository::open(path).await {
        Ok(repository) => repository.has_commits().await,
        Err(_) => false,
    }
}

/// Python's `Path.name`: the last component, empty for the root.
fn directory_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Reference `_inspection_from_git_info`: a checkout the picker resolved,
/// described from what the Teleport reading saw.
fn checkout_view(repo_root: &Path, git: &GitRepoInfo) -> Value {
    json!({
        "directoryPath": repo_root.to_string_lossy(),
        "directoryName": directory_name(repo_root),
        "git": {
            "currentBranch": git.branch,
            "defaultBranch": git.default_branch,
            "githubRepoUrl": git.remote_url,
            "hasCommits": true,
        },
    })
}

/// A `ProjectLinksSavedLink`.
fn saved_summary(link: &ProjectLink) -> Value {
    json!({"projectId": link.project_id(), "projectName": link.project_name()})
}

/// A `ProjectLinkMutationResponse`.
fn link_view(link: &ProjectLink, directory: &Path) -> Value {
    json!({
        "link": {
            "projectId": link.project_id(),
            "projectName": link.project_name(),
            "directoryPath": directory.to_string_lossy(),
        }
    })
}

/// Reference `_is_project_visible_in_picker`.
fn visible(project: &Project, repo_url: &str) -> bool {
    repo_url.is_empty() || is_project_linked_to_repo(project, repo_url)
}

/// A `ProjectLinksPickerCandidates` page (reference `_candidate_page` over
/// `rank_project_items`): the writable projects that list this repository,
/// the saved one first, then single-repository matches, then the rest, each
/// by name. The first is recommended unless the saved project is elsewhere.
fn candidate_page(
    projects: &[Project],
    repo_url: &str,
    saved_project_id: Option<&str>,
    next_cursor: Option<&str>,
    recommend: bool,
) -> Value {
    let rank = |project: &Project| -> u8 {
        if saved_project_id == Some(project.project_id.as_str()) {
            0
        } else if project.repositories.len() == 1 {
            1
        } else {
            2
        }
    };
    let mut ranked = projects
        .iter()
        .filter(|project| !project.is_read_only && is_project_linked_to_repo(project, repo_url))
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        rank(left)
            .cmp(&rank(right))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    let mut items = ranked
        .iter()
        .enumerate()
        .map(|(index, project)| {
            json!({
                "projectId": project.project_id,
                "name": project.name,
                "recommended": recommend && index == 0,
            })
        })
        .collect::<Vec<_>>();
    if let (Some(saved), Some(first)) = (saved_project_id, items.first_mut())
        && first["projectId"].as_str() != Some(saved)
    {
        first["recommended"] = json!(false);
    }
    json!({"items": items, "nextCursor": next_cursor})
}

#[cfg(test)]
mod links_tests;
