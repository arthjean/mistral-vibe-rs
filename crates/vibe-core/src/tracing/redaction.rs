//! What a span loses before it leaves the process.
//!
//! The reference wraps its exporter with `RedactingSpanExporter` from
//! `mistralai.extra.observability`, a Python dependency with no Rust
//! counterpart. What is reproduced here is the behavior that dependency
//! publishes and the `redaction` corpus family measures: which attribute keys
//! survive a mode, and which values are replaced.
//!
//! Two policies answer that. The content-oriented one leaves every key in place
//! and scans string values for credentials and personal data, which is what
//! `otel_redaction = default` installs. The key-oriented one replaces the value
//! of every key judged sensitive before scanning what remains, which is what
//! `strict` installs and what makes a prompt or a tool result impossible to
//! export at all.
//!
//! Neither policy adds or drops a key: a consumer counting attributes sees the
//! same set whatever the mode, and only the values differ.

use std::borrow::Cow;
use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::trace::Status;
use opentelemetry::{KeyValue, StringValue, Value};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use regex::Regex;

use super::OtelRedactionMode;

/// What replaces a redacted value.
pub const REDACTED_VALUE: &str = "[REDACTED]";

/// Keys whose value is content whatever it holds, replaced wholesale by the
/// key-oriented policy.
const SENSITIVE_KEYS: &[&str] = &[
    "client.address",
    "db.query.text",
    "db.statement",
    "exception.message",
    "exception.stacktrace",
    "gen_ai.input.messages",
    "gen_ai.output.messages",
    "gen_ai.tool.call.arguments",
    "gen_ai.tool.call.result",
    "gen_ai.tool.definitions",
    "http.request.body",
    "http.request.header.authorization",
    "http.request.header.cookie",
    "http.response.body",
    "http.response.header.set-cookie",
    "http.target",
    "http.url",
    "server.address",
    "url.full",
    "url.path",
    "url.query",
];

/// Words that make a key content-bearing wherever they appear in it.
const SENSITIVE_FRAGMENTS: &[&str] = &[
    "api_key",
    "argument",
    "arguments",
    "authorization",
    "body",
    "completion",
    "content",
    "cookie",
    "input",
    "message",
    "messages",
    "output",
    "password",
    "payload",
    "prompt",
    "secret",
    "set_cookie",
    "token",
];

/// Keys the key-oriented policy keeps whatever they are named after: the
/// identifiers and counters an observability consumer reads a trace by.
const SAFE_KEYS: &[&str] = &[
    "agent.trace.public",
    "client.port",
    "error.type",
    "exception.type",
    "gen_ai.agent.name",
    "gen_ai.conversation.id",
    "gen_ai.operation.name",
    "gen_ai.provider.name",
    "gen_ai.request.model",
    "gen_ai.response.finish_reasons",
    "gen_ai.response.id",
    "gen_ai.response.model",
    "gen_ai.tool.call.id",
    "gen_ai.tool.name",
    "gen_ai.tool.type",
    "http.request.method",
    "http.response.status_code",
    "network.protocol.name",
    "network.protocol.version",
    "server.port",
    "url.scheme",
];

/// Key prefixes the key-oriented policy keeps, so the usage counters survive
/// without naming each one.
const SAFE_KEY_PREFIXES: &[&str] = &["gen_ai.usage."];

