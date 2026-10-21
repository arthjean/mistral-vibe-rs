//! The project picker a session links its repository through (reference
//! `VibeCodeProjectPickerService`, `vibe/core/vibe_code_project/picker_service.py`,
//! driven by `VibeCodeController` in `vibe/app_server/_vibe_code.py`).
//!
//! A session holds one picker at a time, identified by a fresh identifier and
//! replaced by every `open`; every call on it is serialized by the
//! controller's picker lock.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Value, json};
use vibe_core::telemetry::TelemetryRecord;
use vibe_core::telemetry::records::{
    ProjectPicker, ProjectSelectionSource, RemoteProjectOutcome, TeleportContextSummaryStatus,
    TeleportFailed, TeleportFailureStage,
};

use super::git::{GitRepoInfo, GitRepository, normalize_repo_url};
use super::http::Project;
use super::teleport::require_teleport_available;
use super::{
    Answer, Host, Purpose, Refusal, SavedLink, Service, VibeCodeController,
    is_project_linked_to_repo, message_count,
};

/// Reference `ProjectPickerContext`.
#[derive(Debug, Clone)]
struct Context {
    repo_root: String,
    repo_url: String,
    repo_name: String,
    saved_link: Option<SavedLink>,
}

/// Reference `VibeCodeProjectPickerState`.
#[derive(Debug, Clone)]
struct State {
    projects: Vec<Project>,
    next_cursor: Option<String>,
    repo_url: String,
}

/// The one picker a session holds.
pub(super) struct Picker {
    id: String,
    purpose: Purpose,
    service: Service,
    git: GitRepoInfo,
    context: Context,
    state: State,
    selected: Option<String>,
    created: BTreeSet<String>,
    saved_link_cleared: bool,
    remote_changed: bool,
    /// What a Teleport run started from this picker reports about how its
    /// project was chosen (reference `_project_picker`).
    telemetry: Option<ProjectPicker>,
}

impl Picker {
    fn view(&self) -> Value {
        json!({
            "context": {
                "repoRoot": self.context.repo_root,
                "repoUrl": self.context.repo_url,
                "repoName": self.context.repo_name,
                "savedLink": self.context.saved_link.as_ref().map(link_view),
            },
            "state": {
                "projects": self.state.projects.iter().map(project_view).collect::<Vec<_>>(),
                "nextCursor": self.state.next_cursor,
                "repoUrl": self.state.repo_url,
            },
            "git": {
                "remoteName": self.git.remote_name,
                "remoteUrl": self.git.remote_url,
                "repo": self.git.repo,
                "branch": self.git.branch,
                "defaultBranch": self.git.default_branch,
            },
            "savedProjectLinkCleared": self.saved_link_cleared,
            "projectRepoRemoteChanged": self.remote_changed,
        })
    }

    /// Reference `build_project_picker_telemetry`, read at the moment of the
    /// decision.
    fn telemetry(&self, source: ProjectSelectionSource, shown: bool) -> ProjectPicker {
        ProjectPicker {
            shown,
            selection_source: Some(source),
            candidate_count_loaded: Some(self.state.projects.len() as u64),
            multi_repo_match_count: Some(multi_repo_matches(
                &self.state.projects,
                &self.context.repo_url,
            )),
            saved_project_link_cleared: Some(self.saved_link_cleared),
            repo_remote_changed: Some(self.remote_changed),
        }
    }

    /// Reference `reserve_teleport`'s checks on the picker: opened for
    /// Teleport, on the project it selected, with the payload that choice
    /// reports.
    pub(super) fn teleport_choice(&self, project_id: &str) -> Result<ProjectPicker, Refusal> {
        if self.purpose != Purpose::Teleport {
            return Err(Refusal::conflict(
                "The open project picker was not opened for Teleport.",
            ));
        }
        if self.selected.as_deref() != Some(project_id) {
            return Err(Refusal::conflict(
                "The project to teleport to is not the one the picker selected.",
            ));
        }
        self.telemetry
            .ok_or_else(|| Refusal::conflict("No project has been chosen for Teleport yet."))
    }
}

