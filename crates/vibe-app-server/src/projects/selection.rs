//! Vibe Code projects: the picker a client walks, and the headless resolution
//! that skips it.
//!
//! A session runs against one cloud project. A client picks it interactively
//! through a picker this owns, and a headless caller resolves it from the
//! repository the session sits in. Both end at the same selection.

use super::*;

#[derive(Clone, Default)]
pub(super) struct ProjectState {
    pub(super) pickers: BTreeMap<String, ProjectPicker>,
    pub(super) linked_projects: BTreeMap<String, SavedProjectLink>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProjectPickerPurpose {
    Configure,
    Teleport,
}

#[derive(Clone)]
pub(super) struct ProjectPicker {
    pub(super) session_id: String,
    pub(super) repo_root: String,
    pub(super) repo_url: String,
    pub(super) remote_name: String,
    pub(super) branch: Option<String>,
    pub(super) projects: BTreeMap<String, Project>,
    pub(super) selected: Option<String>,
    pub(super) saved_link: Option<SavedProjectLink>,
    pub(super) saved_project_link_cleared: bool,
    pub(super) project_repo_remote_changed: bool,
    pub(super) next_cursor: Option<String>,
}

pub(super) fn picker<'a>(
    state: &'a ProjectState,
    picker_id: &str,
    session_id: &str,
) -> Result<&'a ProjectPicker, ProjectsServiceError> {
    let picker = state.pickers.get(picker_id).ok_or_else(|| {
        ProjectsServiceError::NotFound(format!("picker `{picker_id}` was not found"))
    })?;
    if picker.session_id != session_id {
        return Err(ProjectsServiceError::NotFound(format!(
            "picker `{picker_id}` is not owned by session `{session_id}`"
        )));
    }
    Ok(picker)
}

pub(super) fn picker_mut<'a>(
    state: &'a mut ProjectState,
    picker_id: &str,
    session_id: &str,
) -> Result<&'a mut ProjectPicker, ProjectsServiceError> {
    let picker = state.pickers.get_mut(picker_id).ok_or_else(|| {
        ProjectsServiceError::NotFound(format!("picker `{picker_id}` was not found"))
    })?;
    if picker.session_id != session_id {
        return Err(ProjectsServiceError::NotFound(format!(
            "picker `{picker_id}` is not owned by session `{session_id}`"
        )));
    }
    Ok(picker)
}

pub(super) fn project_view(picker: &ProjectPicker) -> Value {
    let selected = picker
        .selected
        .as_ref()
        .and_then(|id| picker.projects.get(id));
    let selected_repository = selected.and_then(|project| project.repositories.first());
    let repo_url = picker.repo_url.clone();
    let repo_name = Path::new(&picker.repo_root)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    json!({
        "context": {
            "repoRoot": picker.repo_root,
            "repoUrl": repo_url,
            "repoName": repo_name,
            "savedLink": picker.saved_link.as_ref().map(|link| json!({
                "repoRoot": picker.repo_root,
                "repoUrl": link.repo_url,
                "projectId": link.project_id,
                "projectName": link.project_name,
            })),
        },
        "state": {
            "projects": picker.projects.values().collect::<Vec<_>>(),
            "nextCursor": picker.next_cursor,
            "repoUrl": repo_url,
        },
        "git": {
            "remoteName": picker.remote_name,
            "remoteUrl": repo_url,
            "repo": repo_name,
            "branch": picker.branch,
            "defaultBranch": selected_repository.and_then(|repo| repo.default_branch.clone()),
        },
        "savedProjectLinkCleared": picker.saved_project_link_cleared,
        "projectRepoRemoteChanged": picker.project_repo_remote_changed,
    })
}

