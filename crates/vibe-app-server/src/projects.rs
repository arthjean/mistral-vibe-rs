//! The Vibe Code project a session runs against, and everything that binds one
//! to a local repository.
//!
//! [`cloud`] is the backend contract and the HTTP client that satisfies it,
//! [`git`] the working tree a link and a teleport are decided from,
//! [`selection`] the project a session runs against, [`teleport`] handing a
//! session to the cloud, and [`links`] the saved association between a
//! repository root and a project. [`ProjectsService`] holds the state they
//! share and routes to them.
//!
//! [`loops`] is the exception, and stays here for one reason: the wire routes
//! `loops/*` to this same service, which owns their store and their schedule.
//! A scheduled loop touches a session only through the identifier a fire is
//! attributed to, and knows nothing about a project.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::host;
use crate::params::{self, optional_string, optional_u64, required_bool, required_string};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

mod cloud;
mod git;
mod loops;
mod selection;
mod teleport;

use selection::{MAX_HEADLESS_PROJECT_PAGES, ProjectPicker, ProjectState};
use teleport::TeleportOperation;
mod links;

pub use cloud::{
    AsyncProjectCloud, AsyncTeleportCloud, CloudConfigError, CloudError, CloudFuture,
    DEFAULT_VIBE_CODE_BASE_URL, Project, ProjectCloud, ProjectPage, ProjectRepository,
    TeleportCloud, TeleportFuture, TeleportRepository, TeleportRepositoryDiff,
    TeleportStartFailure, TeleportStartRequest, VibeCodeCloudConfig,
};
pub use git::{
    CommandGitProbe, GitProbe, GitPushStatus, GitSnapshot, ProjectGitSnapshot, ProjectLinkRoot,
    ProjectRootRejection,
};
pub use loops::{LoopFire, LoopState, ScheduledLoop};
use loops::{default_loop_store, load_loops, next_loop_sequence};

use cloud::{
    PROJECT_PAGE_LIMIT, ProjectCloudBackend, TeleportCloudBackend, UnavailableProjectCloud,
    UnavailableTeleportCloud, VibeCodeHttpCloud, validate_cloud_text,
};
use git::{
    UnavailableGitProbe, is_project_linked_to_repo, normalize_repo_url, project_is_selectable,
    suggested_project_name,
};

pub const PROJECTS_METHODS: &[&str] = &[
    "loops/clear",
    "loops/create",
    "loops/delete",
    "loops/list",
    "projectLinks/create",
    "projectLinks/inspectRoot",
    "projectLinks/link",
    "projectLinks/list",
    "projectLinks/picker/load",
    "projectLinks/picker/loadMore",
    "projectLinks/resolveRoot",
    "projectLinks/save",
    "projectLinks/unlink",
    "vibeCode/projects/cancel",
    "vibeCode/projects/create",
    "vibeCode/projects/loadMore",
    "vibeCode/projects/open",
    "vibeCode/projects/recover",
    "vibeCode/projects/select",
    "vibeCode/projects/unlink",
    "vibeCode/teleport/cancel",
    "vibeCode/teleport/push/respond",
    "vibeCode/teleport/start",
];

