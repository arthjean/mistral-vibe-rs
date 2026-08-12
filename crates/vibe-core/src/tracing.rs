//! The OpenTelemetry surface: the exporter this build installs, the four span
//! families it opens, and the attributes each one carries.
//!
//! Reference `vibe/core/tracing.py` holds all of it in one module, and this one
//! keeps that shape. `enable_otel`, `otel_endpoint` and `otel_redaction` are the
//! three configuration keys that decide whether anything is installed at all;
//! before this module they were declared and read by nothing.
//!
//! Two rules run through everything here. Tracing never changes an outcome: a
//! span that cannot be created leaves the body running and the caller none the
//! wiser, which is what the reference's `_safe_span` buys with its
//! `INVALID_SPAN` fallback and this port gets from a non-recording span. And
//! nothing content-bearing leaves the process unfiltered: [`redaction`] wraps
//! the exporter unless `otel_redaction` is `none`.
//!
//! The conversation id travels as baggage rather than as an argument, so a span
//! opened deep inside a turn carries the session it belongs to without every
//! layer between threading it through. [`agent_span`] sets it; every other
//! family reads it back.

#![expect(
    deprecated,
    reason = "`opentelemetry-semantic-conventions` 0.32.1 deprecates the whole `gen_ai.*` family \
              for having moved to the GenAI semantic-conventions repository, without changing a \
              single key string. Reading the constants keeps the keys attributed to the crate that \
              publishes them, and the `spans` corpus family fails the moment one of them changes"
)]

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;

use opentelemetry::baggage::BaggageExt;
use opentelemetry::trace::{FutureExt, Status, TraceContextExt, Tracer as _, TracerProvider as _};
use opentelemetry::{Context, InstrumentationScope, KeyValue, global};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{BatchSpanProcessor, SdkTracerProvider};
use opentelemetry_semantic_conventions::attribute as semconv;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use toml::Table;
use url::Url;

use crate::telemetry::{
    TELEMETRY_AUTHORIZATION_SCHEME, TELEMETRY_DEFAULT_API_KEY_VARIABLE, TELEMETRY_DEFAULT_BASE_URL,
    mistral_provider, server_url_from_api_base,
};

/// The version the tracer and the exported resource report.
const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod redaction;

// --------------------------------------------------------------------------
// The constants the reference declares
// --------------------------------------------------------------------------

/// Reference `VIBE_TRACER_NAME`.
pub const TRACER_NAME: &str = "mistral_vibe";
/// Reference `VIBE_AGENT_NAME`, which names both the agent span and the
/// exported service.
pub const AGENT_NAME: &str = "mistral-vibe";
/// The path a Mistral server publishes its collector under. Reference
/// `MISTRAL_OTEL_PATH`.
pub const MISTRAL_OTEL_PATH: &str = "/telemetry";
/// Reference `DEFAULT_TRACES_EXPORT_PATH`, published by the OTLP HTTP exporter
/// and joined onto whichever endpoint is resolved.
pub const TRACES_EXPORT_PATH: &str = "v1/traces";
/// The server the exporter derives from when no provider supplies one.
/// Reference `DEFAULT_MISTRAL_SERVER_URL`.
pub const DEFAULT_MISTRAL_SERVER_URL: &str = TELEMETRY_DEFAULT_BASE_URL;

/// Reference `GenAiOperationNameValues.INVOKE_AGENT`. The value vocabularies are
/// absent from `opentelemetry-semantic-conventions` 0.32, which publishes keys
/// only, so the three operation names are declared here and held to the corpus.
pub const OPERATION_INVOKE_AGENT: &str = "invoke_agent";
/// Reference `GenAiOperationNameValues.EXECUTE_TOOL`.
pub const OPERATION_EXECUTE_TOOL: &str = "execute_tool";
/// Reference `GenAiOperationNameValues.CHAT`.
pub const OPERATION_CHAT: &str = "chat";
/// Reference `GenAiProviderNameValues.MISTRAL_AI`.
pub const MISTRAL_PROVIDER_VALUE: &str = "mistral_ai";
/// What a provider name that normalizes to nothing resolves to.
pub const UNKNOWN_PROVIDER_VALUE: &str = "unknown";
/// The only tool type the reference reports.
pub const TOOL_TYPE_FUNCTION: &str = "function";

