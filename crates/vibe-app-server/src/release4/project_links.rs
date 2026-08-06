//! The session-less `projectLinks/*` surface.
//!
//! Every method is keyed on an absolute `rootPath` the caller holds rather than
//! on a session, which is what lets a picker run before a session exists. The
//! state it reads and writes is the same saved-link store `vibeCode/projects/*`
//! already owns, so a link saved through either surface is the one the other
//! sees; the reference shares its store the same way.
//!
//! Responses carry `repoLocalPath` rather than a compact label because this is a
//! local boundary: the renderer derives the label, and only the absolute path
//! identifies the checkout.
//!
//! Failures are classified the way the reference classifies them: a root Git
//! cannot resolve is reported as an ineligible answer with a reason drawn from
//! the four reject values, a rejected credential is `unauthorized`, and a Vibe
//! Code call that fails for any other reason is `internal_error`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use crate::host::expand_home;

use super::{
    CloudError, MAX_HEADLESS_PROJECT_PAGES, Project, ProjectLinkRoot, ProjectPage,
    ProjectRootRejection, Release4Dispatch, Release4Error, Release4Service, SavedProjectLink,
    is_project_linked_to_repo, normalize_repo_url, required_string,
};

/// What a saved link looked like once the root it is keyed on was resolved.
struct Reconciliation {
    /// A `ProjectLinksSavedLink`, or null when none survived.
    summary: Value,
    /// A link naming another repository was dropped.
    cleared: bool,
    /// Dropping it did not land, which only a caller with a field for it sees.
    clear_failed: bool,
}

impl Release4Service {
    /// The eight methods that resolve a repository root or reach Vibe Code, and
    /// so run off the caller's loop. `projectLinks/list` is answered inline by
    /// [`Release4Service::project_links_list`].
    pub(super) async fn project_links_deferred(
        &self,
        method: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        match method {
            "projectLinks/resolveRoot" => self.project_links_resolve_root(params).await,
            "projectLinks/inspectRoot" => self.project_links_inspect_root(params).await,
            "projectLinks/picker/load" => self.project_links_picker_load(params).await,
            "projectLinks/picker/loadMore" => self.project_links_picker_load_more(params).await,
            "projectLinks/create" => self.project_links_create(params).await,
            "projectLinks/link" => self.project_links_link(params).await,
            "projectLinks/save" => self.project_links_save(params).await,
            "projectLinks/unlink" => self.project_links_unlink(params).await,
            _ => Err(Release4Error::MethodNotFound(method.to_owned())),
        }
    }

    /// Every saved link, grouped by the project it points at.
    ///
    /// Listing is local state, so it does not need a configured credential.
    pub(super) fn project_links_list(&self) -> Result<Release4Dispatch, Release4Error> {
        let state = self.lock_projects()?;
        let mut grouped: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (repo_root, link) in &state.linked_projects {
            grouped
                .entry(link.project_id.as_str())
                .or_default()
                .push(repo_root.as_str());
        }
        let projects = grouped
            .into_iter()
            .map(|(project_id, repo_local_paths)| {
                json!({"projectId": project_id, "repoLocalPaths": repo_local_paths})
            })
            .collect::<Vec<_>>();
        Ok(Release4Dispatch::result([("projects", json!(projects))]))
    }

    async fn project_links_resolve_root(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let root_path = required_string(params, "rootPath")?.to_owned();
        Ok(match self.resolve_link_root(&root_path).await? {
            Ok(root) => Release4Dispatch::result([
                ("eligible", json!(true)),
                ("rejectReason", Value::Null),
                ("root", resolved_root(&root)),
            ]),
            Err(rejection) => Release4Dispatch::result([
                ("eligible", json!(false)),
                ("rejectReason", reject_reason(rejection)),
                ("root", Value::Null),
            ]),
        })
    }

    async fn project_links_inspect_root(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let root_path = required_string(params, "rootPath")?.to_owned();
        let root = match self.resolve_link_root(&root_path).await? {
            Ok(root) => root,
            Err(rejection) => {
                return Ok(Release4Dispatch::result([
                    ("eligible", json!(false)),
                    ("rejectReason", reject_reason(rejection)),
                    ("root", Value::Null),
                    ("savedLink", Value::Null),
                    ("staleLinkCleared", json!(false)),
                    ("staleLinkClearFailed", json!(false)),
                ]));
            }
        };
        // This is the one method with a field for a failed clear, so it reports
        // the failure instead of failing the request, as the reference does.
        let reconciled = self.reconcile_saved_link(&root, true)?;
        Ok(Release4Dispatch::result([
            ("eligible", json!(true)),
            ("rejectReason", Value::Null),
            ("root", inspected_root(&root)),
            ("savedLink", reconciled.summary),
            ("staleLinkCleared", json!(reconciled.cleared)),
            ("staleLinkClearFailed", json!(reconciled.clear_failed)),
        ]))
    }

