//! The Mistral backend, which the reference drives through the Mistral Python
//! client (`vibe/core/llm/backend/mistral.py` over `mistralai` 2.6).
//!
//! What reaches the wire is the client's: its request models write the body
//! ([`request`]) and its response models read the answer back ([`answer`]).
//! The client retries 429 and the 5xx it knows on its own schedule and
//! retries connection failures and timeouts, but not a peer that hung up
//! mid-answer; the backend reports every retryable status and every such
//! failure as it happens.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::error::{
    BackendErrorSource, BackendFailure, KeyOrigin, KeySource, LocalFailure, LocalKind,
    TransportFailure,
};
use super::generic::CallFacts;
use super::pump::{ChunkPump, ChunkStream, Settle};
use super::retry::{
    Clock, MISTRAL_RETRYABLE_STATUSES, RetryObserver, RetryReason, SleepKind, mistral_delay,
    mistral_retry_after_millis,
};
use super::transport::{HttpClient, HttpResponse, HttpSettings, merge_headers};
use super::types::Chunk;
use super::{BackendContext, ModelRequest};
use crate::provider::config::ProviderConfig;

mod answer;
mod request;

use answer::{EventReader, whole_answer};
use request::body;

const PATH: &str = "/v1/chat/completions";
/// The variable the Mistral client reads a key from when it is handed none.
const CLIENT_KEY_VARIABLE: &str = "MISTRAL_API_KEY";

pub struct MistralBackend {
    provider: ProviderConfig,
    server_url: String,
    client: HttpClient,
    api_key: Option<String>,
    api_key_origin: Option<KeyOrigin>,
    budget_millis: i64,
    context: BackendContext,
}