/// Reference `VIBE_PROVIDER_API_STYLE`.
pub const PROVIDER_API_STYLE_KEY: &str = "vibe.provider.api_style";
/// Reference `VIBE_REQUEST_CALL_TYPE`.
pub const REQUEST_CALL_TYPE_KEY: &str = "vibe.request.call_type";
/// Reference `VIBE_REQUEST_MESSAGE_ID`.
pub const REQUEST_MESSAGE_ID_KEY: &str = "vibe.request.message_id";
/// Reference `VIBE_REQUEST_STREAMING`.
pub const REQUEST_STREAMING_KEY: &str = "vibe.request.streaming";
/// The hook span's own attribute keys, which the semantic conventions do not
/// publish either.
pub const HOOK_NAME_KEY: &str = "vibe.hook.name";
/// As above.
pub const HOOK_TYPE_KEY: &str = "vibe.hook.type";

// --------------------------------------------------------------------------
// The exporter
// --------------------------------------------------------------------------

/// How much of a span survives export. Reference `OtelRedactionMode`, published
/// here as the `otel_redaction` choices in `crate::config::registry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OtelRedactionMode {
    /// Nothing is removed.
    None,
    /// Values are scanned for credentials and personal data. The published
    /// default.
    #[default]
    Default,
    /// Every content-bearing key is replaced wholesale, then what remains is
    /// scanned.
    Strict,
}

impl OtelRedactionMode {
    /// The mode a document names. An unreadable value resolves to the published
    /// default rather than to no redaction, so a typo never widens what leaves
    /// the process.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "none" => Self::None,
            "strict" => Self::Strict,
            _ => Self::Default,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Default => "default",
            Self::Strict => "strict",
        }
    }
}

/// Where spans are exported and what the request carries. Reference
/// `OtelSpanExporterConfig`.
#[derive(Debug, Clone)]
pub struct OtelExporterConfig {
    pub endpoint: String,
    /// Held as secrets: the header set is named on the wire and in the corpus,
    /// never its values.
    pub headers: BTreeMap<String, SecretString>,
}

impl OtelExporterConfig {
    /// The header names the request carries, which is what a capture records.
    #[must_use]
    pub fn header_names(&self) -> Vec<String> {
        self.headers.keys().cloned().collect()
    }
}

/// Reference `build_otel_span_exporter_config`: an explicit endpoint is the
/// user's own collector and authenticates itself, and everything else is derived
/// from the Mistral provider the configuration names.
///
/// [`None`] means the derived branch found no credential, which is the one case
/// the reference warns about before declining to install a provider.
#[must_use]
pub fn build_span_exporter_config(
    otel_endpoint: &str,
    effective: &Table,
    credentials: &dyn Fn(&str) -> Option<String>,
) -> Option<OtelExporterConfig> {
    if !otel_endpoint.is_empty() {
        return Some(OtelExporterConfig {
            endpoint: join_traces_path(otel_endpoint)?,
            headers: BTreeMap::new(),
        });
    }
    let provider = mistral_provider(effective);
    let server = provider
        .as_ref()
        .and_then(|provider| provider.get("api_base"))
        .and_then(toml::Value::as_str)
        .and_then(server_url_from_api_base)
        .unwrap_or_else(|| DEFAULT_MISTRAL_SERVER_URL.to_owned());
    let variable = otel_credential_variable(effective);
    let collector = Url::parse(&server).ok()?.join(MISTRAL_OTEL_PATH).ok()?;
    let endpoint = join_traces_path(collector.as_str())?;
    let credential = credentials(&variable).filter(|value| !value.is_empty())?;
    Some(OtelExporterConfig {
        endpoint,
        headers: BTreeMap::from([(
            "Authorization".to_owned(),
            SecretString::from(format!("{TELEMETRY_AUTHORIZATION_SCHEME} {credential}")),
        )]),
    })
}

