//! The two Vibe Code Web endpoints a session talks to: the project list a
//! picker browses and creates in, and the session start a Teleport run ends
//! with.
//!
//! Reference `VibeCodeProjectClient` (`vibe/core/vibe_code_project/client.py`)
//! and `NuageClient` (`vibe/core/teleport/nuage.py`). Every failure is one
//! sentence a client shows; the ones that carry a status keep the status and
//! the body the service answered with, because that text is what decides
//! whether a saved project went stale.

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

use super::git::FailureClass;

/// How many projects one page asks for (reference
/// `VIBE_CODE_PROJECT_PICKER_PAGE_LIMIT`).
const PAGE_LIMIT: u32 = 100;

/// The session start is tried this many times on an answer that leaves its
/// outcome unknown (reference `NuageClient.max_start_attempts`).
const START_ATTEMPTS: usize = 3;

/// The pause between two of those attempts (reference `retry_delay_seconds`).
const START_RETRY_DELAY: Duration = Duration::from_millis(500);

/// The timeout a Teleport run's client keeps (reference `TeleportService`'s
/// default).
pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(60);

/// Reference `ProjectRepository`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ProjectRepository {
    #[serde(rename = "repoUrl")]
    pub(crate) repo_url: String,
    #[serde(rename = "defaultBranch", default)]
    pub(crate) default_branch: Option<String>,
}

/// Reference `VibeCodeProject`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Project {
    #[serde(rename = "id")]
    pub(crate) project_id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) repositories: Vec<ProjectRepository>,
    #[serde(rename = "isReadOnly", default)]
    pub(crate) is_read_only: bool,
}

#[derive(Debug, Deserialize)]
struct ProjectPage {
    items: Vec<Project>,
    #[serde(rename = "nextCursor", default)]
    next_cursor: Option<String>,
}

/// One page of projects and where the next one starts.
pub(crate) type Page = (Vec<Project>, Option<String>);

/// The project API, bound to one key.
pub(crate) struct ProjectClient {
    base_url: String,
    api_key: String,
    client: Client,
}

impl ProjectClient {
    pub(crate) fn new(base_url: &str, api_key: &str, timeout: Duration) -> Result<Self, String> {
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key: api_key.to_owned(),
            client: http_client(timeout)?,
        })
    }

    /// Reference `list_projects`.
    pub(crate) async fn list(&self, cursor: Option<&str>) -> Result<Page, String> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(cursor) = cursor.filter(|cursor| !cursor.is_empty()) {
            query.push(("cursor", cursor.to_owned()));
        }
        query.push(("limit", PAGE_LIMIT.to_string()));
        let response = self
            .client
            .get(format!("{}/api/v1/code/projects", self.base_url))
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .query(&query)
            .send()
            .await
            .map_err(|_| "Reaching Vibe Code Web to list projects failed.".to_owned())?;
        let (status, body) = read(response)
            .await
            .map_err(|_| "Reaching Vibe Code Web to list projects failed.".to_owned())?;
        if !status.is_success() {
            return Err(format!(
                "Listing Vibe Code Web projects failed (status {}): {body}",
                status.as_u16()
            ));
        }
        let value: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
            "Vibe Code Web answered the project list with something other than JSON.".to_owned()
        })?;
        let page: ProjectPage = serde_json::from_value(value).map_err(|_| {
            "Vibe Code Web answered the project list in an unexpected shape.".to_owned()
        })?;
        Ok((page.items, page.next_cursor))
    }

    /// Reference `create_project`.
    pub(crate) async fn create(
        &self,
        name: &str,
        repo_url: &str,
        default_branch: &str,
    ) -> Result<Project, String> {
        #[derive(Serialize)]
        struct Repository<'a> {
            #[serde(rename = "repoUrl")]
            repo_url: &'a str,
            #[serde(rename = "defaultBranch")]
            default_branch: &'a str,
        }
        #[derive(Serialize)]
        struct Body<'a> {
            name: &'a str,
            repositories: [Repository<'a>; 1],
        }
        let body = Body {
            name,
            repositories: [Repository {
                repo_url,
                default_branch,
            }],
        };
        let response = self
            .client
            .post(format!("{}/api/v1/code/projects", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|_| "Reaching Vibe Code Web to create the project failed.".to_owned())?;
        let (status, body) = read(response)
            .await
            .map_err(|_| "Reaching Vibe Code Web to create the project failed.".to_owned())?;
        if !status.is_success() {
            return Err(format!(
                "Creating the Vibe Code Web project failed (status {}): {body}",
                status.as_u16()
            ));
        }
        let value: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
            "Vibe Code Web answered the new project with something other than JSON.".to_owned()
        })?;
        serde_json::from_value(value).map_err(|_| {
            "Vibe Code Web answered the new project in an unexpected shape.".to_owned()
        })
    }
}