pub(super) fn finish_headless_project_open(
    opened: &mut ProjectsDispatch,
    action: ProjectsDispatch,
) -> Result<(), ProjectsServiceError> {
    let project_id = action
        .result
        .get("project")
        .and_then(|project| project.get("projectId"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ProjectsServiceError::Conflict(
                "headless project resolution omitted the resolved project".to_owned(),
            )
        })?;
    let view = action.result.get("view").cloned().ok_or_else(|| {
        ProjectsServiceError::Conflict(
            "headless project resolution omitted the project picker view".to_owned(),
        )
    })?;
    opened.result.insert("view".to_owned(), view);
    opened
        .result
        .insert("resolvedProjectId".to_owned(), json!(project_id));
    Ok(())
}

pub(super) fn headless_default_branch(
    branch: Option<String>,
) -> Result<String, ProjectsServiceError> {
    branch
        .map(|branch| branch.trim().to_owned())
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| {
            ProjectsServiceError::Cloud(CloudError::Git(
                "Teleport requires a checked-out branch before creating a Vibe Code project"
                    .to_owned(),
            ))
        })
}

pub(super) fn project_picker_purpose(
    params: &BTreeMap<String, Value>,
) -> Result<ProjectPickerPurpose, ProjectsServiceError> {
    match optional_string(params, "purpose")? {
        None | Some("configure") => Ok(ProjectPickerPurpose::Configure),
        Some("teleport") => Ok(ProjectPickerPurpose::Teleport),
        Some(purpose) => Err(ProjectsServiceError::InvalidParams(format!(
            "purpose must be `configure` or `teleport`, got `{purpose}`"
        ))),
    }
}

pub(super) const MAX_HEADLESS_PROJECT_PAGES: usize = 100;

pub(super) const MAX_HEADLESS_PROJECTS: usize = PROJECT_PAGE_LIMIT * MAX_HEADLESS_PROJECT_PAGES;

impl ProjectsService {
    pub(super) async fn project_list_cloud(
        &self,
        cursor: Option<String>,
    ) -> Result<ProjectPage, ProjectsServiceError> {
        match self.project_cloud.clone() {
            ProjectCloudBackend::Sync(cloud) => tokio::task::spawn_blocking(move || {
                cloud
                    .list(cursor.as_deref())
                    .map_err(ProjectsServiceError::Cloud)
            })
            .await
            .map_err(|_| ProjectsServiceError::BackgroundTask)?,
            ProjectCloudBackend::Async(cloud) => cloud
                .list(cursor.as_deref())
                .await
                .map_err(ProjectsServiceError::Cloud),
        }
    }

    pub(super) async fn project_list_all(&self) -> Result<ProjectPage, ProjectsServiceError> {
        let mut projects = Vec::new();
        let mut cursor = None;
        let mut seen_cursors = BTreeSet::new();
        let mut pages_loaded = 0_usize;
        loop {
            if pages_loaded >= MAX_HEADLESS_PROJECT_PAGES {
                return Err(ProjectsServiceError::Conflict(format!(
                    "Vibe Code project pagination exceeded {MAX_HEADLESS_PROJECT_PAGES} pages"
                )));
            }
            let page = self.project_list_cloud(cursor).await?;
            pages_loaded += 1;
            if projects.len().saturating_add(page.projects.len()) > MAX_HEADLESS_PROJECTS {
                return Err(ProjectsServiceError::Conflict(format!(
                    "Vibe Code project pagination exceeded {MAX_HEADLESS_PROJECTS} projects"
                )));
            }
            projects.extend(page.projects);
            let Some(next_cursor) = page.next_cursor else {
                return Ok(ProjectPage {
                    projects,
                    next_cursor: None,
                });
            };
            if !seen_cursors.insert(next_cursor.clone()) {
                return Err(ProjectsServiceError::Conflict(
                    "Vibe Code project pagination repeated a cursor".to_owned(),
                ));
            }
            cursor = Some(next_cursor);
        }
    }

