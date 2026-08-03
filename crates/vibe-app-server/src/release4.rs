use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::host::{self, now_seconds};
use crate::params;
use std::pin::Pin;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use command_group::CommandGroup;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use url::Url;

pub const RELEASE4_METHODS: &[&str] = &[
    "loops/clear",
    "loops/create",
    "loops/delete",
    "loops/list",
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
const DEFERRED_RELEASE4_METHODS: &[&str] = &[
    "vibeCode/projects/create",
    "vibeCode/projects/loadMore",
    "vibeCode/projects/open",
    "vibeCode/teleport/push/respond",
    "vibeCode/teleport/start",
];
const MIN_LOOP_INTERVAL_SECONDS: u64 = 30;
const MAX_LOOPS_PER_SESSION: usize = 50;
const PROJECT_PAGE_LIMIT: usize = 100;
const MAX_HEADLESS_PROJECT_PAGES: usize = 100;
const MAX_HEADLESS_PROJECTS: usize = PROJECT_PAGE_LIMIT * MAX_HEADLESS_PROJECT_PAGES;
const MAX_CLOUD_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CLOUD_TEXT_BYTES: usize = 64 * 1024;
const MAX_GIT_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_TELEPORT_DIFF_ENCODED_BYTES: usize = 1_000_000;
const DEFAULT_CLOUD_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CLOUD_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_GIT_NETWORK_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_VIBE_CODE_BASE_URL: &str = "https://chat.mistral.ai";
static NEXT_LOOP_TEMP_FILE: AtomicU64 = AtomicU64::new(1);
static NEXT_LINK_TEMP_FILE: AtomicU64 = AtomicU64::new(1);
static NEXT_GIT_INDEX_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub project_id: String,
    pub name: String,
    pub repositories: Vec<ProjectRepository>,
    pub is_read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRepository {
    pub repo_url: String,
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPage {
    pub projects: Vec<Project>,
    pub next_cursor: Option<String>,
}

pub trait ProjectCloud: Send + Sync {
    fn create(
        &self,
        name: &str,
        repo_url: &str,
        default_branch: &str,
    ) -> Result<Project, CloudError>;
    fn list(&self, cursor: Option<&str>) -> Result<ProjectPage, CloudError>;
}

pub type CloudFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, CloudError>> + Send + 'a>>;

pub trait AsyncProjectCloud: Send + Sync {
    fn create<'a>(
        &'a self,
        name: &'a str,
        repo_url: &'a str,
        default_branch: &'a str,
    ) -> CloudFuture<'a, Project>;

    fn list<'a>(&'a self, cursor: Option<&'a str>) -> CloudFuture<'a, ProjectPage>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TeleportRepository {
    pub repo_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<TeleportRepositoryDiff>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TeleportRepositoryDiff {
    pub format: &'static str,
    pub encoding: &'static str,
    pub compression: &'static str,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeleportStartRequest {
    pub project_id: String,
    pub idempotency_key: String,
    pub summary: String,
    pub repository: TeleportRepository,
}

pub trait TeleportCloud: Send + Sync {
    fn start(&self, request: &TeleportStartRequest) -> Result<String, CloudError>;
}

pub trait AsyncTeleportCloud: Send + Sync {
    fn start<'a>(&'a self, request: &'a TeleportStartRequest) -> CloudFuture<'a, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSnapshot {
    pub repository: String,
    pub dirty: bool,
    pub unpushed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitPushStatus {
    pub unpushed_count: u64,
    pub branch_not_pushed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectGitSnapshot {
    pub snapshot: GitSnapshot,
    pub repo_root: String,
    pub remote_name: String,
    pub branch: Option<String>,
}

pub trait GitProbe: Send + Sync {
    fn inspect(&self, working_directory: &Path) -> Result<GitSnapshot, CloudError>;

    fn inspect_project(&self, working_directory: &Path) -> Result<ProjectGitSnapshot, CloudError> {
        let snapshot = self.inspect(working_directory)?;
        let repo_root = canonical_repository_root(working_directory)?;
        Ok(ProjectGitSnapshot {
            snapshot,
            repo_root,
            remote_name: String::new(),
            branch: None,
        })
    }

    fn inspect_for_teleport(
        &self,
        working_directory: &Path,
    ) -> Result<(GitSnapshot, TeleportRepository, GitPushStatus), CloudError> {
        let snapshot = self.inspect(working_directory)?;
        let repository = TeleportRepository {
            repo_url: snapshot.repository.clone(),
            branch: None,
            commit_sha: None,
            diff: None,
        };
        let push_status = GitPushStatus {
            unpushed_count: u64::from(snapshot.unpushed),
            branch_not_pushed: snapshot.unpushed,
        };
        Ok((snapshot, repository, push_status))
    }

    fn push(&self, working_directory: &Path) -> Result<(), CloudError>;
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CloudError {
    #[error("cloud service is unavailable: {0}")]
    Unavailable(String),
    #[error("cloud authentication expired: {0}")]
    Unauthorized(String),
    #[error("Git operation failed: {0}")]
    Git(String),
}

#[derive(Debug, Error)]
pub enum CloudConfigError {
    #[error("MISTRAL_API_KEY is not set; authenticate or provide a Vibe Code API key")]
    MissingApiKey,
    #[error("invalid Vibe Code base URL: {0}")]
    InvalidBaseUrl(String),
    #[error("failed to build the bounded Vibe Code HTTP client")]
    HttpClient,
}

#[derive(Clone)]
pub struct VibeCodeCloudConfig {
    base_url: Url,
    api_key: SecretString,
    connect_timeout: Duration,
    request_timeout: Duration,
    max_response_bytes: usize,
    max_start_attempts: usize,
}

impl std::fmt::Debug for VibeCodeCloudConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VibeCodeCloudConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("max_start_attempts", &self.max_start_attempts)
            .finish()
    }
}

impl VibeCodeCloudConfig {
    pub fn new(base_url: &str, api_key: SecretString) -> Result<Self, CloudConfigError> {
        if api_key.expose_secret().trim().is_empty() {
            return Err(CloudConfigError::MissingApiKey);
        }
        let base_url = validate_cloud_base_url(base_url)?;
        Ok(Self {
            base_url,
            api_key,
            connect_timeout: DEFAULT_CLOUD_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_CLOUD_REQUEST_TIMEOUT,
            max_response_bytes: MAX_CLOUD_RESPONSE_BYTES,
            max_start_attempts: 3,
        })
    }

    pub fn mistral(api_key: SecretString) -> Result<Self, CloudConfigError> {
        Self::new(DEFAULT_VIBE_CODE_BASE_URL, api_key)
    }

    pub fn from_credential(api_key: impl Into<String>) -> Result<Self, CloudConfigError> {
        let base_url = std::env::var("VIBE_CODE_SESSIONS_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_VIBE_CODE_BASE_URL.to_owned());
        Self::new(&base_url, SecretString::from(api_key.into()))
    }

    pub fn from_env() -> Result<Self, CloudConfigError> {
        let api_key = std::env::var("MISTRAL_API_KEY")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or(CloudConfigError::MissingApiKey)?;
        Self::from_credential(api_key)
    }

    #[must_use]
    pub fn with_timeouts(mut self, connect_timeout: Duration, request_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout.max(Duration::from_millis(1));
        self.request_timeout = request_timeout.max(Duration::from_millis(1));
        self
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url.as_str().trim_end_matches('/'), path)
    }
}

#[derive(Clone)]
enum ProjectCloudBackend {
    Sync(Arc<dyn ProjectCloud>),
    Async(Arc<dyn AsyncProjectCloud>),
}

#[derive(Clone)]
enum TeleportCloudBackend {
    Sync(Arc<dyn TeleportCloud>),
    Async(Arc<dyn AsyncTeleportCloud>),
}

struct UnavailableProjectCloud;

impl ProjectCloud for UnavailableProjectCloud {
    fn create(
        &self,
        _name: &str,
        _repo_url: &str,
        _default_branch: &str,
    ) -> Result<Project, CloudError> {
        Err(CloudError::Unavailable(
            "Vibe Code is not configured; provide MISTRAL_API_KEY and retry".to_owned(),
        ))
    }

    fn list(&self, _cursor: Option<&str>) -> Result<ProjectPage, CloudError> {
        Err(CloudError::Unavailable(
            "Vibe Code is not configured; provide MISTRAL_API_KEY and retry".to_owned(),
        ))
    }
}

struct UnavailableTeleportCloud;

impl TeleportCloud for UnavailableTeleportCloud {
    fn start(&self, _request: &TeleportStartRequest) -> Result<String, CloudError> {
        Err(CloudError::Unavailable(
            "Teleport is not configured; provide MISTRAL_API_KEY and retry".to_owned(),
        ))
    }
}

struct VibeCodeHttpCloud {
    config: VibeCodeCloudConfig,
    client: Client,
}

impl VibeCodeHttpCloud {
    fn new(config: VibeCodeCloudConfig) -> Result<Self, CloudConfigError> {
        let client = Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| CloudConfigError::HttpClient)?;
        Ok(Self { config, client })
    }

    async fn decode<T: DeserializeOwned>(
        &self,
        response: reqwest::Response,
        operation: &str,
    ) -> Result<T, CloudError> {
        let status = response.status();
        if !status.is_success() {
            return Err(cloud_status_error(status, operation));
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| {
                CloudError::Unavailable(format!(
                    "{operation} response could not be read; local state is unchanged"
                ))
            })?;
            if body.len().saturating_add(chunk.len()) > self.config.max_response_bytes {
                return Err(CloudError::Unavailable(format!(
                    "{operation} response exceeded the {} byte safety limit",
                    self.config.max_response_bytes
                )));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| {
            CloudError::Unavailable(format!(
                "{operation} returned an invalid response; local state is unchanged"
            ))
        })
    }

    async fn list_projects(&self, cursor: Option<&str>) -> Result<ProjectPage, CloudError> {
        let mut request = self
            .client
            .get(self.config.endpoint("/api/v1/code/projects"))
            .bearer_auth(self.config.api_key.expose_secret())
            .header(reqwest::header::ACCEPT, "application/json")
            .query(&[("limit", PROJECT_PAGE_LIMIT)]);
        if let Some(cursor) = cursor {
            request = request.query(&[("cursor", cursor)]);
        }
        let response = request
            .send()
            .await
            .map_err(|_| cloud_request_error("Vibe Code project listing"))?;
        let response: ProjectListResponse =
            self.decode(response, "Vibe Code project listing").await?;
        if response.items.len() > PROJECT_PAGE_LIMIT {
            return Err(CloudError::Unavailable(format!(
                "Vibe Code project listing exceeded the {PROJECT_PAGE_LIMIT} item page limit"
            )));
        }
        Ok(ProjectPage {
            projects: response
                .items
                .into_iter()
                .map(ProjectResponse::into_project)
                .collect::<Result<Vec<_>, _>>()?,
            next_cursor: bounded_optional_text(response.next_cursor, "project cursor")?,
        })
    }

    async fn create_project(
        &self,
        name: &str,
        repo_url: &str,
        default_branch: &str,
    ) -> Result<Project, CloudError> {
        validate_cloud_text(name, "project name")?;
        validate_cloud_text(repo_url, "repository URL")?;
        validate_cloud_text(default_branch, "default branch")?;
        let response = self
            .client
            .post(self.config.endpoint("/api/v1/code/projects"))
            .bearer_auth(self.config.api_key.expose_secret())
            .json(&json!({
                "name": name,
                "repositories": [{
                    "repoUrl": repo_url,
                    "defaultBranch": default_branch,
                }],
            }))
            .send()
            .await
            .map_err(|_| cloud_request_error("Vibe Code project creation"))?;
        self.decode::<ProjectResponse>(response, "Vibe Code project creation")
            .await?
            .into_project()
    }

    async fn start_teleport(&self, request: &TeleportStartRequest) -> Result<String, CloudError> {
        validate_cloud_text(&request.project_id, "project ID")?;
        validate_cloud_text(&request.idempotency_key, "Teleport idempotency key")?;
        validate_cloud_text(&request.summary, "Teleport message")?;
        validate_cloud_text(&request.repository.repo_url, "repository URL")?;
        if let Some(branch) = &request.repository.branch {
            validate_cloud_text(branch, "repository branch")?;
        }
        if let Some(commit_sha) = &request.repository.commit_sha {
            validate_cloud_text(commit_sha, "repository commit")?;
        }
        if let Some(diff) = &request.repository.diff {
            if diff.content.len() > MAX_TELEPORT_DIFF_ENCODED_BYTES {
                return Err(CloudError::Git(format!(
                    "working-tree diff exceeded the {MAX_TELEPORT_DIFF_ENCODED_BYTES} byte safety limit"
                )));
            }
            validate_cloud_text(diff.format, "repository diff format")?;
            validate_cloud_text(diff.encoding, "repository diff encoding")?;
            validate_cloud_text(diff.compression, "repository diff compression")?;
        }
        let repository = serde_json::to_value(&request.repository).map_err(|_| {
            CloudError::Unavailable("Teleport repository context could not be encoded".to_owned())
        })?;
        let body = json!({
            "projectId": request.project_id,
            "source": "vibe_code_cli",
            "idempotencyKey": request.idempotency_key,
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": request.summary}],
            },
            "context": {
                "repositories": [repository],
            },
        });
        for attempt in 0..self.config.max_start_attempts {
            let response = self
                .client
                .post(self.config.endpoint("/api/v1/code/sessions"))
                .bearer_auth(self.config.api_key.expose_secret())
                .json(&body)
                .send()
                .await;
            match response {
                Ok(response)
                    if response.status() == StatusCode::GATEWAY_TIMEOUT
                        && attempt + 1 < self.config.max_start_attempts =>
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Ok(response) if response.status() == StatusCode::GATEWAY_TIMEOUT => {
                    return Err(ambiguous_teleport_error());
                }
                Ok(response) => {
                    let response: TeleportResponse =
                        self.decode(response, "Vibe Code Teleport start").await?;
                    for (value, label) in [
                        (&response.session_id, "Teleport session ID"),
                        (&response.web_session_id, "Teleport web session ID"),
                        (&response.status, "Teleport status"),
                    ] {
                        validate_cloud_text(value, label)?;
                    }
                    if response.project_id != request.project_id {
                        return Err(CloudError::Unavailable(
                            "Vibe Code Teleport returned a different project; local state is unchanged"
                                .to_owned(),
                        ));
                    }
                    let url = Url::parse(&response.url).map_err(|_| {
                        CloudError::Unavailable(
                            "Vibe Code Teleport returned an invalid URL".to_owned(),
                        )
                    })?;
                    if !matches!(url.scheme(), "http" | "https")
                        || !url.username().is_empty()
                        || url.password().is_some()
                    {
                        return Err(CloudError::Unavailable(
                            "Vibe Code Teleport returned an unsafe URL".to_owned(),
                        ));
                    }
                    return Ok(response.url);
                }
                Err(error)
                    if is_ambiguous_request_error(&error)
                        && attempt + 1 < self.config.max_start_attempts =>
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) if is_ambiguous_request_error(&error) => {
                    return Err(ambiguous_teleport_error());
                }
                Err(_) => return Err(cloud_request_error("Vibe Code Teleport start")),
            }
        }
        Err(ambiguous_teleport_error())
    }
}

impl AsyncProjectCloud for VibeCodeHttpCloud {
    fn create<'a>(
        &'a self,
        name: &'a str,
        repo_url: &'a str,
        default_branch: &'a str,
    ) -> CloudFuture<'a, Project> {
        Box::pin(async move { self.create_project(name, repo_url, default_branch).await })
    }

    fn list<'a>(&'a self, cursor: Option<&'a str>) -> CloudFuture<'a, ProjectPage> {
        Box::pin(async move { self.list_projects(cursor).await })
    }
}

