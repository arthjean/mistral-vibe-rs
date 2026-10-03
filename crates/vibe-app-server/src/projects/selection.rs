//! The saved links every `projectLinks/*` call reads, and the project listing
//! they page through.

use super::*;

#[derive(Clone, Default)]
pub(super) struct ProjectState {
    pub(super) linked_projects: BTreeMap<String, SavedProjectLink>,
}

pub(super) const MAX_HEADLESS_PROJECT_PAGES: usize = 100;

const MAX_HEADLESS_PROJECTS: usize = PROJECT_PAGE_LIMIT * MAX_HEADLESS_PROJECT_PAGES;

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

    pub(super) fn lock_projects(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, ProjectState>, ProjectsServiceError> {
        self.projects
            .lock()
            .map_err(|_| ProjectsServiceError::StatePoisoned)
    }
}
