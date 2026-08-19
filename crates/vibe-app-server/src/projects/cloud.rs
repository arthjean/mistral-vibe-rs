//! The Vibe Code cloud the project and teleport workflows run against.
//!
//! The service above states what a workflow does; this states how it reaches the
//! backend. Both halves of the contract live here, the traits a caller programs
//! against and the HTTP client that satisfies them, so a test can substitute a
//! backend without the service knowing which one answered.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::git::MAX_TELEPORT_DIFF_ENCODED_BYTES;
use thiserror::Error;
use url::Url;

pub(super) const PROJECT_PAGE_LIMIT: usize = 100;
pub(super) const MAX_CLOUD_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
pub(super) const MAX_CLOUD_TEXT_BYTES: usize = 64 * 1024;
pub(super) const DEFAULT_CLOUD_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const DEFAULT_CLOUD_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_VIBE_CODE_BASE_URL: &str = "https://chat.mistral.ai";
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

/// Why a Teleport start failed: the error as the service rendered it, and the
/// HTTP status that produced it when a status is what produced it.
///
/// The status travels beside the error rather than inside it. [`CloudError`]
/// classifies a failure as unavailable, unauthorized or git, and that
/// classification decides remappings elsewhere that a numeric code must not
/// move; a consumer that needs the number reads it here. Reference
/// `TeleportFailureDetails.http_status_code`, which is what tells a saved
/// project link the service refused from one that merely failed.
#[derive(Debug)]
pub struct TeleportStartFailure {
    pub error: CloudError,
    pub http_status_code: Option<u16>,
}

impl From<CloudError> for TeleportStartFailure {
    fn from(error: CloudError) -> Self {
        Self {
            error,
            http_status_code: None,
        }
    }
}

impl std::fmt::Display for TeleportStartFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for TeleportStartFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

pub type TeleportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, TeleportStartFailure>> + Send + 'a>>;

pub trait TeleportCloud: Send + Sync {
    fn start(&self, request: &TeleportStartRequest) -> Result<String, TeleportStartFailure>;
}

pub trait AsyncTeleportCloud: Send + Sync {
    fn start<'a>(&'a self, request: &'a TeleportStartRequest) -> TeleportFuture<'a>;
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
pub(super) enum ProjectCloudBackend {
    Sync(Arc<dyn ProjectCloud>),
    Async(Arc<dyn AsyncProjectCloud>),
}

#[derive(Clone)]
pub(super) enum TeleportCloudBackend {
    Sync(Arc<dyn TeleportCloud>),
    Async(Arc<dyn AsyncTeleportCloud>),
}

pub(super) struct UnavailableProjectCloud;

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

pub(super) struct UnavailableTeleportCloud;

impl TeleportCloud for UnavailableTeleportCloud {
    fn start(&self, _request: &TeleportStartRequest) -> Result<String, TeleportStartFailure> {
        Err(CloudError::Unavailable(
            "Teleport is not configured; provide MISTRAL_API_KEY and retry".to_owned(),
        )
        .into())
    }
}

pub(super) struct VibeCodeHttpCloud {
    config: VibeCodeCloudConfig,
    client: Client,
}

impl VibeCodeHttpCloud {
    pub(super) fn new(config: VibeCodeCloudConfig) -> Result<Self, CloudConfigError> {
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

    /// The Teleport start, with the HTTP status kept beside a failure the
    /// service answered with one: a saved project link the service refused
    /// with a 403 or a 404 is reported as cleared, and only the number tells
    /// that refusal from an ordinary outage.
    async fn start_teleport(
        &self,
        request: &TeleportStartRequest,
    ) -> Result<String, TeleportStartFailure> {
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
                ))
                .into());
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
                    return Err(ambiguous_teleport_error().into());
                }
                Ok(response) => {
                    let status = response.status();
                    let decoded = self
                        .decode::<TeleportResponse>(response, "Vibe Code Teleport start")
                        .await;
                    let response = match decoded {
                        Ok(response) => response,
                        Err(error) => {
                            return Err(TeleportStartFailure {
                                error,
                                http_status_code: (!status.is_success()).then(|| status.as_u16()),
                            });
                        }
                    };
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
                        )
                        .into());
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
                        )
                        .into());
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
                    return Err(ambiguous_teleport_error().into());
                }
                Err(_) => return Err(cloud_request_error("Vibe Code Teleport start").into()),
            }
        }
        Err(ambiguous_teleport_error().into())
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
    fn start<'a>(&'a self, request: &'a TeleportStartRequest) -> TeleportFuture<'a> {
        Box::pin(async move { self.start_teleport(request).await })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProjectRepositoryResponse {
    repo_url: String,
    #[serde(default)]
    default_branch: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProjectResponse {
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
pub(super) struct ProjectListResponse {
    items: Vec<ProjectResponse>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TeleportResponse {
    session_id: String,
    web_session_id: String,
    project_id: String,
    status: String,
    url: String,
}

pub(super) fn validate_cloud_base_url(value: &str) -> Result<Url, CloudConfigError> {
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

pub(super) fn cloud_status_error(status: StatusCode, operation: &str) -> CloudError {
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

pub(super) fn cloud_request_error(operation: &str) -> CloudError {
    CloudError::Unavailable(format!(
        "{operation} could not reach Vibe Code within the configured timeout; check the base URL and network"
    ))
}

pub(super) fn ambiguous_teleport_error() -> CloudError {
    CloudError::Unavailable(
        "Vibe Code did not confirm Teleport session creation after bounded retries; check Vibe Code Web before retrying"
            .to_owned(),
    )
}

pub(super) fn is_ambiguous_request_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
}

pub(super) fn validate_cloud_text(value: &str, label: &str) -> Result<(), CloudError> {
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

pub(super) fn bounded_optional_text(
    value: Option<String>,
    label: &str,
) -> Result<Option<String>, CloudError> {
    value
        .map(|value| {
            validate_cloud_text(&value, label)?;
            Ok(value)
        })
        .transpose()
}

#[cfg(test)]
mod cloud_tests;