impl VibeCodeController {
    /// Reference `open`: the Teleport gate first, then the checkout and the
    /// first page, with a saved link resolved without listing when it still
    /// matches the remote.
    pub(crate) async fn open(
        &self,
        host: &Arc<dyn Host>,
        purpose: Purpose,
        prompt: Option<String>,
    ) -> Answer {
        host.require_idle()?;
        if purpose == Purpose::Teleport {
            require_teleport_available(host.as_ref(), prompt.as_deref()).await?;
        }
        let mut guard = self.picker.lock().await;
        *guard = None;
        let service = make_service(host.as_ref())?;
        let git = read_git(host.as_ref()).await?;
        let repo_root = git
            .repo_root
            .clone()
            .unwrap_or_else(|| host.cwd())
            .to_string_lossy()
            .into_owned();
        let mut context = Context {
            saved_link: host.saved_link(&repo_root),
            repo_root,
            repo_url: git.remote_url.clone(),
            repo_name: git.repo.clone(),
        };
        let matching_link = context.saved_link.as_ref().is_some_and(|link| {
            normalize_repo_url(&link.repo_url) == normalize_repo_url(&context.repo_url)
        });
        let (projects, next_cursor) = if purpose == Purpose::Teleport && matching_link {
            (Vec::new(), None)
        } else {
            service.page(None).await.map_err(Refusal::invalid)?
        };
        let mut resolved = None;
        let mut cleared = false;
        if purpose == Purpose::Teleport {
            if matching_link {
                resolved = context
                    .saved_link
                    .as_ref()
                    .map(|link| link.project_id.clone());
            } else if context.saved_link.is_some() {
                host.delete_link(&context.repo_root);
                context.saved_link = None;
                cleared = true;
            }
        }
        let mut picker = Picker {
            id: vibe_core::session_id::uuid_v4(),
            purpose,
            service,
            state: State {
                projects,
                next_cursor,
                repo_url: git.remote_url.clone(),
            },
            git,
            context,
            selected: resolved.clone(),
            created: BTreeSet::new(),
            saved_link_cleared: cleared,
            remote_changed: cleared,
            telemetry: None,
        };
        if resolved.is_some() {
            picker.telemetry = Some(picker.telemetry(ProjectSelectionSource::SavedLink, false));
        }
        let answer = json!({
            "pickerId": picker.id,
            "view": picker.view(),
            "resolvedProjectId": resolved,
        });
        *guard = Some(picker);
        Ok(answer)
    }

    /// Reference `load_more`: pages until one shows a project this
    /// repository can use, focusing it.
    pub(crate) async fn load_more(&self, host: &Arc<dyn Host>, picker_id: &str) -> Answer {
        host.require_idle()?;
        let mut guard = self.picker.lock().await;
        let picker = require_picker(&mut guard, Some(picker_id))?;
        let Some(mut cursor) = picker.state.next_cursor.clone() else {
            return Ok(json!({"view": picker.view(), "focusOptionId": null}));
        };
        let mut projects = picker.state.projects.clone();
        let mut next_cursor;
        let mut focus;
        loop {
            let (page, page_cursor) = picker
                .service
                .page(Some(&cursor))
                .await
                .map_err(Refusal::invalid)?;
            next_cursor = page_cursor.clone();
            let visible = page.iter().find(|project| {
                !project.is_read_only
                    && (picker.state.repo_url.is_empty()
                        || is_project_linked_to_repo(project, &picker.state.repo_url))
            });
            focus = visible.map(|project| format!("project:{}", project.project_id));
            projects.extend(page);
            match (&focus, page_cursor) {
                (None, Some(following)) => cursor = following,
                _ => break,
            }
        }
        picker.state.projects = projects;
        picker.state.next_cursor = next_cursor;
        Ok(json!({"view": picker.view(), "focusOptionId": focus}))
    }