    async fn project_links_picker_load(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let root_path = required_string(params, "rootPath")?.to_owned();
        let root = self.eligible_link_root(&root_path).await?;
        let page = self.project_links_page(None).await?;
        let reconciled = self.reconcile_saved_link(&root, false)?;
        let saved_project_id = reconciled.summary["projectId"]
            .as_str()
            .map(ToOwned::to_owned);
        let candidates = candidate_page(
            &page.projects,
            &root.repo_url,
            saved_project_id.as_deref(),
            page.next_cursor.as_deref(),
            true,
        );
        Ok(Release4Dispatch::result([
            ("root", resolved_root(&root)),
            ("savedLink", reconciled.summary),
            ("staleLinkCleared", json!(reconciled.cleared)),
            ("candidates", candidates),
        ]))
    }

    async fn project_links_picker_load_more(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let root_path = required_string(params, "rootPath")?.to_owned();
        let requested_cursor = required_string(params, "cursor")?.to_owned();
        let root = self.eligible_link_root(&root_path).await?;
        // A page holding no selectable candidate would render as an empty list
        // with more to load, so paging continues until one appears or the
        // cursors run out, which is what the reference's picker does.
        let mut projects = Vec::new();
        let mut next_cursor = Some(requested_cursor.clone());
        let mut focus = None;
        let mut seen = BTreeSet::new();
        let mut cursor = Some(requested_cursor);
        while let Some(page_cursor) = cursor.take() {
            if seen.len() >= MAX_HEADLESS_PROJECT_PAGES || !seen.insert(page_cursor.clone()) {
                return Err(Release4Error::Conflict(
                    "Vibe Code project pagination did not terminate safely".to_owned(),
                ));
            }
            let page = self.project_links_page(Some(page_cursor)).await?;
            next_cursor.clone_from(&page.next_cursor);
            focus = page
                .projects
                .iter()
                .find(|project| {
                    !project.is_read_only && is_project_linked_to_repo(project, &root.repo_url)
                })
                .map(|project| project.project_id.clone());
            projects.extend(page.projects);
            if focus.is_some() {
                break;
            }
            cursor = page.next_cursor;
        }
        // A page past the first carries no saved-link context: the saved
        // project, if any, ranked on the first page, so nothing here is
        // recommended.
        let candidates = candidate_page(
            &projects,
            &root.repo_url,
            None,
            next_cursor.as_deref(),
            false,
        );
        Ok(Release4Dispatch::result([
            ("candidates", candidates),
            (
                "focusProjectId",
                focus.map_or(Value::Null, |project_id| json!(project_id)),
            ),
        ]))
    }

    async fn project_links_create(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let root_path = required_string(params, "rootPath")?.to_owned();
        let name = required_string(params, "name")?.trim().to_owned();
        let default_branch = required_string(params, "defaultBranch")?.trim().to_owned();
        let root = self.eligible_link_root(&root_path).await?;
        let project = self
            .project_create_cloud(name, root.repo_url.clone(), default_branch)
            .await
            .map_err(|error| self.classify_vibe_code(error))?;
        let link = self.upsert_link(&root, &project.project_id, &project.name)?;
        Ok(link_response(&root, &link))
    }

    async fn project_links_link(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let root_path = required_string(params, "rootPath")?.to_owned();
        let project_id = required_string(params, "projectId")?.to_owned();
        // The reference validates the target and then persists the project's
        // own name, so the name a stateless caller sends is required by the
        // model and never written.
        required_string(params, "projectName")?;
        let root = self.eligible_link_root(&root_path).await?;
        let page = self
            .project_list_all()
            .await
            .map_err(|error| self.classify_vibe_code(error))?;
        let project = page
            .projects
            .into_iter()
            .find(|project| project.project_id == project_id)
            .ok_or_else(|| {
                Release4Error::InvalidParams(format!("Unknown Vibe Code project: {project_id}"))
            })?;
        if project.is_read_only || !is_project_linked_to_repo(&project, &root.repo_url) {
            return Err(Release4Error::InvalidParams(
                "The selected Vibe Code project is not available for this repository".to_owned(),
            ));
        }
        let link = self.upsert_link(&root, &project.project_id, &project.name)?;
        Ok(link_response(&root, &link))
    }

    async fn project_links_save(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let root_path = required_string(params, "rootPath")?.to_owned();
        let project_id = required_string(params, "projectId")?.to_owned();
        let project_name = required_string(params, "projectName")?.to_owned();
        let expected_repo_url = required_string(params, "expectedRepoUrl")?
            .trim()
            .to_owned();
        let root = self.eligible_link_root(&root_path).await?;
        if normalize_repo_url(&root.repo_url) != normalize_repo_url(&expected_repo_url) {
            return Err(Release4Error::InvalidParams(
                "The repository remote changed before the link could be saved.".to_owned(),
            ));
        }
        let link = self.upsert_link(&root, &project_id, &project_name)?;
        Ok(link_response(&root, &link))
    }