static SERVER_URL: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^(https?://.+)(/v\d+.*)").ok());

/// The server URL an API base names: what precedes its last `/v<digits>`
/// segment on its first line, or `None` without one. Reference
/// `get_server_url_from_api_base` (`vibe/utils/http.py`), the same regex,
/// which every client of a Mistral API base derives its server from.
#[must_use]
pub fn server_url(api_base: &str) -> Option<String> {
    let captures = SERVER_URL.as_ref()?.captures(api_base)?;
    Some(captures.get(1)?.as_str().to_owned())
}

impl MistralBackend {
    /// # Errors
    ///
    /// A custom reasoning field, which this client cannot honor, an API base
    /// without a version path, and an HTTP client that fails to initialize.
    pub(super) fn new(
        provider: ProviderConfig,
        context: BackendContext,
    ) -> Result<Self, LocalFailure> {
        if provider.reasoning_field_name != "reasoning_content" {
            return Err(LocalFailure::new(
                LocalKind::Configuration,
                format!(
                    "the Mistral backend reads reasoning from thinking chunks and cannot use the \
                     reasoning field `{}`",
                    provider.reasoning_field_name
                ),
            ));
        }
        let server_url = server_url(&provider.api_base).ok_or_else(|| {
            LocalFailure::new(
                LocalKind::Configuration,
                format!(
                    "the API base `{}` is not of the form <server>/v<version>",
                    provider.api_base
                ),
            )
        })?;
        let api = context.api;
        let client = HttpClient::new(HttpSettings {
            read_timeout: api.timeout,
            connect_timeout: api.timeout.min(api.connect_timeout),
            follow_redirects: true,
        })
        .map_err(|failure| LocalFailure::new(LocalKind::Configuration, failure.to_string()))?;
        let resolved = context.credentials.resolve(&provider.api_key_env_var);
        let (api_key, api_key_origin) = match resolved {
            Some((key, origin)) => (Some(key), Some(origin)),
            // The client handed no key reads `MISTRAL_API_KEY` from the
            // environment (`mistralai` `get_security_from_env`), so a provider
            // whose own variable resolves nothing still sends that one, and
            // reports no origin for it.
            None => (
                context
                    .credentials
                    .resolve(CLIENT_KEY_VARIABLE)
                    .filter(|(_, origin)| origin.source == KeySource::Environment)
                    .map(|(key, _)| key),
                None,
            ),
        };
        #[allow(clippy::cast_possible_truncation)]
        let budget_millis = (api.retry_max_elapsed_time.as_secs_f64() * 1000.0) as i64;
        Ok(Self {
            provider,
            server_url,
            client,
            api_key,
            api_key_origin,
            budget_millis,
            context,
        })
    }

    fn clock(&self) -> &dyn Clock {
        self.context.clock.as_ref()
    }

    /// The URL every call is sent to.
    pub(super) fn endpoint(&self) -> String {
        format!("{}{PATH}", self.server_url)
    }

    fn facts(&self, request: &ModelRequest<'_>) -> CallFacts {
        CallFacts::new(
            &self.provider,
            self.server_url.clone(),
            request,
            self.api_key_origin.clone(),
        )
    }

    fn headers(&self, request: &ModelRequest<'_>, streaming: bool) -> Vec<(String, String)> {
        let mut headers = vec![
            (
                "user-agent".to_owned(),
                "mistral-client-python/2.6.0".to_owned(),
            ),
            (
                "Accept".to_owned(),
                if streaming {
                    "text/event-stream"
                } else {
                    "application/json"
                }
                .to_owned(),
            ),
            ("Content-Type".to_owned(), "application/json".to_owned()),
        ];
        if let Some(key) = &self.api_key {
            headers.push(("Authorization".to_owned(), format!("Bearer {key}")));
        }
        merge_headers(
            &mut headers,
            request
                .extra_headers
                .iter()
                .map(|(name, value)| (name, value)),
        );
        headers
    }

    /// Sends the request under the client's retry schedule, reporting each
    /// retryable status and connection failure. What comes back is the last
    /// answer, retryable or not, once the budget is spent.
    async fn send(
        &self,
        request: &ModelRequest<'_>,
        streaming: bool,
        facts: &CallFacts,
        retries: &dyn RetryObserver,
    ) -> Result<(HttpResponse, Option<Vec<u8>>), BackendFailure> {
        let body = serde_json::to_vec(&body(request, streaming))
            .map_err(|error| BackendFailure::local(LocalKind::Type, error.to_string()))?;
        let headers = self.headers(request, streaming);
        let url = self.endpoint();
        let started = millis(self.clock().monotonic());
        let mut attempt: u32 = 0;
        loop {
            let outcome = self.client.post(&url, &headers, body.clone()).await;
            let delay = match outcome {
                Ok(mut response) => {
                    if !MISTRAL_RETRYABLE_STATUSES.contains(&response.status) {
                        return Ok((response, None));
                    }
                    let read = response.read_all().await.ok();
                    let retry_after = response
                        .header("retry-after")
                        .and_then(|value| mistral_retry_after_millis(value, self.clock().now()));
                    let delay = mistral_delay(retry_after, attempt, self.clock().jitter());
                    retries.retrying_after(&RetryReason::for_status(response.status), delay);
                    let over = millis(self.clock().monotonic()) - started > self.budget_millis;
                    if over {
                        return Ok((response, Some(read.unwrap_or_default())));
                    }
                    delay
                }
                Err(failure) => {
                    let retryable = failure.kind.is_network() || failure.kind.is_timeout();
                    if !retryable {
                        return Err(facts.request_error(&failure));
                    }
                    let delay = mistral_delay(None, attempt, self.clock().jitter());
                    retries.retrying_after(
                        &RetryReason::for_failure(&facts.request_error(&failure)),
                        delay,
                    );
                    if millis(self.clock().monotonic()) - started > self.budget_millis {
                        return Err(facts.request_error(&failure));
                    }
                    delay
                }
            };
            self.clock().sleep(delay, SleepKind::Retry).await;
            attempt += 1;
        }
    }

    /// Reads a refused answer the way the client raises it.
    async fn refusal(
        facts: &CallFacts,
        mut response: HttpResponse,
        read: Option<Vec<u8>>,
    ) -> BackendFailure {
        let body = match read {
            Some(body) => body,
            None => match response.read_all().await {
                Ok(body) => body,
                Err(failure) => return facts.request_error(&failure),
            },
        };
        let content_type = response
            .header("content-type")
            .unwrap_or("application/octet-stream");
        if response.status == 422 && content_type_matches(content_type, "application/json") {
            return validation_report(&body);
        }
        facts.status_error(BackendErrorSource::Client, &response, &body)
    }

    /// One non-streaming call.
    ///
    /// # Errors
    ///
    /// The client refusing the answer, and a request that got none.
    pub(super) async fn complete(
        &self,
        request: &ModelRequest<'_>,
        retries: &dyn RetryObserver,
    ) -> Result<Chunk, BackendFailure> {
        let facts = self.facts(request);
        let (mut response, read) = self.send(request, false, &facts, retries).await?;
        let json_answer = response.status == 200
            && content_type_matches(
                response
                    .header("content-type")
                    .unwrap_or("application/octet-stream"),
                "application/json",
            );
        if !json_answer {
            return Err(Self::refusal(&facts, response, read).await);
        }
        let body = match read {
            Some(body) => body,
            None => response
                .read_all()
                .await
                .map_err(|failure| facts.request_error(&failure))?,
        };
        let text = String::from_utf8_lossy(&body).into_owned();
        whole_answer(&text)
            .map_err(|failure| BackendFailure::local(LocalKind::MistralResponse, failure.message))
    }

    /// One streaming call.
    ///
    /// # Errors
    ///
    /// As [`MistralBackend::complete`], for everything before the stream
    /// opens; later failures arrive through the stream.
    pub(super) async fn complete_streaming<'a>(
        &'a self,
        request: &ModelRequest<'_>,
        retries: &dyn RetryObserver,
    ) -> Result<ChunkStream<'a>, BackendFailure> {
        let facts = self.facts(request);
        let (response, read) = self.send(request, true, &facts, retries).await?;
        let event_stream = response.status == 200
            && content_type_matches(
                response
                    .header("content-type")
                    .unwrap_or("application/octet-stream"),
                "text/event-stream",
            );
        if !event_stream {
            return Err(Self::refusal(&facts, response, read).await);
        }
        let correlation_id = response.header("mistral-correlation-id").map(str::to_owned);
        let transport_facts = facts.clone();
        let pump = ChunkPump::new(
            response.body,
            EventReader::new(correlation_id),
            Box::new(move |failure: TransportFailure| transport_facts.request_error(&failure)),
        );
        Ok(pump.into_stream(Settle::none()))
    }
}