    /// Reference `create`: a new project for this repository, listed first
    /// and neither selected nor linked.
    pub(crate) async fn create(
        &self,
        host: &Arc<dyn Host>,
        picker_id: &str,
        name: &str,
        default_branch: &str,
    ) -> Answer {
        host.require_idle()?;
        let mut guard = self.picker.lock().await;
        let picker = require_picker(&mut guard, Some(picker_id))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(Refusal::invalid("A project name is required."));
        }
        let default_branch = default_branch.trim();
        if default_branch.is_empty() {
            return Err(Refusal::invalid("A default branch is required."));
        }
        let project = picker
            .service
            .client()
            .map_err(Refusal::invalid)?
            .create(name, &picker.git.remote_url, default_branch)
            .await
            .map_err(Refusal::invalid)?;
        let mut projects = vec![project.clone()];
        projects.extend(
            picker
                .state
                .projects
                .iter()
                .filter(|existing| existing.project_id != project.project_id)
                .cloned(),
        );
        picker.state.projects = projects;
        picker.created.insert(project.project_id.clone());
        Ok(json!({"view": picker.view(), "project": project_view(&project)}))
    }

    /// Reference `select`: links the chosen project to the checkout.
    pub(crate) async fn select(
        &self,
        host: &Arc<dyn Host>,
        picker_id: &str,
        project_id: &str,
    ) -> Answer {
        host.require_idle()?;
        let mut guard = self.picker.lock().await;
        let picker = require_picker(&mut guard, Some(picker_id))?;
        let project = picker
            .state
            .projects
            .iter()
            .find(|candidate| candidate.project_id == project_id)
            .cloned()
            .ok_or_else(|| {
                Refusal::invalid(format!(
                    "Vibe Code project {project_id} is not in the list."
                ))
            })?;
        if project.is_read_only || !is_project_linked_to_repo(&project, &picker.context.repo_url) {
            return Err(Refusal::invalid(
                "That Vibe Code project cannot be used with this repository.",
            ));
        }
        let link = SavedLink {
            repo_root: picker.context.repo_root.clone(),
            repo_url: picker.context.repo_url.clone(),
            project_id: project.project_id.clone(),
            project_name: project.name.clone(),
        };
        host.save_link(&link).map_err(Refusal::invalid)?;
        picker.context.saved_link = Some(link);
        picker.selected = Some(project.project_id.clone());
        let created = picker.created.contains(&project.project_id);
        let source = if created {
            ProjectSelectionSource::CreatedProject
        } else {
            ProjectSelectionSource::SelectedExisting
        };
        let payload = picker.telemetry(source, true);
        if picker.purpose == Purpose::Teleport {
            picker.telemetry = Some(payload);
        } else {
            host.record(&TelemetryRecord::RemoteProjectConfigured {
                outcome: if created {
                    RemoteProjectOutcome::Created
                } else {
                    RemoteProjectOutcome::Configured
                },
                picker: payload,
            });
        }
        Ok(json!({"view": picker.view(), "project": project_view(&project)}))
    }

    /// Reference `unlink`.
    pub(crate) async fn unlink(&self, host: &Arc<dyn Host>, picker_id: &str) -> Answer {
        host.require_idle()?;
        let mut guard = self.picker.lock().await;
        let picker = require_picker(&mut guard, Some(picker_id))?;
        host.delete_link(&picker.context.repo_root);
        picker.saved_link_cleared = true;
        picker.selected = None;
        picker.context.saved_link = None;
        let payload = picker.telemetry(ProjectSelectionSource::SavedLink, true);
        if picker.purpose == Purpose::Teleport {
            picker_cancelled(host.as_ref(), payload);
            picker.telemetry = None;
        } else {
            host.record(&TelemetryRecord::RemoteProjectConfigured {
                outcome: RemoteProjectOutcome::Unlinked,
                picker: payload,
            });
        }
        Ok(json!({"view": picker.view()}))
    }

    /// Reference `cancel_picker`.
    pub(crate) async fn cancel(&self, host: &Arc<dyn Host>, picker_id: &str) -> Answer {
        host.require_idle()?;
        let mut guard = self.picker.lock().await;
        let picker = require_picker(&mut guard, Some(picker_id))?;
        let payload = picker.telemetry(ProjectSelectionSource::Cancelled, true);
        if picker.purpose == Purpose::Teleport {
            picker_cancelled(host.as_ref(), payload);
        } else {
            host.record(&TelemetryRecord::RemoteProjectConfigured {
                outcome: RemoteProjectOutcome::Cancelled,
                picker: payload,
            });
        }
        *guard = None;
        Ok(json!({}))
    }

    /// Reference `recover_stale_link`: the link goes, and the first page is
    /// listed again.
    pub(crate) async fn recover(&self, host: &Arc<dyn Host>, picker_id: &str) -> Answer {
        host.require_idle()?;
        let mut guard = self.picker.lock().await;
        let picker = require_picker(&mut guard, Some(picker_id))?;
        host.delete_link(&picker.context.repo_root);
        picker.saved_link_cleared = true;
        picker.selected = None;
        picker.telemetry = None;
        picker.context.saved_link = None;
        let Ok((projects, next_cursor)) = picker.service.page(None).await else {
            return Ok(json!({"recovered": false, "view": picker.view()}));
        };
        picker.state = State {
            projects,
            next_cursor,
            repo_url: picker.git.remote_url.clone(),
        };
        Ok(json!({"recovered": true, "view": picker.view()}))
    }
}