    async fn project_links_unlink(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let root_path = required_string(params, "rootPath")?.to_owned();
        match self.resolve_link_root(&root_path).await? {
            Ok(root) => self.remove_link(&root.repo_local_path)?,
            // The checkout moved or was deleted, so Git resolves nothing.
            // Links are keyed on the resolved root, which may be an ancestor of
            // the path the caller chose, so the stranded link is still removable.
            Err(_) => self.remove_stale_link(&root_path)?,
        }
        Ok(Release4Dispatch::result([("unlinked", json!(true))]))
    }

    /// Resolves `root_path` off the runtime thread, since the probe shells out
    /// to Git. The inner result is the reference's eligible/ineligible answer.
    async fn resolve_link_root(
        &self,
        root_path: &str,
    ) -> Result<Result<ProjectLinkRoot, ProjectRootRejection>, Release4Error> {
        let git = self.git.clone();
        let path = expand_home(Path::new(root_path));
        tokio::task::spawn_blocking(move || git.resolve_project_root(&path))
            .await
            .map_err(|_| Release4Error::BackgroundTask)
    }

    /// The resolved root, or the rejection reported as an invalid request,
    /// which is how the reference answers a mutation aimed at an unusable root.
    async fn eligible_link_root(&self, root_path: &str) -> Result<ProjectLinkRoot, Release4Error> {
        self.resolve_link_root(root_path)
            .await?
            .map_err(|rejection| {
                Release4Error::InvalidParams(format!(
                    "`{root_path}` is not an eligible project root: {}",
                    rejection_word(rejection)
                ))
            })
    }

    /// One page of Vibe Code projects, with the failure classified for this
    /// surface.
    async fn project_links_page(
        &self,
        cursor: Option<String>,
    ) -> Result<ProjectPage, Release4Error> {
        self.project_list_cloud(cursor)
            .await
            .map_err(|error| self.classify_vibe_code(error))
    }

    /// Reports a rejected or absent credential as an authorization failure and
    /// every other Vibe Code failure as an internal one, which is the split the
    /// reference makes and what tells a client to sign in rather than retry.
    fn classify_vibe_code(&self, error: Release4Error) -> Release4Error {
        match error {
            Release4Error::Cloud(CloudError::Unauthorized(message)) => {
                Release4Error::Cloud(CloudError::Unauthorized(message))
            }
            Release4Error::Cloud(CloudError::Unavailable(message)) if !self.cloud_configured => {
                Release4Error::Cloud(CloudError::Unauthorized(message))
            }
            Release4Error::Cloud(error) => Release4Error::VibeCode(error.to_string()),
            other => other,
        }
    }

    /// Compares the saved link against the root's current remote, dropping one
    /// that names another repository.
    ///
    /// `tolerate_clear_failure` is for the single method that publishes the
    /// outcome; every other caller has no field for it and fails instead of
    /// answering as though the stale link were gone.
    fn reconcile_saved_link(
        &self,
        root: &ProjectLinkRoot,
        tolerate_clear_failure: bool,
    ) -> Result<Reconciliation, Release4Error> {
        let mut state = self.lock_projects()?;
        let Some(saved) = state.linked_projects.get(&root.repo_local_path).cloned() else {
            return Ok(Reconciliation {
                summary: Value::Null,
                cleared: false,
                clear_failed: false,
            });
        };
        if normalize_repo_url(&saved.repo_url) == normalize_repo_url(&root.repo_url) {
            return Ok(Reconciliation {
                summary: json!({
                    "projectId": saved.project_id,
                    "projectName": saved.project_name,
                }),
                cleared: false,
                clear_failed: false,
            });
        }
        let before = state.clone();
        state.linked_projects.remove(&root.repo_local_path);
        if let Err(error) = self.persist_project_links(&state.linked_projects) {
            *state = before;
            if !tolerate_clear_failure {
                return Err(error);
            }
            return Ok(Reconciliation {
                summary: Value::Null,
                cleared: false,
                clear_failed: true,
            });
        }
        Ok(Reconciliation {
            summary: Value::Null,
            cleared: true,
            clear_failed: false,
        })
    }

    fn upsert_link(
        &self,
        root: &ProjectLinkRoot,
        project_id: &str,
        project_name: &str,
    ) -> Result<SavedProjectLink, Release4Error> {
        let link = SavedProjectLink {
            repo_url: root.repo_url.clone(),
            project_id: project_id.to_owned(),
            project_name: project_name.to_owned(),
        };
        let mut state = self.lock_projects()?;
        let before = state.clone();
        state
            .linked_projects
            .insert(root.repo_local_path.clone(), link.clone());
        if let Err(error) = self.persist_project_links(&state.linked_projects) {
            *state = before;
            return Err(error);
        }
        Ok(link)
    }