/// The variable the derived exporter reads its credential from. Reference
/// `mistral_provider.api_key_env_var or DEFAULT_MISTRAL_API_ENV_KEY`, which is
/// also the variable the missing-credential warning names.
#[must_use]
pub fn otel_credential_variable(effective: &Table) -> String {
    mistral_provider(effective)
        .as_ref()
        .and_then(|provider| provider.get("api_key_env_var"))
        .and_then(toml::Value::as_str)
        .filter(|variable| !variable.is_empty())
        .unwrap_or(TELEMETRY_DEFAULT_API_KEY_VARIABLE)
        .to_owned()
}

/// The reference's `urljoin(f"{base.rstrip('/')}/", traces_export_path)`: the
/// export path lands under the base rather than replacing its last segment.
fn join_traces_path(base: &str) -> Option<String> {
    let base = format!("{}/", base.trim_end_matches('/'));
    Some(
        Url::parse(&base)
            .ok()?
            .join(TRACES_EXPORT_PATH)
            .ok()?
            .into(),
    )
}

/// What [`setup_tracing`] did, so the caller can report a degradation instead of
/// discovering it as silence.
pub enum TracingSetup {
    /// `enable_telemetry` or `enable_otel` is off, so nothing was constructed.
    Disabled,
    /// The derived exporter found no credential under the named variable.
    /// Reference logs a warning naming it and returns; startup continues either
    /// way.
    MissingCredential { variable: String },
    /// `otel_endpoint` names something no request can be sent to. The reference
    /// has no counterpart because Python's `urljoin` accepts anything; here a
    /// collector that cannot be parsed is reported as itself rather than as a
    /// missing credential it has nothing to do with.
    UnusableEndpoint { endpoint: String },
    /// A provider is installed and exports until the guard is dropped.
    Installed(TracingGuard),
}

impl TracingSetup {
    /// Whether a tracer provider is installed, which is what a capture records.
    #[must_use]
    pub const fn is_installed(&self) -> bool {
        matches!(self, Self::Installed(_))
    }
}

/// The installed provider, shut down when dropped. Reference registers
/// `provider.shutdown` with `atexit`; a guard held by the binary is this port's
/// equivalent, and it flushes what a batch is still holding.
pub struct TracingGuard {
    provider: SdkTracerProvider,
}

impl fmt::Debug for TracingGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TracingGuard")
    }
}

impl TracingGuard {
    /// Flushes and shuts the provider down ahead of the drop, for a caller that
    /// wants the failure rather than the silence.
    pub fn shutdown(&self) -> Result<(), String> {
        self.provider.shutdown().map_err(|error| error.to_string())
    }
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        drop(self.provider.shutdown());
    }
}

