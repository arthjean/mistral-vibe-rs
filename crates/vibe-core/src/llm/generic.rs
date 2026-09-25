//! The generic backend: one HTTP client and the dialect a provider's
//! `api_style` names.
//!
//! Reference `GenericBackend` (`vibe/core/llm/backend/generic.py`). A call
//! resolves its key, lets the dialect write the request, adds the loop's
//! headers, and sends it under the pacer and a budget-bounded retry. A
//! streamed call is retried only until its first chunk: once the caller has
//! seen output, starting over would repeat it.

use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::Value;

use super::adapter::{Dialect, PreparedRequest, Reader, RequestParts};
use super::error::{
    BackendError, BackendErrorSource, BackendFailure, KeyOrigin, LocalFailure, LocalKind,
    PayloadSummary, TransportFailure,
};
use super::pump::{ChunkPump, ChunkStream, EventSource, Settle};
use super::retry::{
    AdaptivePacer, Clock, GENERIC_CAP, GENERIC_DELAY, GENERIC_FACTOR, RetryCategory, RetryObserver,
    RetryReason, SleepKind, generic_is_retryable, generic_retry_after, next_delay,
};
use super::sse::{DataLine, LineError, LineSplitter, data_line};
use super::transport::{HttpClient, HttpResponse, HttpSettings};
use super::types::Chunk;
use super::{BackendContext, ModelRequest};
use crate::provider::config::ProviderConfig;

pub struct GenericBackend {
    provider: ProviderConfig,
    client: HttpClient,
    budget: f64,
    pacer: Arc<AdaptivePacer>,
    context: BackendContext,
}

/// What a failure report says about the call that failed. Reference
/// `ModelCall`.
#[derive(Debug, Clone)]
pub(super) struct CallFacts {
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub summary: PayloadSummary,
    pub api_key_origin: Option<KeyOrigin>,
}

impl CallFacts {
    pub(super) fn new(
        provider: &ProviderConfig,
        endpoint: String,
        request: &ModelRequest<'_>,
        api_key_origin: Option<KeyOrigin>,
    ) -> Self {
        Self {
            provider: provider.name.clone(),
            endpoint,
            model: request.model.name.clone(),
            summary: PayloadSummary {
                model: request.model.name.clone(),
                message_count: request.messages.len(),
                approx_chars: request
                    .messages
                    .iter()
                    .map(|message| message.text().chars().count())
                    .sum(),
                temperature: request.temperature,
                has_tools: request.tools.is_some_and(|tools| !tools.is_empty()),
                tool_choice: request.tool_choice.cloned(),
            },
            api_key_origin,
        }
    }

    fn error(
        &self,
        source: BackendErrorSource,
        status: Option<u16>,
        reason: Option<String>,
        headers: std::collections::BTreeMap<String, String>,
        body_text: String,
        parsed_error: Option<String>,
    ) -> BackendError {
        BackendError {
            provider: self.provider.clone(),
            endpoint: self.endpoint.clone(),
            status,
            reason,
            headers,
            body_text,
            parsed_error,
            model: self.model.clone(),
            payload_summary: self.summary.clone(),
            api_key_origin: self.api_key_origin.clone(),
            source,
        }
    }

    /// A request that never got an answer. Reference `build_request_error`.
    pub(super) fn request_error(&self, failure: &TransportFailure) -> BackendFailure {
        self.error(
            BackendErrorSource::Request(failure.kind),
            None,
            Some(failure.to_string()),
            std::collections::BTreeMap::new(),
            String::new(),
            Some(NETWORK_ERROR.to_owned()),
        )
        .into()
    }

    /// An error status and the body that came with it. Reference
    /// `build_http_error`.
    pub(super) fn status_error(
        &self,
        source: BackendErrorSource,
        response: &HttpResponse,
        body: &[u8],
    ) -> BackendFailure {
        let text = String::from_utf8_lossy(body).into_owned();
        self.error(
            source,
            Some(response.status),
            response.reason.clone(),
            response.headers.clone(),
            text.clone(),
            super::error::provider_message(&text),
        )
        .into()
    }