/// Credential shapes, matched against every value both policies keep. Each is a
/// published token format rather than a guess, so a match is a credential and
/// not a coincidence.
const TOKEN_PATTERNS: &[&str] = &[
    r"(?i)bearer\s+[a-z0-9._\-]+",
    r"\bgh[pousr]_[A-Za-z0-9_]{20,}\b",
    r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b",
    r"\bsk-[A-Za-z0-9]{20,}\b",
    r"\bAKIA[0-9A-Z]{16}\b",
    r"\bAIza[0-9A-Za-z_\-]{35}\b",
    r"\beyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\b",
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
    r"\b[sr]k_(?:live|test)_[0-9A-Za-z]{10,}\b",
    r"\bsk-ant-[A-Za-z0-9\-_]{20,}\b",
    r"\bsk-proj-[A-Za-z0-9\-_]{20,}\b",
    r"\bhf_[A-Za-z0-9]{30,}\b",
    r"\bgithub_pat_[A-Za-z0-9_]{22,}\b",
    r"\bglpat-[A-Za-z0-9\-=_]{20,22}\b",
    r"\bshp(?:at|ca|pa|ss)_[a-fA-F0-9]{32}\b",
    r"\bsq0(?:atp|csp|idp)-[0-9A-Za-z\-_]{22,43}\b",
    r"\bPMAK-[a-zA-Z0-9]{24,59}\b",
    r"\bphc_[a-zA-Z0-9_]{43}\b",
    r"\brubygems_[a-f0-9]{48}\b",
    r"\blin_api_[0-9A-Za-z]{40}\b",
    r"pypi-AgEIcHlwaS5vcmc[A-Za-z0-9\-_]{50,}",
    r"\bsecret_[A-Za-z0-9]{43}\b",
    r"[A-Za-z0-9]{14}\.atlasv1\.[A-Za-z0-9]{60,}",
    r"\bSG\.[A-Za-z0-9_\-]{22}\.[A-Za-z0-9_\-]{43}\b",
    r"\bpk_(?:live|test)_[0-9a-zA-Z]{24}\b",
    r"https://hooks\.slack\.com/services/[A-Za-z0-9/+]{40,}",
    r"https://discord(?:app)?\.com/api/webhooks/[0-9]{17,}/[A-Za-z0-9\-_]{60,}",
    r"https://hooks\.zapier\.com/hooks/catch/[A-Za-z0-9/]{16,}",
];

/// Personal data the content-oriented policy removes on top of the credential
/// shapes.
const PII_PATTERNS: &[&str] = &[
    r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b",
    r"\b(?:\d[ -]?){13,16}\b",
    r"\b(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)\b",
];

/// The credential shapes, compiled once. A pattern that fails to compile is
/// dropped rather than panicking, and a test asserting every pattern compiles is
/// what keeps that from happening quietly.
static TOKEN_REGEXES: LazyLock<Vec<Regex>> = LazyLock::new(|| compile(TOKEN_PATTERNS));

/// The credential shapes plus the personal-data ones.
static CONTENT_REGEXES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    let mut patterns = compile(TOKEN_PATTERNS);
    patterns.extend(compile(PII_PATTERNS));
    patterns
});

fn compile(patterns: &[&str]) -> Vec<Regex> {
    patterns
        .iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect()
}

/// How much a span loses. Reference `default_redaction_policy` builds the
/// content-oriented one and `AttributeRedactionPolicy` the key-oriented one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionPolicy {
    /// `otel_redaction = default`: keys and structure are left intact and
    /// string values are scanned.
    Content,
    /// `otel_redaction = strict`: a sensitive key loses its value outright, and
    /// what is kept is still scanned.
    Key,
}

impl RedactionPolicy {
    /// The policy a mode installs, and [`None`] for the mode that installs
    /// none.
    #[must_use]
    pub const fn for_mode(mode: OtelRedactionMode) -> Option<Self> {
        match mode {
            OtelRedactionMode::None => None,
            OtelRedactionMode::Default => Some(Self::Content),
            OtelRedactionMode::Strict => Some(Self::Key),
        }
    }

    /// Every attribute, with the values this policy removes replaced. The key
    /// set is never changed.
    #[must_use]
    pub fn redact_attributes(self, attributes: Vec<KeyValue>) -> Vec<KeyValue> {
        attributes
            .into_iter()
            .map(|attribute| {
                if self == Self::Key && self.redacts_wholesale(&attribute) {
                    return KeyValue::new(attribute.key, REDACTED_VALUE);
                }
                let value = self.redact_value(attribute.value);
                KeyValue::new(attribute.key, value)
            })
            .collect()
    }