/// Reference `setup_tracing`: the two-key gate, the resolved exporter, the
/// redaction wrap and the installed provider.
///
/// The exporter is only constructed once both keys are on, which is what keeps a
/// build with tracing off from allocating one.
pub fn setup_tracing(
    effective: &Table,
    credentials: &dyn Fn(&str) -> Option<String>,
) -> TracingSetup {
    let enabled = |key: &str, fallback: bool| {
        effective
            .get(key)
            .and_then(toml::Value::as_bool)
            .unwrap_or(fallback)
    };
    if !enabled("enable_telemetry", true) || !enabled("enable_otel", false) {
        return TracingSetup::Disabled;
    }
    let endpoint = effective
        .get("otel_endpoint")
        .and_then(toml::Value::as_str)
        .unwrap_or_default();
    let Some(config) = build_span_exporter_config(endpoint, effective, credentials) else {
        return if endpoint.is_empty() {
            TracingSetup::MissingCredential {
                variable: otel_credential_variable(effective),
            }
        } else {
            TracingSetup::UnusableEndpoint {
                endpoint: endpoint.to_owned(),
            }
        };
    };
    let mode = OtelRedactionMode::parse(
        effective
            .get("otel_redaction")
            .and_then(toml::Value::as_str)
            .unwrap_or_default(),
    );
    let Some(exporter) = build_otlp_exporter(&config) else {
        return TracingSetup::UnusableEndpoint {
            endpoint: config.endpoint,
        };
    };
    let provider = match redaction::RedactionPolicy::for_mode(mode) {
        Some(policy) => install(
            BatchSpanProcessor::builder(redaction::RedactingSpanExporter::new(exporter, policy))
                .build(),
        ),
        None => install(BatchSpanProcessor::builder(exporter).build()),
    };
    global::set_tracer_provider(provider.clone());
    TracingSetup::Installed(TracingGuard { provider })
}

fn install<P: opentelemetry_sdk::trace::SpanProcessor + 'static>(
    processor: P,
) -> SdkTracerProvider {
    SdkTracerProvider::builder()
        .with_resource(
            Resource::builder()
                .with_service_name(AGENT_NAME)
                .with_attribute(KeyValue::new("service.version", VERSION))
                .build(),
        )
        .with_span_processor(processor)
        .build()
}

fn build_otlp_exporter(config: &OtelExporterConfig) -> Option<opentelemetry_otlp::SpanExporter> {
    use opentelemetry_otlp::{WithExportConfig as _, WithHttpConfig as _};

    let mut builder = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(&config.endpoint);
    if !config.headers.is_empty() {
        builder = builder.with_headers(
            config
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.expose_secret().to_owned()))
                .collect(),
        );
    }
    builder.build().ok()
}

// --------------------------------------------------------------------------
// What a failing body tells its span
// --------------------------------------------------------------------------

/// A backend failure carried by an error, which is what decides whether a span
/// reports a provider status or an exception. Reference `BackendError`, reached
/// through the cause walk of `_backend_error_from`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendFailure {
    /// The provider the failure came from. Absent when the error names none, in
    /// which case the span supplies its own.
    pub provider: Option<String>,
    pub status: Option<i64>,
}

/// What a failing body tells its span. Reference `_exception_status` reads three
/// things off an exception: its class, its message and whether its cause chain
/// carries a `BackendError`.
pub trait TracedError: fmt::Display {
    /// Reference `type(exc).__name__`.
    fn error_type(&self) -> &'static str;

    /// Reference `_backend_error_from`: the backend failure this error carries,
    /// including through whatever wraps it.
    fn backend_failure(&self) -> Option<BackendFailure> {
        None
    }
}

impl TracedError for String {
    fn error_type(&self) -> &'static str {
        "String"
    }
}

/// Reference `_exception_status`: a backend failure reports the provider and
/// the status and never the raw message, a model call that declines to record
/// reports the class alone, and everything else reports the message and records
/// an exception.
fn exception_status<E: TracedError + ?Sized>(
    error: &E,
    span_provider: Option<&str>,
    record_unhandled_exception: bool,
) -> (String, bool) {
    if let Some(failure) = error.backend_failure() {
        let provider = failure
            .provider
            .as_deref()
            .or(span_provider)
            .unwrap_or(UNKNOWN_PROVIDER_VALUE)
            .to_owned();
        let status = failure
            .status
            .map_or_else(|| "N/A".to_owned(), |status| status.to_string());
        return (
            format!("BackendError(provider={provider}, status={status})"),
            false,
        );
    }
    if !record_unhandled_exception {
        return (
            format!("{} raised during model call.", error.error_type()),
            false,
        );
    }
    (error.to_string(), true)
}