    /// A failure a Responses stream reported. Reference `build_stream_error`.
    fn stream_error(&self, error: &super::responses::StreamError) -> BackendFailure {
        let body = serde_json::json!({
            "error": {"type": error.error_type, "message": error.message},
        });
        self.error(
            BackendErrorSource::Stream,
            error.status,
            Some(error.error_type.clone()),
            std::collections::BTreeMap::new(),
            super::python_json::Ordered::from(&body).dumps(true),
            Some(error.message.clone()),
        )
        .into()
    }

    /// Every failure as the caller sees it once retrying stopped.
    fn surface(&self, failure: BackendFailure) -> BackendFailure {
        match failure {
            BackendFailure::ResponsesStream(error) => self.stream_error(&error),
            other => other,
        }
    }
}

/// What a request error says the provider answered, since none did.
const NETWORK_ERROR: &str = "network error";

impl GenericBackend {
    /// # Errors
    ///
    /// The HTTP client failing to initialize.
    pub(super) fn new(
        provider: ProviderConfig,
        context: BackendContext,
    ) -> Result<Self, LocalFailure> {
        let api = context.api;
        let client = HttpClient::new(HttpSettings {
            read_timeout: api.timeout,
            connect_timeout: api.timeout,
            follow_redirects: false,
        })
        .map_err(|failure| LocalFailure::new(LocalKind::Configuration, failure.to_string()))?;
        Ok(Self {
            provider,
            client,
            budget: api.retry_max_elapsed_time.as_secs_f64(),
            pacer: Arc::new(AdaptivePacer::new(Arc::clone(&context.clock))),
            context,
        })
    }

    fn clock(&self) -> &dyn Clock {
        self.context.clock.as_ref()
    }

    /// The request, its URL and what a failure report says about it.
    async fn prepare(
        &self,
        request: &ModelRequest<'_>,
        streaming: bool,
    ) -> Result<(PreparedRequest, String, CallFacts, Dialect), BackendFailure> {
        let style = self.provider.api_style.as_str();
        let resolved = self
            .context
            .credentials
            .resolve(&self.provider.api_key_env_var);
        let dialect = Dialect::for_style(style).ok_or_else(|| {
            BackendFailure::local(LocalKind::Key, format!("unknown API style `{style}`"))
        })?;
        let (api_key, origin) = match resolved {
            Some((key, origin)) => (
                Some(key),
                (dialect != Dialect::VertexAnthropic).then_some(origin),
            ),
            None => (None, None),
        };
        let token = if dialect == Dialect::VertexAnthropic
            && !self.provider.project_id.is_empty()
            && !self.provider.region.is_empty()
        {
            Some(self.context.vertex.access_token().await?)
        } else {
            None
        };
        let parts = RequestParts {
            model_name: &request.model.name,
            messages: request.messages,
            temperature: request.temperature,
            tools: request.tools,
            max_tokens: request.max_tokens,
            tool_choice: request.tool_choice,
            streaming,
            provider: &self.provider,
            api_key: api_key.as_deref(),
            thinking: &request.model.thinking,
        };
        let mut prepared = dialect.prepare(&parts, token.as_deref())?;
        if dialect == Dialect::VertexAnthropic && prepared.base_url.is_some() {
            prepared.base_url = Some(self.context.vertex.base_url(&self.provider.region));
        }
        merge_headers(&mut prepared.headers, request.extra_headers);
        let base = prepared
            .base_url
            .clone()
            .unwrap_or_else(|| self.provider.api_base.clone());
        let url = format!("{base}{}", prepared.path);
        let facts = CallFacts::new(&self.provider, url.clone(), request, origin);
        Ok((prepared, url, facts, dialect))
    }