pub(super) fn require_picker<'a>(
    guard: &'a mut Option<Picker>,
    picker_id: Option<&str>,
) -> Result<&'a mut Picker, Refusal> {
    let picker = guard
        .as_mut()
        .ok_or_else(|| Refusal::invalid("No Vibe Code project picker is open."))?;
    if picker_id.is_some_and(|id| id != picker.id) {
        return Err(Refusal::conflict(
            "That Vibe Code project picker was replaced by a newer one.",
        ));
    }
    Ok(picker)
}

/// Reference `_make_service`.
fn make_service(host: &dyn Host) -> Result<Service, Refusal> {
    Service::from_config(&host.config(), |variable| host.credential(variable))
        .ok_or_else(|| Refusal::invalid("No Mistral API key is set."))
}

/// Reference `_read_git`.
async fn read_git(host: &dyn Host) -> Result<GitRepoInfo, Refusal> {
    let repository = GitRepository::open(&host.cwd())
        .await
        .map_err(|failure| Refusal::invalid(failure.message))?;
    repository
        .info()
        .await
        .map_err(|failure| Refusal::invalid(failure.message))
}

fn picker_cancelled(host: &dyn Host, payload: ProjectPicker) {
    host.record(&TelemetryRecord::TeleportFailed(TeleportFailed {
        stage: TeleportFailureStage::Cancelled,
        error_class: "TeleportProjectPickerCancelledError".to_owned(),
        push_required: false,
        nb_session_messages: message_count(&host.messages()),
        context_summary: TeleportContextSummaryStatus::Skipped,
        context_summary_chars: None,
        failure_kind: None,
        http_status_code: None,
        picker: Some(payload),
    }));
}

/// Reference `count_multi_repo_matches`.
fn multi_repo_matches(projects: &[Project], repo_url: &str) -> u64 {
    if repo_url.is_empty() {
        return 0;
    }
    projects
        .iter()
        .filter(|project| {
            project.repositories.len() > 1 && is_project_linked_to_repo(project, repo_url)
        })
        .count() as u64
}

fn project_view(project: &Project) -> Value {
    json!({
        "projectId": project.project_id,
        "name": project.name,
        "repositories": project.repositories.iter().map(|repository| json!({
            "repoUrl": repository.repo_url,
            "defaultBranch": repository.default_branch,
        })).collect::<Vec<_>>(),
        "isReadOnly": project.is_read_only,
    })
}

fn link_view(link: &SavedLink) -> Value {
    json!({
        "repoRoot": link.repo_root,
        "repoUrl": link.repo_url,
        "projectId": link.project_id,
        "projectName": link.project_name,
    })
}
