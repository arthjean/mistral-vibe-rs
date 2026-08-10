//! The paginated registry catalog client.
//!
//! Reference `vibe/core/skills/registry/_client.py`: three GET operations over
//! a `/skills` base, a catalog listing that follows `nextPageToken` under a
//! hard 50-page cap, a version listing sorted newest first, and a single-skill
//! fetch whose version selector is `version` or `alias` but never both. Every
//! failure surfaces as one error type carrying its reason: an unauthorized or
//! missing resource names its status, an unexpected status names itself, and a
//! malformed 200 names which payload failed.
//!
//! The HTTP layer sits behind [`RegistryTransport`] so the request parameters
//! and the error taxonomy are observable without a live service, and the
//! transport exists only when the caller opens one explicitly: a client that
//! was never opened answers an error rather than building a transport
//! implicitly.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::models::{
    ListSkillsResponse, ListVersionsResponse, RegistrySkillItem, SkillVersionInfo,
};

/// The pagination cap: a response chain still holding a token after this many
/// pages fails loudly rather than answering a silently truncated catalog.
const MAX_PAGES: usize = 50;

/// The field mask the catalog listing requests, verbatim from the reference.
const CATALOG_FIELDS: &str =
    "skillId,skill.skillName,skill.skillDescription,attributes,metadata,version";

/// Any registry failure, carrying the reason a caller or a log can name.
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct RegistrySkillsError {
    pub reason: String,
}

impl RegistrySkillsError {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// One transport-level response: the status and the raw body.
#[derive(Debug, Clone)]
pub struct TransportResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The seam between the client's behavior and the wire. The error is the
/// underlying transport reason, which the client wraps into its taxonomy.
pub trait RegistryTransport {
    fn get(
        &self,
        url: &str,
        params: &[(String, String)],
    ) -> impl Future<Output = Result<TransportResponse, String>> + Send;
}

/// The production transport: a reqwest client carrying the bearer key, the
/// JSON accept header and the timeout. Built only by [`HttpRegistryTransport::open`],
/// never as a side effect of constructing a [`RegistrySkillsClient`].
#[derive(Debug)]
pub struct HttpRegistryTransport {
    client: reqwest::Client,
}

impl HttpRegistryTransport {
    pub fn open(api_key: &str, timeout: Duration) -> Result<Self, RegistrySkillsError> {
        let mut headers = reqwest::header::HeaderMap::new();
        let mut authorization =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {api_key}"))
                .map_err(|_| RegistrySkillsError::new("the API key is not a valid header value"))?;
        authorization.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, authorization);
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(timeout)
            .build()
            .map_err(|error| {
                RegistrySkillsError::new(format!("the transport could not be built: {error}"))
            })?;
        Ok(Self { client })
    }
}

impl RegistryTransport for HttpRegistryTransport {
    async fn get(
        &self,
        url: &str,
        params: &[(String, String)],
    ) -> Result<TransportResponse, String> {
        let response = self
            .client
            .get(url)
            .query(params)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .await
            .map_err(|error| error.to_string())?
            .to_vec();
        Ok(TransportResponse { status, body })
    }
}

/// The catalog client over one opened transport.
#[derive(Debug)]
pub struct RegistrySkillsClient<T> {
    skills_url: String,
    transport: Option<T>,
}

impl<T: RegistryTransport> RegistrySkillsClient<T> {
    /// A closed client for `base_url`. Every operation answers an error until
    /// [`open`](Self::open) hands it a transport.
    #[must_use]
    pub fn new(base_url: &str) -> Self {
        Self {
            skills_url: format!("{}/skills", base_url.trim_end_matches('/')),
            transport: None,
        }
    }

    /// Establishes the session the operations run in.
    pub fn open(&mut self, transport: T) {
        self.transport = Some(transport);
    }

    /// The whole catalog, followed page by page under the cap, each page
    /// requesting the reference field mask.
    pub async fn list_catalog(
        &self,
        page_size: u32,
    ) -> Result<Vec<RegistrySkillItem>, RegistrySkillsError> {
        let mut items = Vec::new();
        let mut page_token = String::new();
        for _ in 0..MAX_PAGES {
            let mut params = vec![
                ("pageSize".to_owned(), page_size.to_string()),
                ("fields".to_owned(), CATALOG_FIELDS.to_owned()),
            ];
            if !page_token.is_empty() {
                params.push(("pageToken".to_owned(), page_token.clone()));
            }
            let payload = self.get_json(&self.skills_url, &params).await?;
            let page: ListSkillsResponse = parse(payload, "catalog")?;
            items.extend(page.data);
            if page.next_page_token.is_empty() {
                return Ok(items);
            }
            page_token = page.next_page_token;
        }
        // The cap was reached with pages still remaining: fail loudly rather
        // than answer a catalog a caller would treat as complete.
        Err(RegistrySkillsError::new(format!(
            "the catalog kept paging past the {MAX_PAGES}-page cap"
        )))
    }

    /// One skill's versions, newest first, each carrying its author aliases.
    pub async fn list_versions(
        &self,
        skill_id: &str,
    ) -> Result<Vec<SkillVersionInfo>, RegistrySkillsError> {
        let url = format!("{}/{skill_id}/versions", self.skills_url);
        let payload = self.get_json(&url, &[]).await?;
        let parsed: ListVersionsResponse = parse(payload, "versions")?;
        let mut infos: Vec<SkillVersionInfo> = parsed
            .items
            .iter()
            .map(super::models::RegistryVersionRow::to_info)
            .collect();
        infos.sort_by_key(|info| std::cmp::Reverse(info.version));
        Ok(infos)
    }

    /// One skill at one version selector: a concrete `version`, an `alias`,
    /// or neither for the latest, never both at once.
    pub async fn get_skill(
        &self,
        skill_id: &str,
        version: Option<i64>,
        alias: Option<&str>,
    ) -> Result<RegistrySkillItem, RegistrySkillsError> {
        let mut params = Vec::new();
        if let Some(version) = version {
            params.push(("version".to_owned(), version.to_string()));
        } else if let Some(alias) = alias {
            params.push(("alias".to_owned(), alias.to_owned()));
        }
        let url = format!("{}/{skill_id}", self.skills_url);
        let payload = self.get_json(&url, &params).await?;
        parse(payload, "skill")
    }

    async fn get_json(
        &self,
        url: &str,
        params: &[(String, String)],
    ) -> Result<Value, RegistrySkillsError> {
        let Some(transport) = &self.transport else {
            return Err(RegistrySkillsError::new(
                "the client was used before a transport was opened",
            ));
        };
        let response = transport.get(url, params).await.map_err(|reason| {
            RegistrySkillsError::new(format!("the request could not be completed: {reason}"))
        })?;
        match response.status {
            401 | 403 => {
                return Err(RegistrySkillsError::new(format!(
                    "unauthorized ({})",
                    response.status
                )));
            }
            404 => return Err(RegistrySkillsError::new("not found (404)")),
            status if !(200..300).contains(&status) => {
                return Err(RegistrySkillsError::new(format!(
                    "unexpected status {status}"
                )));
            }
            _ => {}
        }
        serde_json::from_slice(&response.body)
            .map_err(|_| RegistrySkillsError::new("the response body is not valid JSON"))
    }
}

/// Validates a registry payload, normalizing failures so a malformed 200
/// surfaces as a registry error naming which payload failed.
fn parse<M: DeserializeOwned>(payload: Value, what: &str) -> Result<M, RegistrySkillsError> {
    serde_json::from_value(payload).map_err(|_| {
        RegistrySkillsError::new(format!("the {what} response does not match its model"))
    })
}