    fn remove_link(&self, repo_root: &str) -> Result<(), Release4Error> {
        let mut state = self.lock_projects()?;
        let before = state.clone();
        if state.linked_projects.remove(repo_root).is_none() {
            return Ok(());
        }
        if let Err(error) = self.persist_project_links(&state.linked_projects) {
            *state = before;
            return Err(error);
        }
        Ok(())
    }

    /// Removes the link whose stored root contains the requested path, closest
    /// first, so a link saved against an ancestor of a nested directory is
    /// still removable once the checkout stops resolving.
    fn remove_stale_link(&self, root_path: &str) -> Result<(), Release4Error> {
        let requested = expand_home(Path::new(root_path));
        let requested = fs::canonicalize(&requested).unwrap_or(requested);
        let target = {
            let state = self.lock_projects()?;
            state
                .linked_projects
                .keys()
                .filter(|stored| {
                    let stored = Path::new(stored.as_str());
                    requested == stored || requested.starts_with(stored)
                })
                .max_by_key(|stored| Path::new(stored.as_str()).components().count())
                .cloned()
        };
        match target {
            Some(target) => self.remove_link(&target),
            None => Ok(()),
        }
    }
}

/// A `ProjectLinksResolvedRoot`: what a root looks like before a remote URL is
/// needed to compare a saved link against it.
fn resolved_root(root: &ProjectLinkRoot) -> Value {
    json!({
        "repoLocalPath": root.repo_local_path,
        "repoName": root.repo_name,
        "currentBranch": root.current_branch,
        "defaultBranch": root.default_branch,
    })
}

/// A `ProjectLinksInspectedRoot`, which adds the remote the saved link is
/// compared against.
fn inspected_root(root: &ProjectLinkRoot) -> Value {
    let mut value = resolved_root(root);
    if let Some(object) = value.as_object_mut() {
        object.insert("repoUrl".to_owned(), json!(root.repo_url));
    }
    value
}

fn link_response(root: &ProjectLinkRoot, link: &SavedProjectLink) -> Release4Dispatch {
    Release4Dispatch::result([(
        "link",
        json!({
            "projectId": link.project_id,
            "projectName": link.project_name,
            "repoLocalPath": root.repo_local_path,
        }),
    )])
}

fn reject_reason(rejection: ProjectRootRejection) -> Value {
    json!(rejection_word(rejection))
}

const fn rejection_word(rejection: ProjectRootRejection) -> &'static str {
    match rejection {
        ProjectRootRejection::NotGit => "not_git",
        ProjectRootRejection::UnsupportedRemote => "unsupported_remote",
        ProjectRootRejection::NestedUnresolvable => "nested_unresolvable",
        ProjectRootRejection::NoCommits => "no_commits",
    }
}

/// A `ProjectLinksPickerCandidates` page.
///
/// The ranking is the one the terminal picker uses, so a client recommends the
/// project the terminal would: the currently linked project first, then
/// single-repository matches, then multi-repository ones, each by name.
fn candidate_page(
    projects: &[Project],
    repo_url: &str,
    saved_project_id: Option<&str>,
    next_cursor: Option<&str>,
    recommend: bool,
) -> Value {
    let mut ranked = projects
        .iter()
        .filter(|project| !project.is_read_only && is_project_linked_to_repo(project, repo_url))
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        match_rank(left, saved_project_id)
            .cmp(&match_rank(right, saved_project_id))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    let mut items = ranked
        .iter()
        .enumerate()
        .map(|(index, project)| {
            json!({
                "projectId": project.project_id,
                "name": project.name,
                "matchKind": match_kind(project),
                "recommended": recommend && index == 0,
            })
        })
        .collect::<Vec<_>>();
    // Never recommend a candidate that contradicts the saved link: the saved
    // project ranks first when it is on this page, so a different top candidate
    // means the saved project is off-page or unselectable.
    if let (Some(saved), Some(first)) = (saved_project_id, items.first_mut())
        && first["projectId"].as_str() != Some(saved)
    {
        first["recommended"] = json!(false);
    }
    json!({"items": items, "nextCursor": next_cursor})
}

fn match_rank(project: &Project, saved_project_id: Option<&str>) -> u8 {
    if saved_project_id == Some(project.project_id.as_str()) {
        return 0;
    }
    u8::from(project.repositories.len() != 1) + 1
}

const fn match_kind(project: &Project) -> &'static str {
    if project.repositories.len() == 1 {
        "exact_repo"
    } else {
        "multi_repo"
    }
}

#[cfg(test)]
mod tests;