impl AsyncTeleportCloud for VibeCodeHttpCloud {
    fn start<'a>(&'a self, request: &'a TeleportStartRequest) -> CloudFuture<'a, String> {
        Box::pin(async move { self.start_teleport(request).await })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectRepositoryResponse {
    repo_url: String,
    #[serde(default)]
    default_branch: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectResponse {
    #[serde(rename = "id")]
    project_id: String,
    name: String,
    #[serde(default)]
    repositories: Vec<ProjectRepositoryResponse>,
    #[serde(default)]
    is_read_only: bool,
}

impl ProjectResponse {
    fn into_project(self) -> Result<Project, CloudError> {
        validate_cloud_text(&self.project_id, "project ID")?;
        validate_cloud_text(&self.name, "project name")?;
        let repositories = self
            .repositories
            .into_iter()
            .map(|repository| {
                validate_cloud_text(&repository.repo_url, "repository URL")?;
                if let Some(branch) = &repository.default_branch {
                    validate_cloud_text(branch, "default branch")?;
                }
                Ok(ProjectRepository {
                    repo_url: repository.repo_url,
                    default_branch: repository.default_branch,
                })
            })
            .collect::<Result<Vec<_>, CloudError>>()?;
        Ok(Project {
            project_id: self.project_id,
            name: self.name,
            repositories,
            is_read_only: self.is_read_only,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectListResponse {
    items: Vec<ProjectResponse>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TeleportResponse {
    session_id: String,
    web_session_id: String,
    project_id: String,
    status: String,
    url: String,
}

fn validate_cloud_base_url(value: &str) -> Result<Url, CloudConfigError> {
    let url = Url::parse(value).map_err(|_| {
        CloudConfigError::InvalidBaseUrl("expected an absolute HTTPS URL".to_owned())
    })?;
    let loopback_http = url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
    if url.scheme() != "https" && !loopback_http {
        return Err(CloudConfigError::InvalidBaseUrl(
            "credentials require HTTPS (plain HTTP is allowed only for loopback tests)".to_owned(),
        ));
    }
    if url.cannot_be_a_base()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(CloudConfigError::InvalidBaseUrl(
            "userinfo, query strings, and fragments are not allowed".to_owned(),
        ));
    }
    Ok(url)
}

fn cloud_status_error(status: StatusCode, operation: &str) -> CloudError {
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return CloudError::Unauthorized(
            "MISTRAL_API_KEY was rejected; authenticate again and retry".to_owned(),
        );
    }
    CloudError::Unavailable(format!(
        "{operation} failed with HTTP status {}; local state is unchanged",
        status.as_u16()
    ))
}

fn cloud_request_error(operation: &str) -> CloudError {
    CloudError::Unavailable(format!(
        "{operation} could not reach Vibe Code within the configured timeout; check the base URL and network"
    ))
}

fn ambiguous_teleport_error() -> CloudError {
    CloudError::Unavailable(
        "Vibe Code did not confirm Teleport session creation after bounded retries; check Vibe Code Web before retrying"
            .to_owned(),
    )
}

fn is_ambiguous_request_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
}

fn validate_cloud_text(value: &str, label: &str) -> Result<(), CloudError> {
    if value.trim().is_empty() {
        return Err(CloudError::Unavailable(format!(
            "{label} was empty; local state is unchanged"
        )));
    }
    if value.len() > MAX_CLOUD_TEXT_BYTES {
        return Err(CloudError::Unavailable(format!(
            "{label} exceeded the {MAX_CLOUD_TEXT_BYTES} byte safety limit"
        )));
    }
    Ok(())
}

fn bounded_optional_text(value: Option<String>, label: &str) -> Result<Option<String>, CloudError> {
    value
        .map(|value| {
            validate_cloud_text(&value, label)?;
            Ok(value)
        })
        .transpose()
}

struct UnavailableGitProbe;

impl GitProbe for UnavailableGitProbe {
    fn inspect(&self, _working_directory: &Path) -> Result<GitSnapshot, CloudError> {
        Err(CloudError::Git(
            "the working directory is not an inspectable Git repository".to_owned(),
        ))
    }

    fn push(&self, _working_directory: &Path) -> Result<(), CloudError> {
        Err(CloudError::Git(
            "no Git push implementation is configured".to_owned(),
        ))
    }
}

pub struct CommandGitProbe {
    git_program: PathBuf,
    command_timeout: Duration,
    network_timeout: Duration,
    runner: Arc<dyn GitCommandRunner>,
}

impl Default for CommandGitProbe {
    fn default() -> Self {
        Self {
            git_program: PathBuf::from("git"),
            command_timeout: DEFAULT_GIT_COMMAND_TIMEOUT,
            network_timeout: DEFAULT_GIT_NETWORK_TIMEOUT,
            runner: Arc::new(SystemGitCommandRunner),
        }
    }
}

impl CommandGitProbe {
    #[must_use]
    pub fn with_timeouts(mut self, command_timeout: Duration, network_timeout: Duration) -> Self {
        self.command_timeout = command_timeout.max(Duration::from_millis(1));
        self.network_timeout = network_timeout.max(Duration::from_millis(1));
        self
    }

    fn metadata(&self, working_directory: &Path) -> Result<GitMetadata, CloudError> {
        let repo_root = self.git_text(
            working_directory,
            &["rev-parse", "--show-toplevel"],
            self.command_timeout,
            "locate the repository root",
        )?;
        let repo_root = fs::canonicalize(repo_root.trim())
            .map_err(|_| CloudError::Git("Git returned an invalid repository root".to_owned()))?;
        let remotes = self.git_text(
            working_directory,
            &["remote"],
            self.command_timeout,
            "list Git remotes",
        )?;
        let mut candidates = remotes
            .lines()
            .map(str::trim)
            .filter(|remote| !remote.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        candidates.sort_by_key(|remote| (remote != "origin", remote.clone()));
        let mut selected = None;
        for remote in candidates {
            let remote_url = match self.git_text_os(
                working_directory,
                &[
                    OsString::from("remote"),
                    OsString::from("get-url"),
                    OsString::from("--"),
                    OsString::from(&remote),
                ],
                self.command_timeout,
                "read the Git remote URL",
            ) {
                Ok(remote_url) => remote_url,
                Err(_) => continue,
            };
            let Ok(repo_url) = sanitize_git_remote(&remote_url) else {
                continue;
            };
            let is_github = Url::parse(&repo_url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|host| host.eq_ignore_ascii_case("github.com"));
            if is_github {
                selected = Some((remote, repo_url));
                break;
            }
        }
        let (remote, repo_url) = selected.ok_or_else(|| {
            CloudError::Git("Teleport requires a GitHub remote; configure one and retry".to_owned())
        })?;
        let branch = self.git_text(
            working_directory,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            self.command_timeout,
            "read the current branch",
        )?;
        let branch = branch.trim().to_owned();
        if branch.is_empty() {
            return Err(CloudError::Git(
                "Teleport requires a checked-out branch; switch branches and retry".to_owned(),
            ));
        }
        let commit_sha = self.git_text(
            working_directory,
            &["rev-parse", "--verify", "HEAD"],
            self.command_timeout,
            "read the current commit",
        )?;
        let commit_sha = commit_sha.trim().to_owned();
        if !(7..=64).contains(&commit_sha.len())
            || !commit_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(CloudError::Git(
                "Git returned an invalid current commit".to_owned(),
            ));
        }
        Ok(GitMetadata {
            repo_root,
            remote,
            repo_url,
            branch,
            commit_sha,
        })
    }

    fn inspection(
        &self,
        working_directory: &Path,
    ) -> Result<(GitSnapshot, TeleportRepository, GitPushStatus), CloudError> {
        let metadata = self.metadata(working_directory)?;
        let status = self.git_text(
            working_directory,
            &["status", "--porcelain=v1", "--untracked-files=normal"],
            self.command_timeout,
            "inspect the Git working tree",
        )?;
        let dirty = !status.is_empty();
        let fetch_args = [
            OsString::from("fetch"),
            OsString::from("--quiet"),
            OsString::from("--no-tags"),
            OsString::from("--"),
            OsString::from(&metadata.remote),
        ];
        let _ = self.run_git(
            working_directory,
            &fetch_args,
            self.network_timeout,
            "refresh the Git remote",
        );
        let remote_ref = format!("refs/remotes/{}/{}", metadata.remote, metadata.branch);
        let branch_pushed = self
            .run_git(
                working_directory,
                &[
                    OsString::from("show-ref"),
                    OsString::from("--verify"),
                    OsString::from("--quiet"),
                    OsString::from(&remote_ref),
                ],
                self.command_timeout,
                "check the remote branch",
            )?
            .status
            .success();
        let revision_range = if branch_pushed {
            format!("{remote_ref}..HEAD")
        } else {
            "HEAD".to_owned()
        };
        let unpushed_count = self
            .git_text_os(
                working_directory,
                &[
                    OsString::from("rev-list"),
                    OsString::from("--count"),
                    OsString::from(revision_range),
                ],
                self.command_timeout,
                "count unpushed commits",
            )?
            .trim()
            .parse::<u64>()
            .map_err(|_| {
                CloudError::Git("Git returned an invalid unpushed commit count".to_owned())
            })?;
        let diff = self.working_tree_diff(&metadata.repo_root, dirty)?;
        let branch_not_pushed = !branch_pushed;
        let unpushed = branch_not_pushed || unpushed_count > 0;
        Ok((
            GitSnapshot {
                repository: metadata.repo_url.clone(),
                dirty,
                unpushed,
            },
            TeleportRepository {
                repo_url: metadata.repo_url,
                branch: Some(metadata.branch),
                commit_sha: Some(metadata.commit_sha),
                diff,
            },
            GitPushStatus {
                unpushed_count,
                branch_not_pushed,
            },
        ))
    }

    fn working_tree_diff(
        &self,
        working_directory: &Path,
        dirty: bool,
    ) -> Result<Option<TeleportRepositoryDiff>, CloudError> {
        if !dirty {
            return Ok(None);
        }
        let git_directory = self.git_text(
            working_directory,
            &["rev-parse", "--absolute-git-dir"],
            self.command_timeout,
            "locate Git metadata",
        )?;
        let git_directory = PathBuf::from(git_directory.trim());
        if !git_directory.is_absolute() || !git_directory.is_dir() {
            return Err(CloudError::Git(
                "Git returned an invalid metadata directory".to_owned(),
            ));
        }
        let sequence = NEXT_GIT_INDEX_FILE.fetch_add(1, Ordering::Relaxed);
        let temporary_index = git_directory.join(format!(
            ".vibe-teleport-index-{}-{sequence}",
            std::process::id()
        ));
        let environment = [(
            OsString::from("GIT_INDEX_FILE"),
            temporary_index.as_os_str().to_owned(),
        )];
        let result = (|| {
            for (args, action) in [
                (
                    vec![OsString::from("read-tree"), OsString::from("HEAD")],
                    "initialize the Teleport diff index",
                ),
                (
                    vec![
                        OsString::from("add"),
                        OsString::from("-A"),
                        OsString::from("--"),
                        OsString::from("."),
                    ],
                    "stage working-tree changes for Teleport",
                ),
            ] {
                let result = self.run_git_with_environment(
                    working_directory,
                    &args,
                    &environment,
                    self.command_timeout,
                    action,
                )?;
                if !result.status.success() {
                    return Err(CloudError::Git(format!(
                        "failed to {action}; the real Git index was not changed"
                    )));
                }
            }
            let diff = self.run_git_with_environment(
                working_directory,
                &[
                    OsString::from("diff"),
                    OsString::from("--cached"),
                    OsString::from("--binary"),
                    OsString::from("--no-ext-diff"),
                    OsString::from("HEAD"),
                    OsString::from("--"),
                ],
                &environment,
                self.command_timeout,
                "capture the working-tree diff",
            )?;
            if !diff.status.success() {
                return Err(CloudError::Git(
                    "failed to capture the working-tree diff; local state is unchanged".to_owned(),
                ));
            }
            if diff.stdout_truncated {
                return Err(CloudError::Git(
                    "working-tree diff exceeded the local Git output safety limit".to_owned(),
                ));
            }
            if diff.stdout.is_empty() {
                return Err(CloudError::Git(
                    "Git reported dirty files but produced no transferable diff".to_owned(),
                ));
            }
            encode_working_tree_diff(&diff.stdout)
        })();
        let _ = fs::remove_file(&temporary_index);
        result.map(Some)
    }

    fn git_text(
        &self,
        working_directory: &Path,
        args: &[&str],
        timeout: Duration,
        action: &str,
    ) -> Result<String, CloudError> {
        let args = args.iter().map(OsString::from).collect::<Vec<_>>();
        self.git_text_os(working_directory, &args, timeout, action)
    }

    fn git_text_os(
        &self,
        working_directory: &Path,
        args: &[OsString],
        timeout: Duration,
        action: &str,
    ) -> Result<String, CloudError> {
        let result = self.run_git(working_directory, args, timeout, action)?;
        if !result.status.success() {
            return Err(CloudError::Git(format!(
                "failed to {action}; verify the repository and Git credentials"
            )));
        }
        if result.stdout_truncated {
            return Err(CloudError::Git(format!(
                "failed to {action}: Git output exceeded the safety limit"
            )));
        }
        String::from_utf8(result.stdout)
            .map_err(|_| CloudError::Git(format!("failed to {action}: Git output was not UTF-8")))
    }

    fn run_git(
        &self,
        working_directory: &Path,
        args: &[OsString],
        timeout: Duration,
        action: &str,
    ) -> Result<GitCommandResult, CloudError> {
        self.run_git_with_environment(working_directory, args, &[], timeout, action)
    }

    fn run_git_with_environment(
        &self,
        working_directory: &Path,
        args: &[OsString],
        environment: &[(OsString, OsString)],
        timeout: Duration,
        action: &str,
    ) -> Result<GitCommandResult, CloudError> {
        self.runner
            .run(
                &self.git_program,
                working_directory,
                args,
                environment,
                timeout,
                MAX_GIT_OUTPUT_BYTES,
            )
            .map_err(|error| match error {
                GitCommandError::Timeout => CloudError::Git(format!(
                    "timed out while trying to {action}; local state is unchanged"
                )),
                GitCommandError::Io => CloudError::Git(format!(
                    "could not run Git to {action}; install Git and retry"
                )),
            })
    }
}

fn encode_working_tree_diff(diff: &[u8]) -> Result<TeleportRepositoryDiff, CloudError> {
    let compressed = zstd::stream::encode_all(diff, 3)
        .map_err(|_| CloudError::Git("working-tree diff compression failed".to_owned()))?;
    let content = BASE64_STANDARD.encode(compressed);
    if content.len() > MAX_TELEPORT_DIFF_ENCODED_BYTES {
        return Err(CloudError::Git(format!(
            "working-tree diff exceeded the {MAX_TELEPORT_DIFF_ENCODED_BYTES} byte Teleport limit"
        )));
    }
    Ok(TeleportRepositoryDiff {
        format: "git-diff",
        encoding: "base64",
        compression: "zstd",
        content,
    })
}

impl GitProbe for CommandGitProbe {
    fn inspect(&self, working_directory: &Path) -> Result<GitSnapshot, CloudError> {
        self.inspection(working_directory)
            .map(|(snapshot, _, _)| snapshot)
    }

    fn inspect_project(&self, working_directory: &Path) -> Result<ProjectGitSnapshot, CloudError> {
        let metadata = self.metadata(working_directory)?;
        Ok(ProjectGitSnapshot {
            snapshot: GitSnapshot {
                repository: metadata.repo_url,
                dirty: false,
                unpushed: false,
            },
            repo_root: metadata.repo_root.to_string_lossy().into_owned(),
            remote_name: metadata.remote,
            branch: Some(metadata.branch),
        })
    }

    fn inspect_for_teleport(
        &self,
        working_directory: &Path,
    ) -> Result<(GitSnapshot, TeleportRepository, GitPushStatus), CloudError> {
        self.inspection(working_directory)
    }

    fn push(&self, working_directory: &Path) -> Result<(), CloudError> {
        let metadata = self.metadata(working_directory)?;
        let result = self.run_git(
            working_directory,
            &[
                OsString::from("push"),
                OsString::from("--set-upstream"),
                OsString::from("--"),
                OsString::from(metadata.remote),
                OsString::from(metadata.branch),
            ],
            self.network_timeout,
            "push the current branch",
        )?;
        if !result.status.success() {
            return Err(CloudError::Git(
                "Git push failed; verify remote access and push the branch manually".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct GitMetadata {
    repo_root: PathBuf,
    remote: String,
    repo_url: String,
    branch: String,
    commit_sha: String,
}

#[derive(Debug)]
struct GitCommandResult {
    status: ExitStatus,
    stdout: Vec<u8>,
    stdout_truncated: bool,
}

#[derive(Debug, Clone, Copy)]
enum GitCommandError {
    Timeout,
    Io,
}

trait GitCommandRunner: Send + Sync {
    fn run(
        &self,
        program: &Path,
        working_directory: &Path,
        args: &[OsString],
        environment: &[(OsString, OsString)],
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<GitCommandResult, GitCommandError>;
}

struct SystemGitCommandRunner;

impl GitCommandRunner for SystemGitCommandRunner {
    fn run(
        &self,
        program: &Path,
        working_directory: &Path,
        args: &[OsString],
        environment: &[(OsString, OsString)],
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<GitCommandResult, GitCommandError> {
        let mut command = Command::new(program);
        command
            .arg("-C")
            .arg(working_directory)
            .args(args)
            .envs(environment.iter().cloned())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "Never")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.group_spawn().map_err(|_| GitCommandError::Io)?;
        let stdout = child.inner().stdout.take().ok_or(GitCommandError::Io)?;
        let stderr = child.inner().stderr.take().ok_or(GitCommandError::Io)?;
        let stdout_reader = thread::spawn(move || drain_process_output(stdout, max_output_bytes));
        let stderr_reader = thread::spawn(move || drain_process_output(stderr, max_output_bytes));
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait().map_err(|_| GitCommandError::Io)? {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(GitCommandError::Timeout);
                }
                None => thread::sleep(Duration::from_millis(10)),
            }
        };
        let (stdout, stdout_truncated) = stdout_reader
            .join()
            .map_err(|_| GitCommandError::Io)?
            .map_err(|_| GitCommandError::Io)?;
        let _stderr = stderr_reader
            .join()
            .map_err(|_| GitCommandError::Io)?
            .map_err(|_| GitCommandError::Io)?;
        Ok(GitCommandResult {
            status,
            stdout,
            stdout_truncated,
        })
    }
}

fn drain_process_output(
    mut output: impl Read,
    max_output_bytes: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = output.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = max_output_bytes.saturating_sub(retained.len());
        let keep = remaining.min(read);
        retained.extend_from_slice(&buffer[..keep]);
        truncated |= keep < read;
    }
    Ok((retained, truncated))
}

fn canonical_repository_root(path: &Path) -> Result<String, CloudError> {
    if let Ok(path) = fs::canonicalize(path) {
        return Ok(path.to_string_lossy().into_owned());
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| CloudError::Git("working directory could not be resolved".to_owned()))?
            .join(path)
    };
    Ok(absolute.to_string_lossy().into_owned())
}

fn normalize_repo_url(value: &str) -> String {
    let sanitized = sanitize_git_remote(value).unwrap_or_else(|_| value.trim().to_owned());
    let mut normalized = if let Ok(url) = Url::parse(&sanitized) {
        match (url.host_str(), url.path().trim_matches('/')) {
            (Some(host), path) if !path.is_empty() => format!("{host}/{path}"),
            _ => sanitized,
        }
    } else {
        sanitized
    };
    normalized = normalized.trim().trim_end_matches('/').to_ascii_lowercase();
    normalized
        .strip_suffix(".git")
        .unwrap_or(&normalized)
        .to_owned()
}

fn is_project_linked_to_repo(project: &Project, repo_url: &str) -> bool {
    let normalized_repo_url = normalize_repo_url(repo_url);
    project
        .repositories
        .iter()
        .any(|repository| normalize_repo_url(&repository.repo_url) == normalized_repo_url)
}

fn project_is_selectable(project: &Project, repo_url: &str) -> bool {
    !project.is_read_only && is_project_linked_to_repo(project, repo_url)
}

fn suggested_project_name(git: &ProjectGitSnapshot) -> String {
    let root_name = Path::new(&git.repo_root)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if let Some(root_name) = root_name {
        return root_name.to_owned();
    }
    normalize_repo_url(&git.snapshot.repository)
        .rsplit('/')
        .next()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("vibe-project")
        .to_owned()
}

fn sanitize_git_remote(raw: &str) -> Result<String, CloudError> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(CloudError::Git(
            "Git remote URL is empty; configure a fetchable remote".to_owned(),
        ));
    }
    let windows_drive = value.as_bytes().get(1) == Some(&b':')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic);
    if windows_drive
        || value.starts_with('/')
        || value.starts_with('\\')
        || value.starts_with("./")
        || value.starts_with("../")
    {
        return Err(CloudError::Git(
            "Teleport requires a network Git remote; local paths are not allowed".to_owned(),
        ));
    }
    if let Some((authority, path)) = value.split_once(':')
        && !value.contains("://")
        && !authority.contains('/')
        && !authority.eq_ignore_ascii_case("http")
        && !authority.eq_ignore_ascii_case("https")
    {
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        if host.is_empty()
            || path.is_empty()
            || path.starts_with('/')
            || path.starts_with('\\')
            || host.chars().any(char::is_whitespace)
            || path.chars().any(char::is_control)
        {
            return Err(CloudError::Git(
                "Git remote URL is invalid; configure a fetchable remote".to_owned(),
            ));
        }
        return Ok(format!("https://{host}/{}", path.trim_start_matches('/')));
    }
    let mut url = Url::parse(value).map_err(|_| {
        CloudError::Git("Git remote URL is invalid; configure a fetchable remote".to_owned())
    })?;
    if !matches!(url.scheme(), "http" | "https" | "ssh" | "git") {
        return Err(CloudError::Git(
            "Teleport requires a network Git remote".to_owned(),
        ));
    }
    if url.host_str().is_none()
        || url.path().trim_matches('/').is_empty()
        || url.path().contains('\\')
    {
        return Err(CloudError::Git(
            "Git remote URL is invalid; configure a fetchable remote".to_owned(),
        ));
    }
    if matches!(url.scheme(), "ssh" | "git") {
        let host = url.host_str().unwrap_or_default();
        return Ok(format!("https://{host}{}", url.path()));
    }
    url.set_username("")
        .map_err(|_| CloudError::Git("Git remote URL could not be sanitized".to_owned()))?;
    url.set_password(None)
        .map_err(|_| CloudError::Git("Git remote URL could not be sanitized".to_owned()))?;
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release4Notification {
    pub method: String,
    pub params: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release4Dispatch {
    pub result: BTreeMap<String, Value>,
    pub notifications: Vec<Release4Notification>,
}

impl Release4Dispatch {
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
        notifications: Vec<Release4Notification>,
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

#[derive(Clone, Default)]
struct ProjectState {
    pickers: BTreeMap<String, ProjectPicker>,
    linked_projects: BTreeMap<String, SavedProjectLink>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectPickerPurpose {
    Configure,
    Teleport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedProjectLink {
    repo_url: String,
    project_id: String,
    project_name: String,
}

#[derive(Clone)]
struct ProjectPicker {
    session_id: String,
    repo_root: String,
    repo_url: String,
    remote_name: String,
    branch: Option<String>,
    projects: BTreeMap<String, Project>,
    selected: Option<String>,
    saved_link: Option<SavedProjectLink>,
    saved_project_link_cleared: bool,
    project_repo_remote_changed: bool,
    next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TeleportState {
    SummarizingContext,
    CheckingGit,
    PushRequired,
    Pushing,
    StartingWorkflow,
    Complete,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct TeleportOperation {
    id: String,
    session_id: String,
    project_id: String,
    working_directory: PathBuf,
    summary: String,
    repository: TeleportRepository,
    state: TeleportState,
    push_response: Option<bool>,
    unpushed_count: u64,
    branch_not_pushed: bool,
    url: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopState {
    Scheduled,
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScheduledLoop {
    pub id: String,
    pub session_id: String,
    pub prompt: String,
    pub interval_seconds: u64,
    pub next_fire_at: u64,
    pub state: LoopState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopFire {
    pub loop_id: String,
    pub session_id: String,
    pub prompt: String,
    pub notice: Release4Notification,
}

pub struct Release4SessionRemoval {
    session_id: String,
    pickers: BTreeMap<String, ProjectPicker>,
    teleports: BTreeMap<String, TeleportOperation>,
    loops: BTreeMap<String, ScheduledLoop>,
}

impl Release4SessionRemoval {
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
pub struct Release4Service {
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
    next_operation: Arc<AtomicU64>,
    next_loop: Arc<AtomicU64>,
}

impl Default for Release4Service {
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
            next_operation: Arc::new(AtomicU64::new(1)),
            next_loop: Arc::new(AtomicU64::new(next_loop)),
        }
    }
}

impl Release4Service {
    pub fn production(config: VibeCodeCloudConfig) -> Result<Self, Release4BuildError> {
        let cloud = Arc::new(VibeCodeHttpCloud::new(config)?);
        let service = Self {
            project_cloud: ProjectCloudBackend::Async(cloud.clone()),
            teleport_cloud: TeleportCloudBackend::Async(cloud),
            git: Arc::new(CommandGitProbe::default()),
            ..Self::default()
        };
        service
            .with_project_link_store(default_project_link_store())
            .map_err(Release4BuildError::Release4)
    }

    pub fn close_transient_session(&self, session_id: &str) -> Result<(), Release4Error> {
        self.lock_projects()?
            .pickers
            .retain(|_, picker| picker.session_id != session_id);
        self.lock_teleports()?
            .retain(|_, operation| operation.session_id != session_id);
        Ok(())
    }

    pub fn remove_session(&self, session_id: &str) -> Result<usize, Release4Error> {
        let removal = self.remove_session_transactional(session_id)?;
        Ok(removal.removed_loop_count())
    }

    pub fn remove_session_transactional(
        &self,
        session_id: &str,
    ) -> Result<Release4SessionRemoval, Release4Error> {
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
        Ok(Release4SessionRemoval {
            session_id: session_id.to_owned(),
            pickers: removed_pickers,
            teleports: removed_teleports,
            loops: removed_loops,
        })
    }

    pub fn restore_session(&self, removal: &Release4SessionRemoval) -> Result<(), Release4Error> {
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
            return Err(Release4Error::Conflict(
                "release-4 session rollback collides with newer session state".to_owned(),
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
    ) -> Result<(), Release4Error> {
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
            ..Self::default()
        }
    }

    pub fn with_project_link_store(mut self, path: PathBuf) -> Result<Self, Release4Error> {
        let linked_projects = load_project_links(&path)?;
        self.projects = Arc::new(Mutex::new(ProjectState {
            pickers: BTreeMap::new(),
            linked_projects,
        }));
        self.project_link_store = Some(path);
        self.project_link_store_error = None;
        Ok(self)
    }

    async fn project_list_cloud(
        &self,
        cursor: Option<String>,
    ) -> Result<ProjectPage, Release4Error> {
        match self.project_cloud.clone() {
            ProjectCloudBackend::Sync(cloud) => tokio::task::spawn_blocking(move || {
                cloud.list(cursor.as_deref()).map_err(Release4Error::Cloud)
            })
            .await
            .map_err(|_| Release4Error::BackgroundTask)?,
            ProjectCloudBackend::Async(cloud) => cloud
                .list(cursor.as_deref())
                .await
                .map_err(Release4Error::Cloud),
        }
    }

    async fn project_list_all(&self) -> Result<ProjectPage, Release4Error> {
        let mut projects = Vec::new();
        let mut cursor = None;
        let mut seen_cursors = BTreeSet::new();
        let mut pages_loaded = 0_usize;
        loop {
            if pages_loaded >= MAX_HEADLESS_PROJECT_PAGES {
                return Err(Release4Error::Conflict(format!(
                    "Vibe Code project pagination exceeded {MAX_HEADLESS_PROJECT_PAGES} pages"
                )));
            }
            let page = self.project_list_cloud(cursor).await?;
            pages_loaded += 1;
            if projects.len().saturating_add(page.projects.len()) > MAX_HEADLESS_PROJECTS {
                return Err(Release4Error::Conflict(format!(
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
                return Err(Release4Error::Conflict(
                    "Vibe Code project pagination repeated a cursor".to_owned(),
                ));
            }
            cursor = Some(next_cursor);
        }
    }

    async fn project_create_cloud(
        &self,
        name: String,
        repo_url: String,
        default_branch: String,
    ) -> Result<Project, Release4Error> {
        match self.project_cloud.clone() {
            ProjectCloudBackend::Sync(cloud) => tokio::task::spawn_blocking(move || {
                cloud
                    .create(&name, &repo_url, &default_branch)
                    .map_err(Release4Error::Cloud)
            })
            .await
            .map_err(|_| Release4Error::BackgroundTask)?,
            ProjectCloudBackend::Async(cloud) => cloud
                .create(&name, &repo_url, &default_branch)
                .await
                .map_err(Release4Error::Cloud),
        }
    }

    async fn teleport_start_cloud(
        &self,
        request: TeleportStartRequest,
    ) -> Result<String, CloudError> {
        match self.teleport_cloud.clone() {
            TeleportCloudBackend::Sync(cloud) => {
                tokio::task::spawn_blocking(move || cloud.start(&request))
                    .await
                    .map_err(|_| {
                        CloudError::Unavailable(
                            "Teleport background task stopped unexpectedly".to_owned(),
                        )
                    })?
            }
            TeleportCloudBackend::Async(cloud) => cloud.start(&request).await,
        }
    }

    async fn inspect_git(
        &self,
        working_directory: PathBuf,
    ) -> Result<(GitSnapshot, TeleportRepository, GitPushStatus), CloudError> {
        let git = self.git.clone();
        tokio::task::spawn_blocking(move || git.inspect_for_teleport(&working_directory))
            .await
            .map_err(|_| {
                CloudError::Git("Git inspection background task stopped unexpectedly".to_owned())
            })?
    }

    async fn push_git(&self, working_directory: PathBuf) -> Result<(), CloudError> {
        let git = self.git.clone();
        tokio::task::spawn_blocking(move || git.push(&working_directory))
            .await
            .map_err(|_| {
                CloudError::Git("Git push background task stopped unexpectedly".to_owned())
            })?
    }

    fn install_project_picker(
        &self,
        session_id: String,
        git: ProjectGitSnapshot,
        page: ProjectPage,
    ) -> Result<Release4Dispatch, Release4Error> {
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
        Ok(Release4Dispatch::result([
            ("pickerId", json!(picker_id)),
            ("view", view),
            ("resolvedProjectId", json!(selected)),
        ]))
    }

    fn has_matching_saved_project_link(
        &self,
        git: &ProjectGitSnapshot,
    ) -> Result<bool, Release4Error> {
        let state = self.lock_projects()?;
        Ok(state
            .linked_projects
            .get(&git.repo_root)
            .is_some_and(|link| {
                normalize_repo_url(&link.repo_url) == normalize_repo_url(&git.snapshot.repository)
            }))
    }

    async fn finish_headless_project_open(
        &self,
        session_id: &str,
        mut opened: Release4Dispatch,
        project_name: String,
        default_branch: Option<String>,
    ) -> Result<Release4Dispatch, Release4Error> {
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
                Release4Error::Conflict("project picker omitted its identifier".to_owned())
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

    fn single_headless_project_match(
        &self,
        session_id: &str,
        picker_id: &str,
    ) -> Result<Option<String>, Release4Error> {
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

    async fn project_open(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let session_id = required_string(params, "sessionId")?.to_owned();
        let purpose = project_picker_purpose(params)?;
        let working_directory =
            optional_string(params, "workingDirectory")?.unwrap_or_else(|| ".".into());
        let git_working_directory = PathBuf::from(&working_directory);
        let git = self.git.clone();
        let git_task =
            tokio::task::spawn_blocking(move || git.inspect_project(&git_working_directory));
        let git = git_task
            .await
            .map_err(|_| Release4Error::BackgroundTask)?
            .map_err(Release4Error::Cloud)?;
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

    async fn project_create(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
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
                return Err(Release4Error::Conflict(
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
        Ok(Release4Dispatch::result([
            ("view", view),
            ("project", serde_json::to_value(project)?),
        ]))
    }

    async fn project_load_more(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let session_id = required_string(params, "sessionId")?.to_owned();
        let picker_id = required_string(params, "pickerId")?.to_owned();
        let requested_cursor = {
            let state = self.lock_projects()?;
            picker(&state, &picker_id, &session_id)?.next_cursor.clone()
        };
        let Some(requested_cursor) = requested_cursor else {
            let state = self.lock_projects()?;
            let picker = picker(&state, &picker_id, &session_id)?;
            return Ok(Release4Dispatch::result([
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
                return Err(Release4Error::Conflict(
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
            return Err(Release4Error::Conflict(
                "project picker changed while loading the next page".to_owned(),
            ));
        }
        for page in pages {
            for project in page.projects {
                picker.projects.insert(project.project_id.clone(), project);
            }
            picker.next_cursor = page.next_cursor;
        }
        Ok(Release4Dispatch::result([
            ("view", project_view(picker)),
            (
                "focusOptionId",
                focus.map_or(Value::Null, |id| json!(format!("project:{id}"))),
            ),
        ]))
    }

    async fn teleport_start(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let session_id = required_string(params, "sessionId")?.to_owned();
        let operation_id = required_string(params, "operationId")?.to_owned();
        let picker_id = required_string(params, "pickerId")?;
        let project_id = required_string(params, "projectId")?.to_owned();
        {
            let state = self.lock_projects()?;
            let picker = picker(&state, picker_id, &session_id)?;
            if !picker.projects.contains_key(&project_id) {
                return Err(Release4Error::NotFound(format!(
                    "project `{project_id}` is not available in picker `{picker_id}`"
                )));
            }
            if picker.selected.as_deref() != Some(&project_id) {
                return Err(Release4Error::Conflict(format!(
                    "project `{project_id}` is not selected in picker `{picker_id}`"
                )));
            }
        }
        let summary = optional_string(params, "prompt")?
            .filter(|prompt| !prompt.trim().is_empty())
            .unwrap_or_else(|| "Continue this session in Vibe Code".to_owned());
        validate_cloud_text(&summary, "Teleport message").map_err(Release4Error::Cloud)?;
        let working_directory = PathBuf::from(
            optional_string(params, "workingDirectory")?.unwrap_or_else(|| ".".to_owned()),
        );
        let mut operation = TeleportOperation {
            id: operation_id.clone(),
            session_id: session_id.clone(),
            project_id,
            working_directory: working_directory.clone(),
            summary,
            repository: TeleportRepository {
                repo_url: String::new(),
                branch: None,
                commit_sha: None,
                diff: None,
            },
            state: TeleportState::SummarizingContext,
            push_response: None,
            unpushed_count: 0,
            branch_not_pushed: false,
            url: None,
            error: None,
        };
        let mut notifications = vec![teleport_notification(&operation)];
        operation.state = TeleportState::CheckingGit;
        notifications.push(teleport_notification(&operation));
        {
            let mut teleports = self.lock_teleports()?;
            if teleports.contains_key(&operation_id) {
                return Err(Release4Error::Conflict(format!(
                    "Teleport operation `{operation_id}` already exists"
                )));
            }
            teleports.insert(operation_id.clone(), operation);
        }

        let inspection = self.inspect_git(working_directory).await;
        let cloud_request = {
            let mut teleports = self.lock_teleports()?;
            let operation = teleports.get_mut(&operation_id).ok_or_else(|| {
                Release4Error::NotFound(format!(
                    "Teleport operation `{operation_id}` was not found"
                ))
            })?;
            if operation.state == TeleportState::Cancelled {
                return Ok(Release4Dispatch::with_notifications(
                    [("operationId", json!(operation_id))],
                    notifications,
                ));
            }
            let (snapshot, repository, push_status) = match inspection {
                Ok(inspection) => inspection,
                Err(error) => {
                    operation.state = TeleportState::Failed;
                    operation.error = Some(error.to_string());
                    notifications.push(teleport_notification(operation));
                    return Ok(Release4Dispatch::with_notifications(
                        [("operationId", json!(operation_id))],
                        notifications,
                    ));
                }
            };
            operation.repository = repository;
            if snapshot.unpushed {
                operation.state = TeleportState::PushRequired;
                operation.unpushed_count = push_status.unpushed_count;
                operation.branch_not_pushed = push_status.branch_not_pushed;
                notifications.push(teleport_notification(operation));
                return Ok(Release4Dispatch::with_notifications(
                    [("operationId", json!(operation_id))],
                    notifications,
                ));
            }
            operation.state = TeleportState::StartingWorkflow;
            teleport_start_request(operation)
        };
        let result = self.teleport_start_cloud(cloud_request).await;
        let mut teleports = self.lock_teleports()?;
        let operation = teleports.get_mut(&operation_id).ok_or_else(|| {
            Release4Error::NotFound(format!("Teleport operation `{operation_id}` was not found"))
        })?;
        if operation.state == TeleportState::Cancelled {
            return Ok(Release4Dispatch::with_notifications(
                [("operationId", json!(operation_id))],
                notifications,
            ));
        }
        match result {
            Ok(url) => {
                operation.state = TeleportState::StartingWorkflow;
                notifications.push(teleport_notification(operation));
                operation.url = Some(url);
                operation.state = TeleportState::Complete;
                notifications.push(teleport_notification(operation));
            }
            Err(error) => {
                operation.error = Some(error.to_string());
                operation.state = TeleportState::Failed;
                notifications.push(teleport_notification(operation));
            }
        }
        Ok(Release4Dispatch::with_notifications(
            [("operationId", json!(operation_id))],
            notifications,
        ))
    }

    async fn teleport_push_respond(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let session_id = required_string(params, "sessionId")?.to_owned();
        let operation_id = required_string(params, "operationId")?.to_owned();
        let accepted = required_bool(params, "approved")?;
        let mut notifications = Vec::new();
        let working_directory = {
            let mut teleports = self.lock_teleports()?;
            let operation = teleports.get_mut(&operation_id).ok_or_else(|| {
                Release4Error::NotFound(format!(
                    "Teleport operation `{operation_id}` was not found"
                ))
            })?;
            if operation.session_id != session_id {
                return Err(Release4Error::NotFound(format!(
                    "Teleport operation `{operation_id}` is not owned by session `{session_id}`"
                )));
            }
            if let Some(previous) = operation.push_response {
                if previous == accepted {
                    return Ok(Release4Dispatch::result([] as [(&str, Value); 0]));
                }
                return Err(Release4Error::Conflict(
                    "Teleport push response conflicts with the recorded answer".to_owned(),
                ));
            }
            if operation.state != TeleportState::PushRequired {
                return Err(Release4Error::Conflict(
                    "Teleport operation is not waiting for a push response".to_owned(),
                ));
            }
            operation.push_response = Some(accepted);
            if !accepted {
                operation.state = TeleportState::Failed;
                operation.error = Some(
                    "Git push was denied; the local session and working tree were not changed"
                        .to_owned(),
                );
                notifications.push(teleport_notification(operation));
                return Ok(Release4Dispatch::with_notifications(
                    [] as [(&str, Value); 0],
                    notifications,
                ));
            }
            operation.state = TeleportState::Pushing;
            notifications.push(teleport_notification(operation));
            operation.working_directory.clone()
        };
        if let Err(error) = self.push_git(working_directory).await {
            let mut teleports = self.lock_teleports()?;
            let operation = teleports.get_mut(&operation_id).ok_or_else(|| {
                Release4Error::NotFound(format!(
                    "Teleport operation `{operation_id}` was not found"
                ))
            })?;
            if operation.state != TeleportState::Cancelled {
                operation.state = TeleportState::Failed;
                operation.error = Some(error.to_string());
                notifications.push(teleport_notification(operation));
            }
            return Ok(Release4Dispatch::with_notifications(
                [] as [(&str, Value); 0],
                notifications,
            ));
        }
        let cloud_request = {
            let mut teleports = self.lock_teleports()?;
            let operation = teleports.get_mut(&operation_id).ok_or_else(|| {
                Release4Error::NotFound(format!(
                    "Teleport operation `{operation_id}` was not found"
                ))
            })?;
            if operation.state == TeleportState::Cancelled {
                return Ok(Release4Dispatch::with_notifications(
                    [] as [(&str, Value); 0],
                    notifications,
                ));
            }
            operation.state = TeleportState::StartingWorkflow;
            teleport_start_request(operation)
        };
        let result = self.teleport_start_cloud(cloud_request).await;
        let mut teleports = self.lock_teleports()?;
        let operation = teleports.get_mut(&operation_id).ok_or_else(|| {
            Release4Error::NotFound(format!("Teleport operation `{operation_id}` was not found"))
        })?;
        if operation.state != TeleportState::Cancelled {
            match result {
                Ok(url) => {
                    operation.state = TeleportState::StartingWorkflow;
                    notifications.push(teleport_notification(operation));
                    operation.url = Some(url);
                    operation.state = TeleportState::Complete;
                    notifications.push(teleport_notification(operation));
                }
                Err(error) => {
                    operation.error = Some(error.to_string());
                    operation.state = TeleportState::Failed;
                    notifications.push(teleport_notification(operation));
                }
            }
        }
        Ok(Release4Dispatch::with_notifications(
            [] as [(&str, Value); 0],
            notifications,
        ))
    }

    pub fn with_loop_store(mut self, path: PathBuf) -> Result<Self, Release4Error> {
        let mut loops = load_loops(&path)?;
        for scheduled in loops.values_mut() {
            if scheduled.state == LoopState::Running {
                scheduled.state = LoopState::Scheduled;
            }
        }
        let next = next_loop_sequence(&loops);
        self.loops = Arc::new(Mutex::new(loops));
        self.loop_store = path;
        self.loop_store_error = None;
        self.next_loop = Arc::new(AtomicU64::new(next));
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
    ) -> Result<Release4Dispatch, Release4Error> {
        if DEFERRED_RELEASE4_METHODS.contains(&method) {
            return Err(Release4Error::Conflict(format!(
                "`{method}` reaches Vibe Code and must be dispatched asynchronously"
            )));
        }
        match method {
            "vibeCode/projects/recover" => self.project_recover(params),
            "vibeCode/projects/select" => self.project_select(params),
            "vibeCode/projects/unlink" => self.project_unlink(params),
            "vibeCode/projects/cancel" => self.project_cancel(params),
            "vibeCode/teleport/cancel" => self.teleport_cancel(params),
            "loops/create" => self.loop_create(params),
            "loops/list" => self.loop_list(params),
            "loops/clear" => self.loop_clear(params),
            "loops/delete" => self.loop_delete(params),
            _ => Err(Release4Error::MethodNotFound(method.to_owned())),
        }
    }

    #[must_use]
    pub fn requires_deferred_dispatch(&self, method: &str) -> bool {
        DEFERRED_RELEASE4_METHODS.contains(&method)
    }

    pub async fn dispatch_deferred(
        &self,
        method: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        match method {
            "vibeCode/projects/create" => self.project_create(params).await,
            "vibeCode/projects/loadMore" => self.project_load_more(params).await,
            "vibeCode/projects/open" => self.project_open(params).await,
            "vibeCode/teleport/start" => self.teleport_start(params).await,
            "vibeCode/teleport/push/respond" => self.teleport_push_respond(params).await,
            _ => self.dispatch(method, params),
        }
    }

    pub fn fire_loop(
        &self,
        loop_id: &str,
        now_seconds: u64,
        session_idle: bool,
    ) -> Result<LoopFire, Release4Error> {
        self.fire_loop_owned(loop_id, None, now_seconds, session_idle)
    }

    pub fn fire_loop_for_session(
        &self,
        loop_id: &str,
        session_id: &str,
        now_seconds: u64,
        session_idle: bool,
    ) -> Result<LoopFire, Release4Error> {
        self.fire_loop_owned(loop_id, Some(session_id), now_seconds, session_idle)
    }

    fn fire_loop_owned(
        &self,
        loop_id: &str,
        session_id: Option<&str>,
        now_seconds: u64,
        session_idle: bool,
    ) -> Result<LoopFire, Release4Error> {
        self.ensure_loop_store_ready()?;
        if !session_idle {
            return Err(Release4Error::Conflict(
                "scheduled loop cannot fire while its session has active work".to_owned(),
            ));
        }
        let mut loops = self.lock_loops()?;
        let before = loops.clone();
        let scheduled = loops
            .get_mut(loop_id)
            .ok_or_else(|| Release4Error::NotFound(format!("loop `{loop_id}` was not found")))?;
        if session_id.is_some_and(|session_id| scheduled.session_id != session_id) {
            return Err(Release4Error::NotFound(format!(
                "loop `{loop_id}` is not owned by session `{}`",
                session_id.unwrap_or_default()
            )));
        }
        if scheduled.state == LoopState::Running {
            return Err(Release4Error::Conflict(format!(
                "loop `{loop_id}` is already running"
            )));
        }
        if now_seconds < scheduled.next_fire_at {
            return Err(Release4Error::Conflict(format!(
                "loop `{loop_id}` is not due"
            )));
        }
        scheduled.state = LoopState::Running;
        scheduled.next_fire_at = now_seconds.saturating_add(scheduled.interval_seconds);
        let fire = LoopFire {
            loop_id: scheduled.id.clone(),
            session_id: scheduled.session_id.clone(),
            prompt: scheduled.prompt.clone(),
            notice: notification(
                "history/entryAdded",
                [(
                    "entry",
                    json!({
                        "id": format!("scheduled-loop:{loop_id}"),
                        "sessionId": scheduled.session_id,
                        "turnId": Value::Null,
                        "createdAt": now_seconds.saturating_mul(1_000),
                        "updatedAt": now_seconds.saturating_mul(1_000),
                        "generationStatus": "completed",
                        "relatedEntryId": Value::Null,
                        "type": "notice",
                        "level": "info",
                        "message": format!("Loop `{loop_id}` fired"),
                        "detail": {
                            "kind": "scheduled_loop_fired",
                            "loopId": loop_id,
                        },
                    }),
                )],
            ),
        };
        if let Err(error) = self.persist_loops(&loops) {
            *loops = before;
            return Err(error);
        }
        Ok(fire)
    }

    pub fn next_due_loop_id(
        &self,
        session_id: &str,
        now_seconds: u64,
    ) -> Result<Option<String>, Release4Error> {
        self.ensure_loop_store_ready()?;
        let loops = self.lock_loops()?;
        Ok(loops
            .values()
            .filter(|scheduled| {
                scheduled.session_id == session_id
                    && scheduled.state == LoopState::Scheduled
                    && scheduled.next_fire_at <= now_seconds
            })
            .min_by_key(|scheduled| (scheduled.next_fire_at, &scheduled.id))
            .map(|scheduled| scheduled.id.clone()))
    }

    pub fn finish_loop_fire(
        &self,
        loop_id: &str,
        _completed_at_seconds: u64,
    ) -> Result<(), Release4Error> {
        self.ensure_loop_store_ready()?;
        let mut loops = self.lock_loops()?;
        let before = loops.clone();
        let scheduled = loops
            .get_mut(loop_id)
            .ok_or_else(|| Release4Error::NotFound(format!("loop `{loop_id}` was not found")))?;
        if scheduled.state != LoopState::Running {
            return Err(Release4Error::Conflict(format!(
                "loop `{loop_id}` is not running"
            )));
        }
        scheduled.state = LoopState::Scheduled;
        if let Err(error) = self.persist_loops(&loops) {
            *loops = before;
            return Err(error);
        }
        Ok(())
    }

    fn project_recover(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
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
        Ok(Release4Dispatch::result([
            ("recovered", json!(selected.is_some())),
            ("view", project_view(picker)),
        ]))
    }

    fn project_select(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let session_id = required_string(params, "sessionId")?;
        let picker_id = required_string(params, "pickerId")?;
        let project_id = required_string(params, "projectId")?;
        let mut state = self.lock_projects()?;
        let before = state.clone();
        let (project, repo_root, link, view) = {
            let picker = picker_mut(&mut state, picker_id, session_id)?;
            let project = picker.projects.get(project_id).cloned().ok_or_else(|| {
                Release4Error::NotFound(format!(
                    "project `{project_id}` is not available in picker `{picker_id}`"
                ))
            })?;
            if project.is_read_only {
                return Err(Release4Error::InvalidParams(format!(
                    "project `{project_id}` is read-only and cannot be selected"
                )));
            }
            if !is_project_linked_to_repo(&project, &picker.repo_url) {
                return Err(Release4Error::InvalidParams(format!(
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
        Ok(Release4Dispatch::result([
            ("view", view),
            ("project", serde_json::to_value(project)?),
        ]))
    }

    fn project_unlink(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
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
        Ok(Release4Dispatch::result([("view", view)]))
    }

    fn project_cancel(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let session_id = required_string(params, "sessionId")?;
        let picker_id = required_string(params, "pickerId")?;
        let mut state = self.lock_projects()?;
        let current = state.pickers.get(picker_id).ok_or_else(|| {
            Release4Error::NotFound(format!("picker `{picker_id}` was not found"))
        })?;
        if current.session_id != session_id {
            return Err(Release4Error::NotFound(format!(
                "picker `{picker_id}` is not owned by session `{session_id}`"
            )));
        }
        state.pickers.remove(picker_id);
        Ok(Release4Dispatch::result([] as [(&str, Value); 0]))
    }

    fn teleport_cancel(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        let session_id = required_string(params, "sessionId")?;
        let operation_id = required_string(params, "operationId")?;
        let mut teleports = self.lock_teleports()?;
        let operation = teleports.get_mut(operation_id).ok_or_else(|| {
            Release4Error::NotFound(format!("Teleport operation `{operation_id}` was not found"))
        })?;
        if operation.session_id != session_id {
            return Err(Release4Error::NotFound(format!(
                "Teleport operation `{operation_id}` is not owned by session `{session_id}`"
            )));
        }
        match operation.state {
            TeleportState::Complete | TeleportState::Failed => {
                return Err(Release4Error::Conflict(
                    "Teleport operation is already terminal".to_owned(),
                ));
            }
            TeleportState::Pushing | TeleportState::StartingWorkflow => {
                return Err(Release4Error::Conflict(
                    "Teleport operation is already performing irreversible work".to_owned(),
                ));
            }
            TeleportState::Cancelled => {}
            _ => {
                operation.state = TeleportState::Cancelled;
            }
        }
        Ok(Release4Dispatch::with_notifications(
            [("cancelled", json!(true))],
            Vec::new(),
        ))
    }

    fn loop_create(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        self.ensure_loop_store_ready()?;
        let session_id = required_string(params, "sessionId")?;
        let prompt = required_string(params, "prompt")?;
        if prompt.starts_with('/') {
            return Err(Release4Error::InvalidParams(
                "scheduled-loop prompts cannot start with `/`".to_owned(),
            ));
        }
        let interval_seconds = parse_interval(required_string(params, "interval")?)?;
        let now_seconds = optional_u64(params, "nowSeconds")?.unwrap_or_else(now_seconds);
        if interval_seconds < MIN_LOOP_INTERVAL_SECONDS {
            return Err(Release4Error::InvalidParams(format!(
                "intervalSeconds must be at least {MIN_LOOP_INTERVAL_SECONDS}"
            )));
        }
        let mut loops = self.lock_loops()?;
        if loops
            .values()
            .filter(|scheduled| scheduled.session_id == session_id)
            .count()
            >= MAX_LOOPS_PER_SESSION
        {
            return Err(Release4Error::Conflict(format!(
                "session `{session_id}` already owns {MAX_LOOPS_PER_SESSION} scheduled loops"
            )));
        }
        let before = loops.clone();
        let id = format!(
            "{:08x}",
            self.next_loop.fetch_add(1, Ordering::Relaxed) & u64::from(u32::MAX)
        );
        let scheduled = ScheduledLoop {
            id: id.clone(),
            session_id: session_id.to_owned(),
            prompt: prompt.to_owned(),
            interval_seconds,
            next_fire_at: now_seconds.saturating_add(interval_seconds),
            state: LoopState::Scheduled,
        };
        loops.insert(id, scheduled.clone());
        if let Err(error) = self.persist_loops(&loops) {
            *loops = before;
            return Err(error);
        }
        Ok(Release4Dispatch::result([(
            "loop",
            public_loop_value(&scheduled),
        )]))
    }

    fn loop_list(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        self.ensure_loop_store_ready()?;
        let session_id = required_string(params, "sessionId")?;
        let loops = self.lock_loops()?;
        let items = loops
            .values()
            .filter(|scheduled| scheduled.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        Ok(Release4Dispatch::result([(
            "loops",
            Value::Array(items.iter().map(public_loop_value).collect()),
        )]))
    }

    fn loop_clear(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        self.ensure_loop_store_ready()?;
        let session_id = required_string(params, "sessionId")?;
        let mut loops = self.lock_loops()?;
        let before = loops.clone();
        let removed = loops
            .values()
            .filter(|scheduled| scheduled.session_id == session_id)
            .count();
        loops.retain(|_, scheduled| scheduled.session_id != session_id);
        if let Err(error) = self.persist_loops(&loops) {
            *loops = before;
            return Err(error);
        }
        Ok(Release4Dispatch::result([("count", json!(removed))]))
    }

    fn loop_delete(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release4Dispatch, Release4Error> {
        self.ensure_loop_store_ready()?;
        let session_id = required_string(params, "sessionId")?;
        let loop_id = required_string(params, "loopId")?;
        let mut loops = self.lock_loops()?;
        let before = loops.clone();
        let scheduled = loops
            .get(loop_id)
            .ok_or_else(|| Release4Error::NotFound(format!("loop `{loop_id}` was not found")))?;
        if scheduled.session_id != session_id {
            return Err(Release4Error::NotFound(format!(
                "loop `{loop_id}` is not owned by session `{session_id}`"
            )));
        }
        if scheduled.state == LoopState::Running {
            return Err(Release4Error::Conflict(format!(
                "loop `{loop_id}` cannot be deleted while running"
            )));
        }
        let removed = loops
            .remove(loop_id)
            .ok_or_else(|| Release4Error::NotFound(loop_id.to_owned()))?;
        if let Err(error) = self.persist_loops(&loops) {
            *loops = before;
            return Err(error);
        }
        Ok(Release4Dispatch::result([(
            "loop",
            public_loop_value(&removed),
        )]))
    }

    fn next_operation_id(&self, prefix: &str) -> String {
        format!(
            "{prefix}-operation-{}",
            self.next_operation.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn lock_projects(&self) -> Result<std::sync::MutexGuard<'_, ProjectState>, Release4Error> {
        self.projects
            .lock()
            .map_err(|_| Release4Error::StatePoisoned)
    }

    fn lock_teleports(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, TeleportOperation>>, Release4Error> {
        self.teleports
            .lock()
            .map_err(|_| Release4Error::StatePoisoned)
    }

    fn lock_loops(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, ScheduledLoop>>, Release4Error> {
        self.loops.lock().map_err(|_| Release4Error::StatePoisoned)
    }

    fn persist_loops(&self, loops: &BTreeMap<String, ScheduledLoop>) -> Result<(), Release4Error> {
        persist_json_atomically(&self.loop_store, loops, &NEXT_LOOP_TEMP_FILE)
            .map_err(Release4Error::Persistence)
    }

    fn persist_project_links(
        &self,
        links: &BTreeMap<String, SavedProjectLink>,
    ) -> Result<(), Release4Error> {
        let Some(path) = &self.project_link_store else {
            return Ok(());
        };
        if let Some(error) = &self.project_link_store_error {
            return Err(Release4Error::ProjectLinkPersistenceState(error.clone()));
        }
        persist_json_atomically(path, links, &NEXT_LINK_TEMP_FILE)
            .map_err(Release4Error::ProjectLinkPersistence)
    }

    fn ensure_loop_store_ready(&self) -> Result<(), Release4Error> {
        self.loop_store_error.as_ref().map_or(Ok(()), |error| {
            Err(Release4Error::PersistenceState(error.clone()))
        })
    }
}

fn picker<'a>(
    state: &'a ProjectState,
    picker_id: &str,
    session_id: &str,
) -> Result<&'a ProjectPicker, Release4Error> {
    let picker = state
        .pickers
        .get(picker_id)
        .ok_or_else(|| Release4Error::NotFound(format!("picker `{picker_id}` was not found")))?;
    if picker.session_id != session_id {
        return Err(Release4Error::NotFound(format!(
            "picker `{picker_id}` is not owned by session `{session_id}`"
        )));
    }
    Ok(picker)
}

fn picker_mut<'a>(
    state: &'a mut ProjectState,
    picker_id: &str,
    session_id: &str,
) -> Result<&'a mut ProjectPicker, Release4Error> {
    let picker = state
        .pickers
        .get_mut(picker_id)
        .ok_or_else(|| Release4Error::NotFound(format!("picker `{picker_id}` was not found")))?;
    if picker.session_id != session_id {
        return Err(Release4Error::NotFound(format!(
            "picker `{picker_id}` is not owned by session `{session_id}`"
        )));
    }
    Ok(picker)
}

fn project_view(picker: &ProjectPicker) -> Value {
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

fn finish_headless_project_open(
    opened: &mut Release4Dispatch,
    action: Release4Dispatch,
) -> Result<(), Release4Error> {
    let project_id = action
        .result
        .get("project")
        .and_then(|project| project.get("projectId"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Release4Error::Conflict(
                "headless project resolution omitted the resolved project".to_owned(),
            )
        })?;
    let view = action.result.get("view").cloned().ok_or_else(|| {
        Release4Error::Conflict(
            "headless project resolution omitted the project picker view".to_owned(),
        )
    })?;
    opened.result.insert("view".to_owned(), view);
    opened
        .result
        .insert("resolvedProjectId".to_owned(), json!(project_id));
    Ok(())
}

fn headless_default_branch(branch: Option<String>) -> Result<String, Release4Error> {
    branch
        .map(|branch| branch.trim().to_owned())
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| {
            Release4Error::Cloud(CloudError::Git(
                "Teleport requires a checked-out branch before creating a Vibe Code project"
                    .to_owned(),
            ))
        })
}

fn teleport_start_request(operation: &TeleportOperation) -> TeleportStartRequest {
    TeleportStartRequest {
        project_id: operation.project_id.clone(),
        idempotency_key: operation.id.clone(),
        summary: operation.summary.clone(),
        repository: operation.repository.clone(),
    }
}

fn teleport_notification(operation: &TeleportOperation) -> Release4Notification {
    let event = match operation.state {
        TeleportState::SummarizingContext => {
            json!({"kind": "summarizing_context", "operationId": operation.id})
        }
        TeleportState::CheckingGit => {
            json!({"kind": "checking_git", "operationId": operation.id})
        }
        TeleportState::PushRequired => json!({
            "kind": "push_required",
            "operationId": operation.id,
            "unpushedCount": operation.unpushed_count,
            "branchNotPushed": operation.branch_not_pushed,
        }),
        TeleportState::Pushing => json!({"kind": "pushing", "operationId": operation.id}),
        TeleportState::StartingWorkflow => {
            json!({"kind": "starting_workflow", "operationId": operation.id})
        }
        TeleportState::Complete => json!({
            "kind": "complete",
            "operationId": operation.id,
            "url": operation.url,
        }),
        TeleportState::Failed => json!({
            "kind": "failed",
            "operationId": operation.id,
            "error": {
                "message": operation.error,
                "code": "teleport_failed",
                "details": Value::Null,
            },
        }),
        TeleportState::Cancelled => {
            json!({"kind": "cancelled", "operationId": operation.id})
        }
    };
    notification("vibeCode/teleport/event", [("event", event)])
}

fn public_loop_value(scheduled: &ScheduledLoop) -> Value {
    json!({
        "id": scheduled.id,
        "prompt": scheduled.prompt,
        "intervalSeconds": scheduled.interval_seconds,
        "nextFireAt": scheduled.next_fire_at as f64,
    })
}

fn parse_interval(value: &str) -> Result<u64, Release4Error> {
    let mut normalized = value.trim().to_ascii_lowercase();
    let unit = normalized.pop().ok_or_else(|| {
        Release4Error::InvalidParams("interval must use `<digits><s|m|h|d>` syntax".to_owned())
    })?;
    let digits = normalized;
    let amount = digits.parse::<u64>().map_err(|_| {
        Release4Error::InvalidParams("interval must use `<digits><s|m|h|d>` syntax".to_owned())
    })?;
    let multiplier = match unit {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        _ => {
            return Err(Release4Error::InvalidParams(
                "interval must use `<digits><s|m|h|d>` syntax".to_owned(),
            ));
        }
    };
    amount.checked_mul(multiplier).ok_or_else(|| {
        Release4Error::InvalidParams("interval exceeds the supported range".to_owned())
    })
}

fn notification<const N: usize>(method: &str, entries: [(&str, Value); N]) -> Release4Notification {
    Release4Notification {
        method: method.to_owned(),
        params: entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    }
}

fn invalid_params(error: params::ParamError) -> Release4Error {
    Release4Error::InvalidParams(error.message())
}

fn required_string<'a>(
    values: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a str, Release4Error> {
    params::required_string(values, key).map_err(invalid_params)
}

fn optional_string(
    values: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<String>, Release4Error> {
    params::optional_string(values, key)
        .map(|value| value.map(ToOwned::to_owned))
        .map_err(invalid_params)
}

fn project_picker_purpose(
    params: &BTreeMap<String, Value>,
) -> Result<ProjectPickerPurpose, Release4Error> {
    match optional_string(params, "purpose")?.as_deref() {
        None | Some("configure") => Ok(ProjectPickerPurpose::Configure),
        Some("teleport") => Ok(ProjectPickerPurpose::Teleport),
        Some(purpose) => Err(Release4Error::InvalidParams(format!(
            "purpose must be `configure` or `teleport`, got `{purpose}`"
        ))),
    }
}

fn required_bool(values: &BTreeMap<String, Value>, key: &str) -> Result<bool, Release4Error> {
    params::required_bool(values, key).map_err(invalid_params)
}

fn optional_u64(
    values: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<u64>, Release4Error> {
    params::optional_u64(values, key).map_err(invalid_params)
}

fn load_loops(path: &Path) -> Result<BTreeMap<String, ScheduledLoop>, Release4Error> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents).map_err(Release4Error::Json),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(Release4Error::Persistence(error)),
    }
}

fn load_project_links(path: &Path) -> Result<BTreeMap<String, SavedProjectLink>, Release4Error> {
    match fs::read(path) {
        Ok(contents) => {
            let stored = serde_json::from_slice::<BTreeMap<String, StoredProjectLink>>(&contents)
                .map_err(Release4Error::Json)?;
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
        Err(error) => Err(Release4Error::ProjectLinkPersistence(error)),
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

fn next_loop_sequence(loops: &BTreeMap<String, ScheduledLoop>) -> u64 {
    loops
        .keys()
        .filter_map(|id| {
            u64::from_str_radix(id, 16).ok().or_else(|| {
                id.strip_prefix("loop-")
                    .and_then(|suffix| suffix.parse::<u64>().ok())
            })
        })
        .max()
        .unwrap_or_default()
        .saturating_add(1)
}

fn default_loop_store() -> PathBuf {
    host::vibe_home().join("scheduled-loops.json")
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

#[derive(Debug, Error)]
pub enum Release4Error {
    #[error("unknown release-4 method `{0}`")]
    MethodNotFound(String),
    #[error("invalid parameters: {0}")]
    InvalidParams(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Cloud(CloudError),
    #[error("scheduled-loop persistence failed: {0}")]
    Persistence(std::io::Error),
    #[error("scheduled-loop persistence is unavailable: {0}")]
    PersistenceState(String),
    #[error("Vibe Code project-link persistence failed: {0}")]
    ProjectLinkPersistence(std::io::Error),
    #[error("Vibe Code project-link persistence is unavailable: {0}")]
    ProjectLinkPersistenceState(String),
    #[error("release-4 background task stopped unexpectedly")]
    BackgroundTask,
    #[error("release-4 state lock is poisoned")]
    StatePoisoned,
    #[error("JSON conversion failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Error)]
pub enum Release4BuildError {
    #[error(transparent)]
    Cloud(#[from] CloudConfigError),
    #[error(transparent)]
    Release4(Release4Error),
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::sync::mpsc;
    use std::time::Duration;

    use tempfile::tempdir;

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

        fn inspect_project(
            &self,
            _working_directory: &Path,
        ) -> Result<ProjectGitSnapshot, CloudError> {
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
        fn start(&self, request: &TeleportStartRequest) -> Result<String, CloudError> {
            if self.fail.load(AtomicOrdering::Relaxed) {
                Err(CloudError::Unauthorized("sign in again".to_owned()))
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

    fn run_test_git(working_directory: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(working_directory)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .status()
            .expect("Git test command starts");
        assert!(status.success(), "Git test command failed: {args:?}");
    }

    fn committed_github_repository() -> tempfile::TempDir {
        let repository = tempdir().expect("temporary Git repository");
        run_test_git(repository.path(), &["init", "--quiet"]);
        run_test_git(repository.path(), &["config", "user.name", "Vibe Test"]);
        run_test_git(
            repository.path(),
            &["config", "user.email", "vibe@example.test"],
        );
        run_test_git(repository.path(), &["branch", "-M", "main"]);
        fs::write(repository.path().join("tracked.txt"), "base\n").expect("tracked fixture");
        run_test_git(repository.path(), &["add", "--", "tracked.txt"]);
        run_test_git(repository.path(), &["commit", "--quiet", "-m", "base"]);
        run_test_git(
            repository.path(),
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        run_test_git(
            repository.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        repository
    }

    #[test]
    fn command_git_probe_tolerates_fetch_failure_and_transfers_dirty_only_changes() {
        let repository = committed_github_repository();
        let nested = repository.path().join("nested/deeper");
        fs::create_dir_all(&nested).expect("nested working directory");
        fs::write(repository.path().join("tracked.txt"), "changed\n").expect("tracked change");
        fs::write(
            repository.path().join("untracked.bin"),
            [0_u8, 1, 2, 0xff, 0, 0x80, 3],
        )
        .expect("untracked change");
        let probe = CommandGitProbe::default()
            .with_timeouts(Duration::from_secs(2), Duration::from_millis(1));

        let (snapshot, context, push) = probe.inspection(&nested).expect("Git inspection succeeds");

        assert!(snapshot.dirty);
        assert!(!snapshot.unpushed);
        assert_eq!(push.unpushed_count, 0);
        assert!(!push.branch_not_pushed);
        let encoded = context.diff.expect("dirty diff");
        let compressed = BASE64_STANDARD
            .decode(encoded.content)
            .expect("base64 diff");
        let decoded = zstd::stream::decode_all(compressed.as_slice()).expect("zstd diff");
        let decoded = String::from_utf8(decoded).expect("UTF-8 Git patch");
        assert!(decoded.contains("changed"));
        assert!(decoded.contains("untracked.bin"));
        assert!(decoded.contains("GIT binary patch"));
        assert!(!repository.path().join(".git/index.lock").exists());
    }

    #[test]
    fn command_git_probe_reports_true_unpushed_commit_count() {
        let repository = committed_github_repository();
        for (name, contents) in [("one.txt", "one\n"), ("two.txt", "two\n")] {
            fs::write(repository.path().join(name), contents).expect("commit fixture");
            run_test_git(repository.path(), &["add", "--", name]);
            run_test_git(repository.path(), &["commit", "--quiet", "-m", name]);
        }
        let probe = CommandGitProbe::default()
            .with_timeouts(Duration::from_secs(2), Duration::from_millis(1));

        let (snapshot, context, push) = probe
            .inspection(repository.path())
            .expect("Git inspection succeeds");

        assert!(snapshot.unpushed);
        assert!(!snapshot.dirty);
        assert_eq!(push.unpushed_count, 2);
        assert!(!push.branch_not_pushed);
        assert!(context.diff.is_none());
    }

    #[test]
    fn git_remote_selection_prefers_an_eligible_github_remote_and_rejects_paths() {
        let repository = committed_github_repository();
        run_test_git(repository.path(), &["remote", "remove", "origin"]);
        run_test_git(
            repository.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://gitlab.example/owner/repo.git",
            ],
        );
        run_test_git(
            repository.path(),
            &[
                "remote",
                "add",
                "github",
                "ssh://git@github.com/owner/repo.git",
            ],
        );
        let metadata = CommandGitProbe::default()
            .metadata(repository.path())
            .expect("eligible remote");
        assert_eq!(metadata.remote, "github");
        assert_eq!(metadata.repo_url, "https://github.com/owner/repo.git");

        for value in [
            "C:\\workspace\\repo",
            "\\\\server\\share\\repo",
            "/workspace/repo",
            "../repo",
            "file:///workspace/repo",
        ] {
            assert!(matches!(
                sanitize_git_remote(value),
                Err(CloudError::Git(_))
            ));
        }
    }

    #[test]
    fn oversized_encoded_diff_fails_instead_of_truncating() {
        let mut state = 0x1234_5678_u32;
        let mut diff = vec![0_u8; 800_000];
        for byte in &mut diff {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state as u8;
        }
        assert!(matches!(
            encode_working_tree_diff(&diff),
            Err(CloudError::Git(message)) if message.contains("Teleport limit")
        ));
    }

    fn spawn_http_response(
        status: &str,
        response_body: Value,
    ) -> (String, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("loopback address");
        let (sender, receiver) = mpsc::channel();
        let status = status.to_owned();
        let response_body = serde_json::to_vec(&response_body).expect("response JSON");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("HTTP request");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("read timeout");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4 * 1024];
            loop {
                let read = stream.read(&mut buffer).expect("HTTP request bytes");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(headers_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers_end = headers_end + 4;
                let headers = String::from_utf8_lossy(&request[..headers_end]);
                let content_length = headers.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                });
                if request.len() >= headers_end.saturating_add(content_length.unwrap_or(0)) {
                    break;
                }
            }
            sender.send(request).expect("captured request");
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            stream
                .write_all(response.as_bytes())
                .and_then(|()| stream.write_all(&response_body))
                .expect("HTTP response");
        });
        (format!("http://{address}"), receiver)
    }

    #[tokio::test]
    async fn http_teleport_omits_nulls_and_transports_the_encoded_diff() {
        let (base_url, captured) = spawn_http_response(
            "200 OK",
            json!({
                "sessionId": "cloud-session",
                "webSessionId": "web-session",
                "projectId": "project-1",
                "status": "created",
                "url": "https://cloud.example/session/cloud-session",
            }),
        );
        let cloud = VibeCodeHttpCloud::new(
            VibeCodeCloudConfig::new(&base_url, SecretString::from("test-credential".to_owned()))
                .expect("cloud config"),
        )
        .expect("HTTP cloud");
        let diff = encode_working_tree_diff(b"diff --git a/file b/file\n").expect("encoded diff");
        let request = TeleportStartRequest {
            project_id: "project-1".to_owned(),
            idempotency_key: "operation-1".to_owned(),
            summary: "continue".to_owned(),
            repository: TeleportRepository {
                repo_url: "https://github.com/owner/repo.git".to_owned(),
                branch: None,
                commit_sha: None,
                diff: Some(diff.clone()),
            },
        };

        let url = cloud
            .start_teleport(&request)
            .await
            .expect("Teleport response");
        assert_eq!(url, "https://cloud.example/session/cloud-session");
        let captured = captured
            .recv_timeout(Duration::from_secs(2))
            .expect("captured HTTP request");
        let body_start = captured
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
            .expect("HTTP body");
        let body: Value = serde_json::from_slice(&captured[body_start..]).expect("request JSON");
        let repository = &body["context"]["repositories"][0];
        assert_eq!(repository["repoUrl"], request.repository.repo_url);
        assert!(repository.get("branch").is_none());
        assert!(repository.get("commitSha").is_none());
        assert_eq!(repository["diff"]["format"], "git-diff");
        assert_eq!(repository["diff"]["encoding"], "base64");
        assert_eq!(repository["diff"]["compression"], "zstd");
        assert_eq!(repository["diff"]["content"], diff.content);
    }

    #[tokio::test]
    async fn http_auth_expiry_is_typed_and_does_not_retry() {
        let (base_url, captured) = spawn_http_response("401 Unauthorized", json!({}));
        let cloud = VibeCodeHttpCloud::new(
            VibeCodeCloudConfig::new(
                &base_url,
                SecretString::from("expired-credential".to_owned()),
            )
            .expect("cloud config"),
        )
        .expect("HTTP cloud");
        let request = TeleportStartRequest {
            project_id: "project-1".to_owned(),
            idempotency_key: "operation-auth".to_owned(),
            summary: "continue".to_owned(),
            repository: TeleportRepository {
                repo_url: "https://github.com/owner/repo.git".to_owned(),
                branch: Some("main".to_owned()),
                commit_sha: Some("0123456789abcdef".to_owned()),
                diff: None,
            },
        };

        assert!(matches!(
            cloud.start_teleport(&request).await,
            Err(CloudError::Unauthorized(message)) if message.contains("authenticate")
        ));
        captured
            .recv_timeout(Duration::from_secs(2))
            .expect("single captured HTTP request");
    }

    #[tokio::test]
    async fn http_teleport_rejects_missing_required_response_fields() {
        let (base_url, captured) = spawn_http_response(
            "200 OK",
            json!({
                "sessionId": "cloud-session",
                "projectId": "project-1",
                "status": "created",
                "url": "https://cloud.example/session/cloud-session",
            }),
        );
        let cloud = VibeCodeHttpCloud::new(
            VibeCodeCloudConfig::new(&base_url, SecretString::from("test-credential".to_owned()))
                .expect("cloud config"),
        )
        .expect("HTTP cloud");
        let request = TeleportStartRequest {
            project_id: "project-1".to_owned(),
            idempotency_key: "operation-invalid-response".to_owned(),
            summary: "continue".to_owned(),
            repository: TeleportRepository {
                repo_url: "https://github.com/owner/repo.git".to_owned(),
                branch: None,
                commit_sha: None,
                diff: None,
            },
        };

        assert!(matches!(
            cloud.start_teleport(&request).await,
            Err(CloudError::Unavailable(message)) if message.contains("invalid response")
        ));
        captured
            .recv_timeout(Duration::from_secs(2))
            .expect("captured HTTP request");
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
        fn start(&self, request: &TeleportStartRequest) -> Result<String, CloudError> {
            self.requests
                .lock()
                .map_err(|_| CloudError::Unavailable("capture lock failed".to_owned()))?
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
        let service = Release4Service::with_backends(
            Arc::new(FixtureProjects),
            teleport.clone(),
            git.clone(),
        );
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

    fn fixture_service(git: Arc<FixtureGit>, teleport: Arc<FixtureTeleport>) -> Release4Service {
        Release4Service::with_backends(Arc::new(FixtureProjects), teleport, git)
    }

    fn headless_service(cloud: Arc<HeadlessProjects>) -> Release4Service {
        Release4Service::with_backends(
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

    async fn open_headless_project(
        service: &Release4Service,
        session_id: &str,
    ) -> Release4Dispatch {
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

    async fn open_picker(service: &Release4Service, session_id: &str) -> String {
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

    fn select_project(
        service: &Release4Service,
        session_id: &str,
        picker_id: &str,
        project_id: &str,
    ) {
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
            Err(Release4Error::NotFound(_))
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
            Err(Release4Error::NotFound(_))
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
                Err(Release4Error::InvalidParams(message))
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
            Err(Release4Error::Conflict(message)) if message.contains("repeated a cursor")
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
            Err(Release4Error::InvalidParams(message))
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

        let unavailable = Release4Service::default();
        assert!(matches!(
            unavailable.dispatch_deferred(
                "vibeCode/projects/open",
                &params(json!({"sessionId": "session-2"}))
            )
            .await,
            Err(Release4Error::Cloud(CloudError::Git(_)))
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
            Err(Release4Error::NotFound(_))
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
        let service = Release4Service::with_backends(Arc::new(FixtureProjects), teleport, git)
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
            service.dispatch_deferred(
                "vibeCode/teleport/push/respond",
                &params(json!({
                    "sessionId": "session-1",
                    "operationId": operation_id,
                    "approved": false
                }))
            )
            .await,
            Err(Release4Error::Conflict(_))
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
        assert_eq!(
            failed.notifications.last().expect("failure event").params["event"]["kind"],
            json!("failed")
        );
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
            Err(Release4Error::Conflict(message))
                if message.contains("selected")
        ));
        select_project(&service, "session-1", &picker_id, "page-first");
        assert!(matches!(
            service.dispatch_deferred(
                "vibeCode/teleport/start",
                &params(json!({
                    "sessionId": "session-2",
                    "pickerId": picker_id,
                    "operationId": "operation-foreign",
                    "projectId": "page-first"
                }))
            )
            .await,
            Err(Release4Error::NotFound(_))
        ));
        assert!(matches!(
            service.dispatch_deferred(
                "vibeCode/teleport/start",
                &params(json!({
                    "sessionId": "session-1",
                    "pickerId": picker_id,
                    "operationId": "operation-missing-project",
                    "projectId": "missing"
                }))
            )
            .await,
            Err(Release4Error::NotFound(_))
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
                Err(Release4Error::Conflict(message))
                    if message.contains("irreversible")
            ));
        }
    }

    #[test]
    fn scheduled_loops_are_owned_persistent_and_retry_safe() {
        let temporary = tempdir().expect("temporary directory");
        let path = temporary.path().join("loops.json");
        let service = Release4Service::default()
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
            Err(Release4Error::Conflict(_))
        ));
        let fired = service.fire_loop(&loop_id, 40, true).expect("due loop");
        assert_eq!(fired.prompt, "review");
        assert!(matches!(
            service.fire_loop(&loop_id, 40, true),
            Err(Release4Error::Conflict(_))
        ));
        drop(service);

        let reloaded = Release4Service::default()
            .with_loop_store(path)
            .expect("reload");
        assert!(matches!(
            reloaded.fire_loop(&loop_id, 40, true),
            Err(Release4Error::Conflict(_))
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
            Err(Release4Error::NotFound(_))
        ));
    }

    #[test]
    fn session_removal_deletes_owned_loops_transactionally_and_durably() {
        let temporary = tempdir().expect("loop store");
        let loop_path = temporary.path().join("loops.json");
        let mut service = Release4Service::default()
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
            Err(Release4Error::Persistence(_))
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
        let reloaded = Release4Service::default()
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
            Err(Release4Error::Persistence(_))
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
        let reloaded = Release4Service::default()
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
}