fn millis(seconds: f64) -> i64 {
    #[allow(clippy::cast_possible_truncation)]
    let millis = (seconds * 1000.0).round_ties_even() as i64;
    millis
}

/// Reference `match_content_type`: the exact value, a wildcard, or the media
/// type with its parameters stripped.
fn content_type_matches(content_type: &str, pattern: &str) -> bool {
    if pattern == content_type || pattern == "*" || pattern == "*/*" {
        return true;
    }
    let media = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let media = if media.matches('/').count() == 1 {
        media
    } else {
        "text/plain".to_owned()
    };
    if media == pattern {
        return true;
    }
    media.split_once('/').is_some_and(|(major, minor)| {
        pattern == format!("{major}/*") || pattern == format!("*/{minor}")
    })
}

/// A 422 the client reads as a validation report, or fails to read as one.
fn validation_report(body: &[u8]) -> BackendFailure {
    let well_formed = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .is_some_and(|report| match report.get("detail") {
            None | Some(Value::Null) => true,
            Some(Value::Array(items)) => items.iter().all(|item| {
                item.as_object().is_some_and(|item| {
                    item.get("msg").is_some_and(Value::is_string)
                        && item.get("type").is_some_and(Value::is_string)
                        && item
                            .get("loc")
                            .and_then(Value::as_array)
                            .is_some_and(|loc| {
                                loc.iter()
                                    .all(|part| part.is_string() || part.is_i64() || part.is_u64())
                            })
                })
            }),
            Some(_) => false,
        });
    if well_formed {
        BackendFailure::local(
            LocalKind::MistralValidation,
            "the provider refused the request as invalid",
        )
    } else {
        BackendFailure::local(
            LocalKind::MistralResponse,
            "the provider's validation report could not be read",
        )
    }
}