/// Methods that reach Vibe Code over the network. They are always dispatched on
/// the asynchronous path so a slow cloud call never blocks the caller's loop.
const DEFERRED_PROJECTS_METHODS: &[&str] = &[
    "projectLinks/create",
    "projectLinks/inspectRoot",
    "projectLinks/link",
    "projectLinks/picker/load",
    "projectLinks/picker/loadMore",
    "projectLinks/resolveRoot",
    "projectLinks/save",
    "projectLinks/unlink",
    "vibeCode/projects/create",
    "vibeCode/projects/loadMore",
    "vibeCode/projects/open",
    "vibeCode/teleport/push/respond",
    "vibeCode/teleport/start",
];
static NEXT_LINK_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectsNotification {
    pub method: String,
    pub params: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectsDispatch {
    pub result: BTreeMap<String, Value>,
    pub notifications: Vec<ProjectsNotification>,
}

impl ProjectsDispatch {
    fn result(entries: impl IntoIterator<Item = (impl Into<String>, Value)>) -> Self {
        Self {
            result: entries
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
            notifications: Vec::new(),
        }
    }

    fn with_notifications(
        entries: impl IntoIterator<Item = (impl Into<String>, Value)>,
        notifications: Vec<ProjectsNotification>,
    ) -> Self {
        Self {
            result: entries
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
            notifications,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedProjectLink {
    repo_url: String,
    project_id: String,
    project_name: String,
}

pub struct ProjectsSessionRemoval {
    session_id: String,
    pickers: BTreeMap<String, ProjectPicker>,
    teleports: BTreeMap<String, TeleportOperation>,
    loops: BTreeMap<String, ScheduledLoop>,
}

impl ProjectsSessionRemoval {
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn removed_loop_count(&self) -> usize {
        self.loops.len()
    }
}

#[derive(Clone)]
pub struct ProjectsService {
    projects: Arc<Mutex<ProjectState>>,
    teleports: Arc<Mutex<BTreeMap<String, TeleportOperation>>>,
    loops: Arc<Mutex<BTreeMap<String, ScheduledLoop>>>,
    project_link_store: Option<PathBuf>,
    project_link_store_error: Option<String>,
    loop_store: PathBuf,
    loop_store_error: Option<String>,
    project_cloud: ProjectCloudBackend,
    teleport_cloud: TeleportCloudBackend,
    git: Arc<dyn GitProbe>,
    /// Whether a Vibe Code backend is attached at all.
    ///
    /// The session-less project surface reports an absent one as an
    /// authorization failure, which is how the reference classifies a missing
    /// API key; a backend that is attached but failing is an internal error.
    cloud_configured: bool,
    next_operation: Arc<AtomicU64>,
    next_loop: Arc<AtomicU64>,
}

impl Default for ProjectsService {
    fn default() -> Self {
        let loop_store = default_loop_store();
        let (loops, loop_store_error) = match load_loops(&loop_store) {
            Ok(mut loops) => {
                for scheduled in loops.values_mut() {
                    if scheduled.state == LoopState::Running {
                        scheduled.state = LoopState::Scheduled;
                    }
                }
                (loops, None)
            }
            Err(error) => (BTreeMap::new(), Some(error.to_string())),
        };
        let next_loop = next_loop_sequence(&loops);
        Self {
            projects: Arc::new(Mutex::new(ProjectState::default())),
            teleports: Arc::new(Mutex::new(BTreeMap::new())),
            loops: Arc::new(Mutex::new(loops)),
            project_link_store: None,
            project_link_store_error: None,
            loop_store,
            loop_store_error,
            project_cloud: ProjectCloudBackend::Sync(Arc::new(UnavailableProjectCloud)),
            teleport_cloud: TeleportCloudBackend::Sync(Arc::new(UnavailableTeleportCloud)),
            git: Arc::new(UnavailableGitProbe),
            cloud_configured: false,
            next_operation: Arc::new(AtomicU64::new(1)),
            next_loop: Arc::new(AtomicU64::new(next_loop)),
        }
    }
}

impl ProjectsService {
    pub fn production(config: VibeCodeCloudConfig) -> Result<Self, ProjectsBuildError> {
        let cloud = Arc::new(VibeCodeHttpCloud::new(config)?);
        let service = Self {
            project_cloud: ProjectCloudBackend::Async(cloud.clone()),
            teleport_cloud: TeleportCloudBackend::Async(cloud),
            git: Arc::new(CommandGitProbe::default()),
            cloud_configured: true,
            ..Self::default()
        };
        service
            .with_project_link_store(default_project_link_store())
            .map_err(ProjectsBuildError::Service)
    }

    pub fn close_transient_session(&self, session_id: &str) -> Result<(), ProjectsServiceError> {
        self.lock_projects()?
            .pickers
            .retain(|_, picker| picker.session_id != session_id);
        self.lock_teleports()?
            .retain(|_, operation| operation.session_id != session_id);
        Ok(())
    }

    pub fn remove_session(&self, session_id: &str) -> Result<usize, ProjectsServiceError> {
        let removal = self.remove_session_transactional(session_id)?;
        Ok(removal.removed_loop_count())
    }

    pub fn remove_session_transactional(
        &self,
        session_id: &str,
    ) -> Result<ProjectsSessionRemoval, ProjectsServiceError> {
        self.ensure_loop_store_ready()?;
        let mut projects = self.lock_projects()?;
        let mut teleports = self.lock_teleports()?;
        let mut loops = self.lock_loops()?;
        let projects_before = projects.clone();
        let teleports_before = teleports.clone();
        let loops_before = loops.clone();

        let picker_ids = projects
            .pickers
            .iter()
            .filter(|(_, picker)| picker.session_id == session_id)
            .map(|(picker_id, _)| picker_id.clone())
            .collect::<Vec<_>>();
        let removed_pickers = picker_ids
            .into_iter()
            .filter_map(|picker_id| projects.pickers.remove_entry(&picker_id))
            .collect();
        let teleport_ids = teleports
            .iter()
            .filter(|(_, operation)| operation.session_id == session_id)
            .map(|(operation_id, _)| operation_id.clone())
            .collect::<Vec<_>>();
        let removed_teleports = teleport_ids
            .into_iter()
            .filter_map(|operation_id| teleports.remove_entry(&operation_id))
            .collect();
        let loop_ids = loops
            .iter()
            .filter(|(_, scheduled)| scheduled.session_id == session_id)
            .map(|(loop_id, _)| loop_id.clone())
            .collect::<Vec<_>>();
        let removed_loops = loop_ids
            .into_iter()
            .filter_map(|loop_id| loops.remove_entry(&loop_id))
            .collect();
        if let Err(error) = self.persist_loops(&loops) {
            *projects = projects_before;
            *teleports = teleports_before;
            *loops = loops_before;
            return Err(error);
        }
        Ok(ProjectsSessionRemoval {
            session_id: session_id.to_owned(),
            pickers: removed_pickers,
            teleports: removed_teleports,
            loops: removed_loops,
        })
    }

    pub fn restore_session(
        &self,
        removal: &ProjectsSessionRemoval,
    ) -> Result<(), ProjectsServiceError> {
        self.ensure_loop_store_ready()?;
        let mut projects = self.lock_projects()?;
        let mut teleports = self.lock_teleports()?;
        let mut loops = self.lock_loops()?;
        if removal
            .pickers
            .keys()
            .any(|picker_id| projects.pickers.contains_key(picker_id))
            || removal
                .teleports
                .keys()
                .any(|operation_id| teleports.contains_key(operation_id))
            || removal
                .loops
                .keys()
                .any(|loop_id| loops.contains_key(loop_id))
        {
            return Err(ProjectsServiceError::Conflict(
                "projects session rollback collides with newer session state".to_owned(),
            ));
        }
        let projects_before = projects.clone();
        let teleports_before = teleports.clone();
        let loops_before = loops.clone();

        projects.pickers.extend(removal.pickers.clone());
        teleports.extend(removal.teleports.clone());
        loops.extend(removal.loops.clone());
        if let Err(error) = self.persist_loops(&loops) {
            *projects = projects_before;
            *teleports = teleports_before;
            *loops = loops_before;
            return Err(error);
        }
        Ok(())
    }

    pub fn rebind_session(
        &self,
        old_session_id: &str,
        new_session_id: &str,
    ) -> Result<(), ProjectsServiceError> {
        self.ensure_loop_store_ready()?;
        if old_session_id == new_session_id {
            return Ok(());
        }
        let mut projects = self.lock_projects()?;
        let mut teleports = self.lock_teleports()?;
        let mut loops = self.lock_loops()?;
        let projects_before = projects.clone();
        let teleports_before = teleports.clone();
        let loops_before = loops.clone();

        for picker in projects.pickers.values_mut() {
            if picker.session_id == old_session_id {
                picker.session_id = new_session_id.to_owned();
            }
        }
        for operation in teleports.values_mut() {
            if operation.session_id == old_session_id {
                operation.session_id = new_session_id.to_owned();
            }
        }
        for scheduled in loops.values_mut() {
            if scheduled.session_id == old_session_id {
                scheduled.session_id = new_session_id.to_owned();
            }
        }
        if let Err(error) = self.persist_loops(&loops) {
            *projects = projects_before;
            *teleports = teleports_before;
            *loops = loops_before;
            return Err(error);
        }
        Ok(())
    }

    #[must_use]
    pub fn with_backends(
        project_cloud: Arc<dyn ProjectCloud>,
        teleport_cloud: Arc<dyn TeleportCloud>,
        git: Arc<dyn GitProbe>,
    ) -> Self {
        Self {
            project_cloud: ProjectCloudBackend::Sync(project_cloud),
            teleport_cloud: TeleportCloudBackend::Sync(teleport_cloud),
            git,
            cloud_configured: true,
            ..Self::default()
        }
    }

    pub fn with_project_link_store(mut self, path: PathBuf) -> Result<Self, ProjectsServiceError> {
        let linked_projects = load_project_links(&path)?;
        self.projects = Arc::new(Mutex::new(ProjectState {
            pickers: BTreeMap::new(),
            linked_projects,
        }));
        self.project_link_store = Some(path);
        self.project_link_store_error = None;
        Ok(self)
    }

    /// Dispatches the methods that only touch local state.
    ///
    /// Cloud-backed methods reach the network and are served by
    /// [`Self::dispatch_deferred`]; calling them here is a routing mistake.
    pub fn dispatch(
        &self,
        method: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        if DEFERRED_PROJECTS_METHODS.contains(&method) {
            return Err(ProjectsServiceError::Conflict(format!(
                "`{method}` reaches Vibe Code and must be dispatched asynchronously"
            )));
        }
        match method {
            "projectLinks/list" => self.project_links_list(),
            "vibeCode/projects/recover" => self.project_recover(params),
            "vibeCode/projects/select" => self.project_select(params),
            "vibeCode/projects/unlink" => self.project_unlink(params),
            "vibeCode/projects/cancel" => self.project_cancel(params),
            "vibeCode/teleport/cancel" => self.teleport_cancel(params),
            "loops/create" => self.loop_create(params),
            "loops/list" => self.loop_list(params),
            "loops/clear" => self.loop_clear(params),
            "loops/delete" => self.loop_delete(params),
            _ => Err(ProjectsServiceError::MethodNotFound(method.to_owned())),
        }
    }

    #[must_use]
    pub fn requires_deferred_dispatch(&self, method: &str) -> bool {
        DEFERRED_PROJECTS_METHODS.contains(&method)
    }

    pub async fn dispatch_deferred(
        &self,
        method: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        match method {
            method if method.starts_with("projectLinks/") => {
                self.project_links_deferred(method, params).await
            }
            "vibeCode/projects/create" => self.project_create(params).await,
            "vibeCode/projects/loadMore" => self.project_load_more(params).await,
            "vibeCode/projects/open" => self.project_open(params).await,
            "vibeCode/teleport/start" => self.teleport_start(params).await,
            "vibeCode/teleport/push/respond" => self.teleport_push_respond(params).await,
            _ => self.dispatch(method, params),
        }
    }

    fn next_operation_id(&self, prefix: &str) -> String {
        format!(
            "{prefix}-operation-{}",
            self.next_operation.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn persist_project_links(
        &self,
        links: &BTreeMap<String, SavedProjectLink>,
    ) -> Result<(), ProjectsServiceError> {
        let Some(path) = &self.project_link_store else {
            return Ok(());
        };
        if let Some(error) = &self.project_link_store_error {
            return Err(ProjectsServiceError::ProjectLinkPersistenceState(
                error.clone(),
            ));
        }
        persist_json_atomically(path, links, &NEXT_LINK_TEMP_FILE)
            .map_err(ProjectsServiceError::ProjectLinkPersistence)
    }
}

fn notification<const N: usize>(method: &str, entries: [(&str, Value); N]) -> ProjectsNotification {
    ProjectsNotification {
        method: method.to_owned(),
        params: entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    }
}

fn load_project_links(
    path: &Path,
) -> Result<BTreeMap<String, SavedProjectLink>, ProjectsServiceError> {
    match fs::read(path) {
        Ok(contents) => {
            let stored = serde_json::from_slice::<BTreeMap<String, StoredProjectLink>>(&contents)
                .map_err(ProjectsServiceError::Json)?;
            Ok(stored
                .into_iter()
                .map(|(root, link)| {
                    let link = match link {
                        StoredProjectLink::Detailed(link) => link,
                        StoredProjectLink::Legacy(project_id) => SavedProjectLink {
                            repo_url: String::new(),
                            project_name: project_id.clone(),
                            project_id,
                        },
                    };
                    (root, link)
                })
                .collect())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(ProjectsServiceError::ProjectLinkPersistence(error)),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredProjectLink {
    Detailed(SavedProjectLink),
    Legacy(String),
}

fn persist_json_atomically<T: Serialize>(
    path: &Path,
    value: &T,
    sequence: &AtomicU64,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let sequence = sequence.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.json");
    let temporary = path.with_file_name(format!(
        ".{file_name}.tmp-{}-{sequence}",
        std::process::id()
    ));
    let contents = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let write_result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(&contents)?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = replace_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn default_project_link_store() -> PathBuf {
    host::vibe_home().join("vibe-code-project-links.json")
}

fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(source, destination)
}

impl From<params::ParamError> for ProjectsServiceError {
    fn from(error: params::ParamError) -> Self {
        Self::InvalidParams(error.message())
    }
}

#[derive(Debug, Error)]
pub enum ProjectsServiceError {
    #[error("unknown projects method `{0}`")]
    MethodNotFound(String),
    #[error("invalid parameters: {0}")]
    InvalidParams(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Cloud(CloudError),
    /// A Vibe Code call that failed for a reason the caller cannot act on.
    ///
    /// The session-less project surface separates this from an authorization
    /// failure, because the reference answers the two with different codes and
    /// a client shows a sign-in prompt for one and a retry for the other.
    #[error("Vibe Code request failed: {0}")]
    VibeCode(String),
    #[error("scheduled-loop persistence failed: {0}")]
    Persistence(std::io::Error),
    #[error("scheduled-loop persistence is unavailable: {0}")]
    PersistenceState(String),
    #[error("Vibe Code project-link persistence failed: {0}")]
    ProjectLinkPersistence(std::io::Error),
    #[error("Vibe Code project-link persistence is unavailable: {0}")]
    ProjectLinkPersistenceState(String),
    #[error("projects background task stopped unexpectedly")]
    BackgroundTask,
    #[error("projects state lock is poisoned")]
    StatePoisoned,
    #[error("JSON conversion failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Error)]
pub enum ProjectsBuildError {
    #[error(transparent)]
    Cloud(#[from] CloudConfigError),
    #[error(transparent)]
    Service(ProjectsServiceError),
}

#[cfg(test)]
mod projects_tests;