// --------------------------------------------------------------------------
// The spans
// --------------------------------------------------------------------------

fn scope() -> InstrumentationScope {
    InstrumentationScope::builder(TRACER_NAME)
        .with_version(VERSION)
        .build()
}

/// Reference `_safe_span`: the body runs whatever the tracer answers, the status
/// is decided by the outcome, and no tracing failure reaches the caller.
///
/// Span creation is infallible in Rust and export failures surface only on
/// flush, so the reference's `INVALID_SPAN` fallback is what a non-recording
/// span already is here.
async fn safe_span<T, E, F>(
    parent: Context,
    name: String,
    attributes: Vec<KeyValue>,
    provider: Option<&str>,
    record_unhandled_exception: bool,
    body: F,
) -> Result<T, E>
where
    E: TracedError,
    F: Future<Output = Result<T, E>>,
{
    let tracer = global::tracer_provider().tracer_with_scope(scope());
    let span = tracer
        .span_builder(name)
        .with_attributes(attributes)
        .start_with_context(&tracer, &parent);
    let context = parent.with_span(span);
    let outcome = body.with_context(context.clone()).await;
    let span = context.span();
    match &outcome {
        Ok(_) => span.set_status(Status::Ok),
        Err(error) => {
            let (description, record) =
                exception_status(error, provider, record_unhandled_exception);
            span.set_status(Status::error(description));
            if record {
                span.add_event(
                    "exception",
                    vec![
                        KeyValue::new("exception.type", error.error_type()),
                        KeyValue::new("exception.message", error.to_string()),
                    ],
                );
            }
        }
    }
    span.end();
    outcome
}

/// The conversation id a parent span left in baggage. Reference
/// `baggage.get_baggage(GEN_AI_CONVERSATION_ID)`.
fn conversation_from_baggage(context: &Context) -> Option<String> {
    context
        .baggage()
        .get(semconv::GEN_AI_CONVERSATION_ID)
        .map(ToString::to_string)
}

/// What a turn reports about itself. Reference `agent_span`.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgentSpan<'a> {
    pub model: Option<&'a str>,
    pub session_id: Option<&'a str>,
}

/// Reference `agent_span`: one span per turn, and the conversation id it
/// publishes in baggage for every descendant.
pub async fn agent_span<T, E, F>(span: AgentSpan<'_>, body: F) -> Result<T, E>
where
    E: TracedError,
    F: Future<Output = Result<T, E>>,
{
    let mut attributes = vec![
        KeyValue::new(semconv::GEN_AI_OPERATION_NAME, OPERATION_INVOKE_AGENT),
        KeyValue::new(semconv::GEN_AI_PROVIDER_NAME, MISTRAL_PROVIDER_VALUE),
        KeyValue::new(semconv::GEN_AI_AGENT_NAME, AGENT_NAME),
    ];
    if let Some(model) = span.model.filter(|model| !model.is_empty()) {
        attributes.push(KeyValue::new(
            semconv::GEN_AI_REQUEST_MODEL,
            model.to_owned(),
        ));
    }
    let mut parent = Context::current();
    if let Some(session) = span.session_id.filter(|session| !session.is_empty()) {
        attributes.push(KeyValue::new(
            semconv::GEN_AI_CONVERSATION_ID,
            session.to_owned(),
        ));
        parent = parent.with_baggage(vec![KeyValue::new(
            semconv::GEN_AI_CONVERSATION_ID,
            session.to_owned(),
        )]);
    }
    safe_span(
        parent,
        format!("{OPERATION_INVOKE_AGENT} {AGENT_NAME}"),
        attributes,
        Some(MISTRAL_PROVIDER_VALUE),
        true,
        body,
    )
    .await
}

/// What a tool call reports. Reference `tool_span`.
#[derive(Debug, Clone, Copy)]
pub struct ToolSpan<'a> {
    pub tool_name: &'a str,
    pub call_id: &'a str,
    pub arguments: &'a str,
}