    pub(super) async fn project_create_cloud(
        &self,
        name: String,
        repo_url: String,
        default_branch: String,
    ) -> Result<Project, ProjectsServiceError> {
        match self.project_cloud.clone() {
            ProjectCloudBackend::Sync(cloud) => tokio::task::spawn_blocking(move || {
                cloud
                    .create(&name, &repo_url, &default_branch)
                    .map_err(ProjectsServiceError::Cloud)
            })
            .await
            .map_err(|_| ProjectsServiceError::BackgroundTask)?,
            ProjectCloudBackend::Async(cloud) => cloud
                .create(&name, &repo_url, &default_branch)
                .await
                .map_err(ProjectsServiceError::Cloud),
        }
    }

    pub(super) fn install_project_picker(
        &self,
        session_id: String,
        git: ProjectGitSnapshot,
        page: ProjectPage,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        let picker_id = self.next_operation_id("picker");
        let mut projects = page
            .projects
            .into_iter()
            .map(|project| (project.project_id.clone(), project))
            .collect::<BTreeMap<_, _>>();
        let mut state = self.lock_projects()?;
        let before = state.clone();
        let mut saved_project_link_cleared = false;
        let mut project_repo_remote_changed = false;
        let saved_link = state.linked_projects.get(&git.repo_root).cloned();
        let saved_link = match saved_link {
            Some(link)
                if normalize_repo_url(&link.repo_url)
                    == normalize_repo_url(&git.snapshot.repository) =>
            {
                Some(link)
            }
            Some(_) => {
                state.linked_projects.remove(&git.repo_root);
                if let Err(error) = self.persist_project_links(&state.linked_projects) {
                    *state = before;
                    return Err(error);
                }
                saved_project_link_cleared = true;
                project_repo_remote_changed = true;
                None
            }
            None => None,
        };
        if let Some(link) = &saved_link {
            projects
                .entry(link.project_id.clone())
                .or_insert_with(|| Project {
                    project_id: link.project_id.clone(),
                    name: link.project_name.clone(),
                    repositories: vec![ProjectRepository {
                        repo_url: link.repo_url.clone(),
                        default_branch: None,
                    }],
                    is_read_only: false,
                });
        }
        let selected = saved_link.as_ref().map(|link| link.project_id.clone());
        let picker = ProjectPicker {
            session_id,
            repo_root: git.repo_root,
            repo_url: git.snapshot.repository,
            remote_name: git.remote_name,
            branch: git.branch,
            projects,
            selected: selected.clone(),
            saved_link,
            saved_project_link_cleared,
            project_repo_remote_changed,
            next_cursor: page.next_cursor,
        };
        let view = project_view(&picker);
        state.pickers.insert(picker_id.clone(), picker);
        Ok(ProjectsDispatch::result([
            ("pickerId", json!(picker_id)),
            ("view", view),
            ("resolvedProjectId", json!(selected)),
        ]))
    }

    pub(super) fn has_matching_saved_project_link(
        &self,
        git: &ProjectGitSnapshot,
    ) -> Result<bool, ProjectsServiceError> {
        let state = self.lock_projects()?;
        Ok(state
            .linked_projects
            .get(&git.repo_root)
            .is_some_and(|link| {
                normalize_repo_url(&link.repo_url) == normalize_repo_url(&git.snapshot.repository)
            }))
    }