    /// Waits before the next attempt, or says the failure is final.
    async fn backoff(
        &self,
        failure: &BackendFailure,
        attempt: u32,
        started: f64,
        retries: &dyn RetryObserver,
    ) -> bool {
        let budget_spent = self.clock().monotonic() - started >= self.budget;
        if budget_spent || !generic_is_retryable(failure) {
            return false;
        }
        let retry_after = match failure {
            BackendFailure::Backend(error) if error.source == BackendErrorSource::Status => error
                .headers
                .get("retry-after")
                .and_then(|value| generic_retry_after(value, self.clock().now())),
            _ => None,
        };
        let delay = next_delay(
            retry_after,
            attempt,
            GENERIC_DELAY,
            GENERIC_FACTOR,
            GENERIC_CAP,
        );
        let reason = RetryReason::for_failure(failure);
        if reason.category == RetryCategory::RateLimited {
            self.pacer.on_rate_limited();
        }
        retries.retrying(&reason);
        self.clock().sleep(delay, SleepKind::Retry).await;
        true
    }

    /// Sends one attempt and returns the answer when its status is a success.
    async fn send(
        &self,
        url: &str,
        prepared: &PreparedRequest,
        facts: &CallFacts,
    ) -> Result<HttpResponse, BackendFailure> {
        let body = serde_json::to_vec(&prepared.body)
            .map_err(|error| BackendFailure::local(LocalKind::Type, error.to_string()))?;
        let mut response = self
            .client
            .post(url, &prepared.headers, body)
            .await
            .map_err(|failure| facts.request_error(&failure))?;
        if !response.is_success() {
            let body = response
                .read_all()
                .await
                .map_err(|failure| facts.request_error(&failure))?;
            return Err(facts.status_error(BackendErrorSource::Status, &response, &body));
        }
        Ok(response)
    }

    /// One non-streaming call.
    ///
    /// # Errors
    ///
    /// The provider or the network refusing the call once retrying stopped,
    /// and an answer that cannot be read.
    pub(super) async fn complete(
        &self,
        request: &ModelRequest<'_>,
        retries: &dyn RetryObserver,
    ) -> Result<Chunk, BackendFailure> {
        let (prepared, url, facts, dialect) = self.prepare(request, false).await?;
        self.pacer.acquire().await;
        let started = self.clock().monotonic();
        let mut attempt = 0;
        let outcome = loop {
            let result = async {
                let mut response = self.send(&url, &prepared, &facts).await?;
                response
                    .read_all()
                    .await
                    .map_err(|failure| facts.request_error(&failure))
            }
            .await;
            match result {
                Ok(body) => break Ok(body),
                Err(failure) => {
                    if !self.backoff(&failure, attempt, started, retries).await {
                        break Err(failure);
                    }
                    attempt += 1;
                }
            }
        };
        match &outcome {
            Ok(_) => self.pacer.on_success(),
            Err(_) => self.pacer.on_failure(),
        }
        let body = outcome.map_err(|failure| facts.surface(failure))?;
        let text = String::from_utf8_lossy(&body);
        let data: Value = serde_json::from_str(&text).map_err(|error| {
            BackendFailure::local(LocalKind::Json, format!("the answer is not JSON: {error}"))
        })?;
        let mut chunk = dialect
            .reader()
            .parse(&data, &self.provider)
            .map_err(|failure| facts.surface(failure))?;
        if matches!(dialect, Dialect::Anthropic | Dialect::VertexAnthropic) {
            super::anthropic::reorder_tool_inputs(&mut chunk, &text);
        }
        Ok(chunk)
    }