/// Reference `tool_span`.
pub async fn tool_span<T, E, F>(span: ToolSpan<'_>, body: F) -> Result<T, E>
where
    E: TracedError,
    F: Future<Output = Result<T, E>>,
{
    let parent = Context::current();
    let mut attributes = vec![
        KeyValue::new(semconv::GEN_AI_OPERATION_NAME, OPERATION_EXECUTE_TOOL),
        KeyValue::new(semconv::GEN_AI_TOOL_NAME, span.tool_name.to_owned()),
        KeyValue::new(semconv::GEN_AI_TOOL_CALL_ID, span.call_id.to_owned()),
        KeyValue::new(
            semconv::GEN_AI_TOOL_CALL_ARGUMENTS,
            span.arguments.to_owned(),
        ),
        KeyValue::new(semconv::GEN_AI_TOOL_TYPE, TOOL_TYPE_FUNCTION),
    ];
    if let Some(conversation) = conversation_from_baggage(&parent) {
        attributes.push(KeyValue::new(semconv::GEN_AI_CONVERSATION_ID, conversation));
    }
    safe_span(
        parent,
        format!("{OPERATION_EXECUTE_TOOL} {}", span.tool_name),
        attributes,
        None,
        true,
        body,
    )
    .await
}

/// What one backend request reports. Reference `model_call_span`, whose
/// `metadata` mapping is spelled out here as the three fields it is ever read
/// for.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelCallSpan<'a> {
    pub provider_name: &'a str,
    pub provider_api_style: &'a str,
    pub model: &'a str,
    pub streaming: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i64>,
    /// The conversation id to fall back on when no parent published one.
    pub session_id: Option<&'a str>,
    pub call_type: Option<&'a str>,
    pub message_id: Option<&'a str>,
    /// Absent means `POST`, which is the reference's default.
    pub http_method: Option<&'a str>,
    pub http_url: Option<&'a str>,
}

/// Reference `model_call_span`. A failing model call reports the backend status
/// rather than the exception, which is the one family that declines to record
/// an unhandled one.
pub async fn model_call_span<T, E, F>(span: ModelCallSpan<'_>, body: F) -> Result<T, E>
where
    E: TracedError,
    F: Future<Output = Result<T, E>>,
{
    let parent = Context::current();
    let provider = provider_attribute_value(Some(span.provider_name));
    let mut attributes = vec![
        KeyValue::new(semconv::GEN_AI_OPERATION_NAME, OPERATION_CHAT),
        KeyValue::new(semconv::GEN_AI_PROVIDER_NAME, provider.clone()),
        KeyValue::new(semconv::GEN_AI_REQUEST_MODEL, span.model.to_owned()),
        KeyValue::new(PROVIDER_API_STYLE_KEY, span.provider_api_style.to_owned()),
        KeyValue::new(REQUEST_STREAMING_KEY, span.streaming),
    ];
    if let Some(temperature) = span.temperature {
        attributes.push(KeyValue::new(
            semconv::GEN_AI_REQUEST_TEMPERATURE,
            temperature,
        ));
    }
    if let Some(max_tokens) = span.max_tokens {
        attributes.push(KeyValue::new(
            semconv::GEN_AI_REQUEST_MAX_TOKENS,
            max_tokens,
        ));
    }
    if let Some(url) = span.http_url.filter(|url| !url.is_empty()) {
        attributes.push(KeyValue::new(
            semconv::HTTP_REQUEST_METHOD,
            span.http_method.unwrap_or("POST").to_owned(),
        ));
        attributes.push(KeyValue::new(semconv::HTTP_URL, url.to_owned()));
    }
    if let Some(conversation) = conversation_from_baggage(&parent)
        .or_else(|| span.session_id.map(ToOwned::to_owned))
        .filter(|conversation| !conversation.is_empty())
    {
        attributes.push(KeyValue::new(semconv::GEN_AI_CONVERSATION_ID, conversation));
    }
    if let Some(call_type) = span.call_type.filter(|value| !value.is_empty()) {
        attributes.push(KeyValue::new(REQUEST_CALL_TYPE_KEY, call_type.to_owned()));
    }
    if let Some(message_id) = span.message_id.filter(|value| !value.is_empty()) {
        attributes.push(KeyValue::new(REQUEST_MESSAGE_ID_KEY, message_id.to_owned()));
    }
    safe_span(
        parent,
        format!("{OPERATION_CHAT} {}", span.model),
        attributes,
        Some(&provider),
        false,
        body,
    )
    .await
}

/// What one hook run reports. Reference `hook_span`.
#[derive(Debug, Clone, Copy)]
pub struct HookSpan<'a> {
    pub hook_name: &'a str,
    pub hook_type: &'a str,
    pub tool_name: Option<&'a str>,
    pub tool_call_id: Option<&'a str>,
}