    pub(super) async fn finish_headless_project_open(
        &self,
        session_id: &str,
        mut opened: ProjectsDispatch,
        project_name: String,
        default_branch: Option<String>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        if opened
            .result
            .get("resolvedProjectId")
            .is_some_and(|project_id| !project_id.is_null())
        {
            return Ok(opened);
        }
        let picker_id = opened
            .result
            .get("pickerId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProjectsServiceError::Conflict("project picker omitted its identifier".to_owned())
            })?
            .to_owned();
        let matched_project_id = self.single_headless_project_match(session_id, &picker_id)?;
        let action = if let Some(project_id) = matched_project_id {
            self.project_select(&BTreeMap::from([
                ("sessionId".to_owned(), json!(session_id)),
                ("pickerId".to_owned(), json!(picker_id)),
                ("projectId".to_owned(), json!(project_id)),
            ]))?
        } else {
            let default_branch = headless_default_branch(default_branch)?;
            self.project_create(&BTreeMap::from([
                ("sessionId".to_owned(), json!(session_id)),
                ("pickerId".to_owned(), json!(picker_id)),
                ("name".to_owned(), json!(project_name)),
                ("defaultBranch".to_owned(), json!(default_branch)),
            ]))
            .await?
        };
        finish_headless_project_open(&mut opened, action)?;
        Ok(opened)
    }

    pub(super) fn single_headless_project_match(
        &self,
        session_id: &str,
        picker_id: &str,
    ) -> Result<Option<String>, ProjectsServiceError> {
        let state = self.lock_projects()?;
        let picker = picker(&state, picker_id, session_id)?;
        let mut matches = picker.projects.values().filter(|project| {
            !project.is_read_only
                && project.repositories.len() == 1
                && is_project_linked_to_repo(project, &picker.repo_url)
        });
        let matched = matches.next().map(|project| project.project_id.clone());
        Ok(matched.filter(|_| matches.next().is_none()))
    }

    pub(super) async fn project_open(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        let session_id = required_string(params, "sessionId")?.to_owned();
        let purpose = project_picker_purpose(params)?;
        let working_directory = optional_string(params, "workingDirectory")?
            .unwrap_or(".")
            .to_owned();
        let git_working_directory = PathBuf::from(&working_directory);
        let git = self.git.clone();
        let git_task =
            tokio::task::spawn_blocking(move || git.inspect_project(&git_working_directory));
        let git = git_task
            .await
            .map_err(|_| ProjectsServiceError::BackgroundTask)?
            .map_err(ProjectsServiceError::Cloud)?;
        if purpose == ProjectPickerPurpose::Configure {
            let page = self.project_list_cloud(None).await?;
            return self.install_project_picker(session_id, git, page);
        }
        let project_name = suggested_project_name(&git);
        let default_branch = git.branch.clone();
        let page = if self.has_matching_saved_project_link(&git)? {
            ProjectPage {
                projects: Vec::new(),
                next_cursor: None,
            }
        } else {
            self.project_list_all().await?
        };
        let opened = self.install_project_picker(session_id.clone(), git, page)?;
        self.finish_headless_project_open(&session_id, opened, project_name, default_branch)
            .await
    }

    pub(super) async fn project_create(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        let session_id = required_string(params, "sessionId")?.to_owned();
        let picker_id = required_string(params, "pickerId")?.to_owned();
        let name = required_string(params, "name")?.to_owned();
        let default_branch = required_string(params, "defaultBranch")?.to_owned();
        let repo_url = {
            let state = self.lock_projects()?;
            picker(&state, &picker_id, &session_id)?.repo_url.clone()
        };
        let project = self
            .project_create_cloud(name, repo_url.clone(), default_branch)
            .await?;
        let mut state = self.lock_projects()?;
        let before = state.clone();
        let (repo_root, link, view) = {
            let picker = picker_mut(&mut state, &picker_id, &session_id)?;
            if picker.repo_url != repo_url {
                return Err(ProjectsServiceError::Conflict(
                    "project picker changed while creating the project".to_owned(),
                ));
            }
            picker
                .projects
                .insert(project.project_id.clone(), project.clone());
            picker.selected = Some(project.project_id.clone());
            let link = SavedProjectLink {
                repo_url: picker.repo_url.clone(),
                project_id: project.project_id.clone(),
                project_name: project.name.clone(),
            };
            picker.saved_link = Some(link.clone());
            picker.saved_project_link_cleared = false;
            picker.project_repo_remote_changed = false;
            (picker.repo_root.clone(), link, project_view(picker))
        };
        state.linked_projects.insert(repo_root, link);
        if let Err(error) = self.persist_project_links(&state.linked_projects) {
            *state = before;
            return Err(error);
        }
        Ok(ProjectsDispatch::result([
            ("view", view),
            ("project", serde_json::to_value(project)?),
        ]))
    }

    pub(super) async fn project_load_more(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        let session_id = required_string(params, "sessionId")?.to_owned();
        let picker_id = required_string(params, "pickerId")?.to_owned();
        let requested_cursor = {
            let state = self.lock_projects()?;
            picker(&state, &picker_id, &session_id)?.next_cursor.clone()
        };
        let Some(requested_cursor) = requested_cursor else {
            let state = self.lock_projects()?;
            let picker = picker(&state, &picker_id, &session_id)?;
            return Ok(ProjectsDispatch::result([
                ("view", project_view(picker)),
                ("focusOptionId", Value::Null),
            ]));
        };
        let repo_url = {
            let state = self.lock_projects()?;
            picker(&state, &picker_id, &session_id)?.repo_url.clone()
        };
        let mut cursor = Some(requested_cursor.clone());
        let mut pages = Vec::new();
        let mut focus = None;
        let mut seen = BTreeSet::new();
        while let Some(page_cursor) = cursor.take() {
            if pages.len() >= MAX_HEADLESS_PROJECT_PAGES || !seen.insert(page_cursor.clone()) {
                return Err(ProjectsServiceError::Conflict(
                    "Vibe Code project pagination did not terminate safely".to_owned(),
                ));
            }
            let page = self.project_list_cloud(Some(page_cursor)).await?;
            focus = page
                .projects
                .iter()
                .find(|project| project_is_selectable(project, &repo_url))
                .map(|project| project.project_id.clone());
            cursor.clone_from(&page.next_cursor);
            pages.push(page);
            if focus.is_some() {
                break;
            }
        }
        let mut state = self.lock_projects()?;
        let picker = picker_mut(&mut state, &picker_id, &session_id)?;
        if picker.next_cursor.as_deref() != Some(&requested_cursor) {
            return Err(ProjectsServiceError::Conflict(
                "project picker changed while loading the next page".to_owned(),
            ));
        }
        for page in pages {
            for project in page.projects {
                picker.projects.insert(project.project_id.clone(), project);
            }
            picker.next_cursor = page.next_cursor;
        }
        Ok(ProjectsDispatch::result([
            ("view", project_view(picker)),
            (
                "focusOptionId",
                focus.map_or(Value::Null, |id| json!(format!("project:{id}"))),
            ),
        ]))
    }

    pub(super) fn project_recover(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        let session_id = required_string(params, "sessionId")?;
        let picker_id = required_string(params, "pickerId")?;
        let mut state = self.lock_projects()?;
        let before = state.clone();
        let (repo_root, repo_url) = {
            let picker = picker(&state, picker_id, session_id)?;
            (picker.repo_root.clone(), picker.repo_url.clone())
        };
        let linked = state.linked_projects.get(&repo_root).cloned();
        let (saved_link, cleared, remote_changed) = match linked {
            Some(link) if normalize_repo_url(&link.repo_url) == normalize_repo_url(&repo_url) => {
                (Some(link), false, false)
            }
            Some(_) => {
                state.linked_projects.remove(&repo_root);
                if let Err(error) = self.persist_project_links(&state.linked_projects) {
                    *state = before;
                    return Err(error);
                }
                (None, true, true)
            }
            None => (None, false, false),
        };
        let picker = picker_mut(&mut state, picker_id, session_id)?;
        if let Some(link) = &saved_link {
            picker
                .projects
                .entry(link.project_id.clone())
                .or_insert_with(|| Project {
                    project_id: link.project_id.clone(),
                    name: link.project_name.clone(),
                    repositories: vec![ProjectRepository {
                        repo_url: link.repo_url.clone(),
                        default_branch: None,
                    }],
                    is_read_only: false,
                });
        }
        let selected = saved_link.as_ref().map(|link| link.project_id.clone());
        picker.selected.clone_from(&selected);
        picker.saved_link = saved_link;
        picker.saved_project_link_cleared = cleared;
        picker.project_repo_remote_changed = remote_changed;
        Ok(ProjectsDispatch::result([
            ("recovered", json!(selected.is_some())),
            ("view", project_view(picker)),
        ]))
    }

    pub(super) fn project_select(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        let session_id = required_string(params, "sessionId")?;
        let picker_id = required_string(params, "pickerId")?;
        let project_id = required_string(params, "projectId")?;
        let mut state = self.lock_projects()?;
        let before = state.clone();
        let (project, repo_root, link, view) = {
            let picker = picker_mut(&mut state, picker_id, session_id)?;
            let project = picker.projects.get(project_id).cloned().ok_or_else(|| {
                ProjectsServiceError::NotFound(format!(
                    "project `{project_id}` is not available in picker `{picker_id}`"
                ))
            })?;
            if project.is_read_only {
                return Err(ProjectsServiceError::InvalidParams(format!(
                    "project `{project_id}` is read-only and cannot be selected"
                )));
            }
            if !is_project_linked_to_repo(&project, &picker.repo_url) {
                return Err(ProjectsServiceError::InvalidParams(format!(
                    "project `{project_id}` is not linked to the current Git repository"
                )));
            }
            picker.selected = Some(project_id.to_owned());
            let link = SavedProjectLink {
                repo_url: picker.repo_url.clone(),
                project_id: project_id.to_owned(),
                project_name: project.name.clone(),
            };
            picker.saved_link = Some(link.clone());
            picker.saved_project_link_cleared = false;
            picker.project_repo_remote_changed = false;
            (
                project,
                picker.repo_root.clone(),
                link,
                project_view(picker),
            )
        };
        state.linked_projects.insert(repo_root, link);
        if let Err(error) = self.persist_project_links(&state.linked_projects) {
            *state = before;
            return Err(error);
        }
        Ok(ProjectsDispatch::result([
            ("view", view),
            ("project", serde_json::to_value(project)?),
        ]))
    }

    pub(super) fn project_unlink(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        let session_id = required_string(params, "sessionId")?;
        let picker_id = required_string(params, "pickerId")?;
        let mut state = self.lock_projects()?;
        let before = state.clone();
        let (repo_root, view) = {
            let picker = picker_mut(&mut state, picker_id, session_id)?;
            picker.selected = None;
            picker.saved_link = None;
            picker.saved_project_link_cleared = true;
            picker.project_repo_remote_changed = false;
            (picker.repo_root.clone(), project_view(picker))
        };
        state.linked_projects.remove(&repo_root);
        if let Err(error) = self.persist_project_links(&state.linked_projects) {
            *state = before;
            return Err(error);
        }
        Ok(ProjectsDispatch::result([("view", view)]))
    }

    pub(super) fn project_cancel(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        let session_id = required_string(params, "sessionId")?;
        let picker_id = required_string(params, "pickerId")?;
        let mut state = self.lock_projects()?;
        let current = state.pickers.get(picker_id).ok_or_else(|| {
            ProjectsServiceError::NotFound(format!("picker `{picker_id}` was not found"))
        })?;
        if current.session_id != session_id {
            return Err(ProjectsServiceError::NotFound(format!(
                "picker `{picker_id}` is not owned by session `{session_id}`"
            )));
        }
        state.pickers.remove(picker_id);
        Ok(ProjectsDispatch::result([] as [(&str, Value); 0]))
    }

    pub(super) fn lock_projects(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, ProjectState>, ProjectsServiceError> {
        self.projects
            .lock()
            .map_err(|_| ProjectsServiceError::StatePoisoned)
    }
}