    /// The span name, scanned by the content-oriented policy and left alone by
    /// the key-oriented one, which reads a name as structure.
    #[must_use]
    pub fn redact_span_name(self, name: Cow<'static, str>) -> Cow<'static, str> {
        match self {
            Self::Content => Cow::Owned(self.redact_text(&name)),
            Self::Key => name,
        }
    }

    /// The status. A description often quotes a request or a response, so the
    /// key-oriented policy replaces it outright rather than scanning it.
    #[must_use]
    pub fn redact_status(self, status: Status) -> Status {
        match status {
            Status::Error { description } => Status::Error {
                description: match self {
                    Self::Content => Cow::Owned(self.redact_text(&description)),
                    Self::Key => Cow::Borrowed(REDACTED_VALUE),
                },
            },
            other => other,
        }
    }

    /// Reference `_should_redact`: a safe key is kept, a sensitive one is
    /// replaced, a key carrying a sensitive word is replaced, and anything
    /// whose value is not a scalar is replaced for being structure this policy
    /// cannot read.
    fn redacts_wholesale(self, attribute: &KeyValue) -> bool {
        let key = attribute.key.as_str().to_lowercase();
        if SAFE_KEYS.contains(&key.as_str())
            || SAFE_KEY_PREFIXES
                .iter()
                .any(|prefix| key.starts_with(prefix))
        {
            return false;
        }
        if SENSITIVE_KEYS.contains(&key.as_str()) {
            return true;
        }
        if has_sensitive_fragment(&key) {
            return true;
        }
        matches!(attribute.value, Value::Array(_))
    }

    fn patterns(self) -> &'static [Regex] {
        match self {
            Self::Content => &CONTENT_REGEXES,
            Self::Key => &TOKEN_REGEXES,
        }
    }

    fn redact_value(self, value: Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.redact_text(text.as_str()).into()),
            Value::Array(opentelemetry::Array::String(items)) => Value::Array(
                items
                    .iter()
                    .map(|item| StringValue::from(self.redact_text(item.as_str())))
                    .collect::<Vec<_>>()
                    .into(),
            ),
            other => other,
        }
    }

    fn redact_text(self, text: &str) -> String {
        let mut redacted = Cow::Borrowed(text);
        for pattern in self.patterns() {
            if pattern.is_match(&redacted) {
                redacted = Cow::Owned(pattern.replace_all(&redacted, REDACTED_VALUE).into_owned());
            }
        }
        redacted.into_owned()
    }
}

/// Reference `_has_sensitive_fragment`: the key is read both as a whole and as
/// the words separators split it into, so `vibe.request.payload` and
/// `request_payload` are judged the same.
fn has_sensitive_fragment(key: &str) -> bool {
    let words = key.replace(['-', '.'], "_");
    let fragments = words
        .split('_')
        .filter(|fragment| !fragment.is_empty())
        .collect::<Vec<_>>();
    SENSITIVE_FRAGMENTS
        .iter()
        .any(|sensitive| fragments.contains(sensitive) || words.contains(sensitive))
}

/// The exporter every other one is wrapped in when redaction is on. Reference
/// `RedactingSpanExporter`: the spans reaching the inner exporter are the
/// redacted ones, and nothing it answers reaches the span-producing code.
#[derive(Debug)]
pub struct RedactingSpanExporter<E> {
    inner: E,
    policy: RedactionPolicy,
}

impl<E> RedactingSpanExporter<E> {
    pub const fn new(inner: E, policy: RedactionPolicy) -> Self {
        Self { inner, policy }
    }

    /// One span with its attributes, its events, its links, its name and its
    /// status redacted.
    fn redact(&self, mut span: SpanData) -> SpanData {
        span.name = self.policy.redact_span_name(span.name);
        span.attributes = self.policy.redact_attributes(span.attributes);
        span.status = self.policy.redact_status(span.status);
        for event in &mut span.events.events {
            event.attributes = self
                .policy
                .redact_attributes(std::mem::take(&mut event.attributes));
        }
        for link in &mut span.links.links {
            link.attributes = self
                .policy
                .redact_attributes(std::mem::take(&mut link.attributes));
        }
        span
    }
}

impl<E: SpanExporter> SpanExporter for RedactingSpanExporter<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let redacted = batch.into_iter().map(|span| self.redact(span)).collect();
        self.inner.export(redacted).await
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}