    /// One streaming call.
    ///
    /// # Errors
    ///
    /// As [`GenericBackend::complete`], for everything that happens before the
    /// first chunk; later failures arrive through the stream.
    pub(super) async fn complete_streaming<'a>(
        &'a self,
        request: &ModelRequest<'_>,
        retries: &dyn RetryObserver,
    ) -> Result<ChunkStream<'a>, BackendFailure> {
        let (prepared, url, facts, dialect) = self.prepare(request, true).await?;
        self.pacer.acquire().await;
        let pacer = Arc::clone(&self.pacer);
        let settle = Settle::new(move |succeeded| {
            if succeeded {
                pacer.on_success();
            } else {
                pacer.on_failure();
            }
        });
        let started = self.clock().monotonic();
        let mut attempt = 0;
        loop {
            match self.open(&url, &prepared, &facts, dialect).await {
                Ok(pump) => {
                    return Ok(Box::pin(
                        pump.into_stream(settle)
                            .map(move |item| item.map_err(|failure| facts.surface(failure))),
                    ));
                }
                Err(failure) => {
                    if !self.backoff(&failure, attempt, started, retries).await {
                        settle.done(false);
                        return Err(facts.surface(failure));
                    }
                    attempt += 1;
                }
            }
        }
    }

    /// Sends one attempt and reads until the first chunk or the end.
    async fn open(
        &self,
        url: &str,
        prepared: &PreparedRequest,
        facts: &CallFacts,
        dialect: Dialect,
    ) -> Result<ChunkPump<LineSource>, BackendFailure> {
        let response = self.send(url, prepared, facts).await?;
        let transport_facts = facts.clone();
        let mut pump = ChunkPump::new(
            response.body,
            LineSource {
                splitter: Some(LineSplitter::default()),
                reader: dialect.reader(),
                provider: self.provider.clone(),
                done: false,
                deferred: None,
            },
            Box::new(move |failure| transport_facts.request_error(&failure)),
        );
        match pump.next().await? {
            Some(first) => {
                pump.push_front(first);
                Ok(pump)
            }
            None => Ok(pump),
        }
    }
}

/// Adds the loop's headers over the dialect's, a later name replacing an
/// earlier one whatever its case.
pub(super) fn merge_headers(headers: &mut Vec<(String, String)>, extra: &[(String, String)]) {
    for (name, value) in extra {
        if let Some(existing) = headers
            .iter_mut()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
        {
            existing.1.clone_from(value);
        } else {
            headers.push((name.clone(), value.clone()));
        }
    }
}

/// The generic backend's framing: `key: value` lines, `data` events, a
/// `[DONE]` sentinel.
struct LineSource {
    splitter: Option<LineSplitter>,
    reader: Reader,
    provider: ProviderConfig,
    done: bool,
    /// A failure met after chunks the same bytes completed, reported once
    /// those chunks are read, as a generator raises after its last yield.
    deferred: Option<BackendFailure>,
}

impl LineSource {
    fn lines(&mut self, lines: Vec<String>) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        for line in lines {
            if self.done || self.deferred.is_some() {
                break;
            }
            let outcome = match data_line(&line) {
                Ok(DataLine::Skip) => Ok(Vec::new()),
                Ok(DataLine::Done) => {
                    self.done = true;
                    Ok(Vec::new())
                }
                Ok(DataLine::Event(data)) => self.reader.stream_event(&data, &self.provider),
                Err(LineError::Format) => Err(BackendFailure::local(
                    LocalKind::Value,
                    "an event stream line is not formatted as `key: value`",
                )),
                Err(LineError::Json) => Err(BackendFailure::local(
                    LocalKind::Value,
                    "an event stream line carries malformed JSON",
                )),
            };
            match outcome {
                Ok(released) => chunks.extend(released),
                Err(failure) => self.deferred = Some(failure),
            }
        }
        chunks
    }
}

impl EventSource for LineSource {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Chunk>, BackendFailure> {
        let lines = self
            .splitter
            .as_mut()
            .map(|splitter| splitter.push(bytes))
            .unwrap_or_default();
        Ok(self.lines(lines))
    }

    fn finish(&mut self) -> Result<Vec<Chunk>, BackendFailure> {
        let mut chunks = Vec::new();
        if !self.done
            && let Some(line) = self.splitter.take().and_then(LineSplitter::finish)
        {
            chunks.extend(self.lines(vec![line]));
        }
        if self.deferred.is_some() {
            return Ok(chunks);
        }
        self.done = true;
        chunks.extend(self.reader.finish());
        Ok(chunks)
    }

    fn done(&self) -> bool {
        self.done
    }

    fn take_failure(&mut self) -> Option<BackendFailure> {
        self.deferred.take()
    }
}