/// Reference `hook_span`.
pub async fn hook_span<T, E, F>(span: HookSpan<'_>, body: F) -> Result<T, E>
where
    E: TracedError,
    F: Future<Output = Result<T, E>>,
{
    let parent = Context::current();
    let mut attributes = vec![
        KeyValue::new(HOOK_NAME_KEY, span.hook_name.to_owned()),
        KeyValue::new(HOOK_TYPE_KEY, span.hook_type.to_owned()),
    ];
    if let Some(tool) = span.tool_name {
        attributes.push(KeyValue::new(semconv::GEN_AI_TOOL_NAME, tool.to_owned()));
    }
    if let Some(call) = span.tool_call_id {
        attributes.push(KeyValue::new(semconv::GEN_AI_TOOL_CALL_ID, call.to_owned()));
    }
    if let Some(conversation) = conversation_from_baggage(&parent) {
        attributes.push(KeyValue::new(semconv::GEN_AI_CONVERSATION_ID, conversation));
    }
    safe_span(
        parent,
        format!("hook {} {}", span.hook_type, span.hook_name),
        attributes,
        None,
        true,
        body,
    )
    .await
}

// --------------------------------------------------------------------------
// What a returning call attaches to the span it ran under
// --------------------------------------------------------------------------

/// Reference `set_model_call_http_status`.
pub fn set_model_call_http_status(status: u16) {
    with_active_span(|span| {
        span.set_attribute(KeyValue::new(
            semconv::HTTP_RESPONSE_STATUS_CODE,
            i64::from(status),
        ));
    });
}

/// Reference `set_model_call_usage`.
pub fn set_model_call_usage(prompt_tokens: u64, completion_tokens: u64) {
    let prompt = i64::try_from(prompt_tokens).unwrap_or(i64::MAX);
    let completion = i64::try_from(completion_tokens).unwrap_or(i64::MAX);
    with_active_span(|span| {
        span.set_attribute(KeyValue::new(semconv::GEN_AI_USAGE_INPUT_TOKENS, prompt));
        span.set_attribute(KeyValue::new(
            semconv::GEN_AI_USAGE_OUTPUT_TOKENS,
            completion,
        ));
    });
}

/// Reference `set_tool_result`.
pub fn set_tool_result(result: &str) {
    with_active_span(|span| {
        span.set_attribute(KeyValue::new(
            semconv::GEN_AI_TOOL_CALL_RESULT,
            result.to_owned(),
        ));
    });
}

/// Reference `set_model_call_response_metadata`: the model, the id and the
/// finish reasons, each taken from the first source that supplies it.
pub fn set_model_call_response_metadata(response: &Value) {
    let sources = response_metadata_sources(response);
    let model = sources
        .iter()
        .find_map(|source| string_attribute(source, "model"));
    let id = sources
        .iter()
        .find_map(|source| string_attribute(source, "id"));
    let reasons = response_finish_reasons(response);
    with_active_span(|span| {
        if let Some(model) = model.clone() {
            span.set_attribute(KeyValue::new(semconv::GEN_AI_RESPONSE_MODEL, model));
        }
        if let Some(id) = id.clone() {
            span.set_attribute(KeyValue::new(semconv::GEN_AI_RESPONSE_ID, id));
        }
        if !reasons.is_empty() {
            span.set_attribute(KeyValue::new(
                semconv::GEN_AI_RESPONSE_FINISH_REASONS,
                opentelemetry::Value::Array(
                    reasons
                        .iter()
                        .map(|reason| reason.clone().into())
                        .collect::<Vec<opentelemetry::StringValue>>()
                        .into(),
                ),
            ));
        }
    });
}

/// The span the current context carries, if it is recording. A body running
/// without a provider installed reaches a non-recording span, where every setter
/// is a no-op.
fn with_active_span(apply: impl FnOnce(&opentelemetry::trace::SpanRef<'_>)) {
    let context = Context::current();
    let span = context.span();
    if span.is_recording() {
        apply(&span);
    }
}

/// Reference `_string_attribute`: a non-empty string, and nothing else.
fn string_attribute(source: &Value, key: &str) -> Option<String> {
    source
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Reference `_response_metadata_sources`: the body, then its `message` and
/// `response` members.
fn response_metadata_sources(response: &Value) -> Vec<&Value> {
    let mut sources = vec![response];
    for key in ["message", "response"] {
        if let Some(source) = response.get(key).filter(|value| value.is_object()) {
            sources.push(source);
        }
    }
    sources
}

/// Reference `_response_finish_reasons`: every choice's reason, then the body's
/// own, its `delta`'s and its `response`'s.
fn response_finish_reasons(response: &Value) -> Vec<String> {
    let mut reasons = Vec::new();
    if let Some(choices) = response.get("choices").and_then(Value::as_array) {
        for choice in choices {
            if let Some(reason) = string_attribute(choice, "finish_reason") {
                reasons.push(reason);
            }
        }
    }
    for source in [
        Some(response),
        response.get("delta").filter(|value| value.is_object()),
        response.get("response").filter(|value| value.is_object()),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(reason) = string_attribute(source, "finish_reason")
            .or_else(|| string_attribute(source, "stop_reason"))
        {
            reasons.push(reason);
        }
    }
    reasons
}

// --------------------------------------------------------------------------
// The provider vocabulary
// --------------------------------------------------------------------------

/// The seven names the reference folds onto a semantic-convention value. Every
/// other name travels normalized, which is why an unrecognized provider is still
/// reported rather than hidden behind `unknown`.
const PROVIDER_ALIASES: &[(&str, &str)] = &[
    ("mistral", MISTRAL_PROVIDER_VALUE),
    ("mistral_ai", MISTRAL_PROVIDER_VALUE),
    ("google_vertex", "gcp.vertex_ai"),
    ("vertex", "gcp.vertex_ai"),
    ("google", "gcp.gen_ai"),
    ("azure_openai", "azure.ai.openai"),
    ("bedrock", "aws.bedrock"),
    ("xai", "x_ai"),
];

/// Reference `_provider_attribute_value`: trimmed, lowercased, hyphens folded to
/// underscores, the aliases applied, and `unknown` for a name that normalizes to
/// nothing.
#[must_use]
pub fn provider_attribute_value(provider_name: Option<&str>) -> String {
    let normalized = provider_name
        .unwrap_or_default()
        .trim()
        .to_lowercase()
        .replace('-', "_");
    if let Some((_, value)) = PROVIDER_ALIASES
        .iter()
        .find(|(alias, _)| *alias == normalized)
    {
        return (*value).to_owned();
    }
    if normalized.is_empty() {
        return UNKNOWN_PROVIDER_VALUE.to_owned();
    }
    normalized
}

#[cfg(test)]
pub(crate) mod harness;

#[cfg(test)]
mod tracing_tests;