/// Reference `NuageRequest`, serialized by alias with absent values left out.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StartRequest {
    #[serde(rename = "projectId")]
    pub(crate) project_id: String,
    pub(crate) source: &'static str,
    #[serde(rename = "idempotencyKey")]
    pub(crate) idempotency_key: String,
    #[serde(rename = "conversationId", skip_serializing_if = "Option::is_none")]
    pub(crate) conversation_id: Option<String>,
    pub(crate) message: StartMessage,
    pub(crate) context: StartContext,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StartMessage {
    pub(crate) role: &'static str,
    pub(crate) parts: Vec<TextPart>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TextPart {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StartContext {
    pub(crate) repositories: Vec<StartRepository>,
    #[serde(rename = "messageContext", skip_serializing_if = "Option::is_none")]
    pub(crate) message_context: Option<MessageContext>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StartRepository {
    #[serde(rename = "repoUrl")]
    pub(crate) repo_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) branch: Option<String>,
    #[serde(rename = "commitSha", skip_serializing_if = "Option::is_none")]
    pub(crate) commit_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) diff: Option<StartDiff>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StartDiff {
    pub(crate) format: &'static str,
    pub(crate) encoding: &'static str,
    pub(crate) compression: &'static str,
    pub(crate) content: String,
}

/// Reference `TeleportMessageContext`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct MessageContext {
    pub(crate) summary: String,
    pub(crate) source: MessageContextSource,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MessageContextSource {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) entrypoint: String,
    #[serde(rename = "clientName", skip_serializing_if = "Option::is_none")]
    pub(crate) client_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StartResponse {
    #[serde(rename = "sessionId")]
    _session_id: String,
    #[serde(rename = "webSessionId")]
    _web_session_id: String,
    #[serde(rename = "projectId")]
    _project_id: String,
    #[serde(rename = "status")]
    _status: String,
    url: String,
}

/// Why a session start failed, with what telemetry reports about it
/// (reference `TeleportFailureDetails`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartFailure {
    pub(crate) message: String,
    pub(crate) class: FailureClass,
    pub(crate) failure_kind: Option<&'static str>,
    pub(crate) http_status_code: Option<u16>,
}

impl StartFailure {
    fn ambiguous(http_status_code: Option<u16>) -> Self {
        Self {
            message: "Vibe Code Web never confirmed the session was created, even after \
                      retrying. Check Vibe Code Web before teleporting again."
                .to_owned(),
            class: FailureClass::Service,
            failure_kind: Some("ambiguous_create"),
            http_status_code,
        }
    }
}

/// Reference `NuageClient.start`: the same body and key on every attempt, a
/// gateway timeout or a broken exchange tried again, and the final answer
/// classified.
pub(crate) async fn start_session(
    base_url: &str,
    api_key: &str,
    request: &StartRequest,
) -> Result<String, StartFailure> {
    let client = http_client(START_TIMEOUT).map_err(|message| StartFailure {
        message,
        class: FailureClass::Service,
        failure_kind: None,
        http_status_code: None,
    })?;
    let body = serde_json::to_vec(request).map_err(|error| StartFailure {
        message: error.to_string(),
        class: FailureClass::Service,
        failure_kind: None,
        http_status_code: None,
    })?;
    let endpoint = format!("{}/api/v1/code/sessions", base_url.trim_end_matches('/'));
    let mut answer: Option<(StatusCode, String)> = None;
    for attempt in 0..START_ATTEMPTS {
        let last = attempt + 1 == START_ATTEMPTS;
        let exchanged = match client
            .post(&endpoint)
            .bearer_auth(api_key)
            .header("content-type", "application/json")
            .body(body.clone())
            .send()
            .await
        {
            Ok(response) => read(response).await,
            Err(error) => Err(error),
        };
        match exchanged {
            Err(_) if !last => {
                tokio::time::sleep(START_RETRY_DELAY).await;
            }
            Err(_) => return Err(StartFailure::ambiguous(None)),
            Ok((status, _)) if status == StatusCode::GATEWAY_TIMEOUT && !last => {
                tokio::time::sleep(START_RETRY_DELAY).await;
            }
            Ok(exchanged) => {
                answer = Some(exchanged);
                break;
            }
        }
    }
    let Some((status, text)) = answer else {
        return Err(StartFailure::ambiguous(None));
    };
    let code = status.as_u16();
    if status == StatusCode::GATEWAY_TIMEOUT {
        return Err(StartFailure::ambiguous(Some(code)));
    }
    if !status.is_success() {
        return Err(StartFailure {
            message: format!("Vibe Code Web could not start the session (status {code}): {text}"),
            class: FailureClass::Service,
            failure_kind: Some("http_error"),
            http_status_code: Some(code),
        });
    }
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| StartFailure {
        message: "Vibe Code Web answered the session start with something other than JSON."
            .to_owned(),
        class: FailureClass::Service,
        failure_kind: Some("invalid_json"),
        http_status_code: Some(code),
    })?;
    let response: StartResponse = serde_json::from_value(value).map_err(|_| StartFailure {
        message: "Vibe Code Web answered the session start in an unexpected shape.".to_owned(),
        class: FailureClass::Service,
        failure_kind: Some("invalid_schema"),
        http_status_code: Some(code),
    })?;
    Ok(response.url)
}

async fn read(response: reqwest::Response) -> Result<(StatusCode, String), reqwest::Error> {
    let status = response.status();
    let text = response.text().await?;
    Ok((status, text))
}

/// A client bounded the way the reference bounds httpx: every phase gets the
/// same timeout, and a redirect is answered rather than followed. The certificate environment is honored as for every other
/// outbound client.
fn http_client(timeout: Duration) -> Result<Client, String> {
    let builder = Client::builder()
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .redirect(reqwest::redirect::Policy::none());
    vibe_core::http_trust::trust_certificate_environment(builder)
        .build()
        .map_err(|error| error.to_string())
}
