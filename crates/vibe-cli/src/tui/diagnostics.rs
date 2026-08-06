//! Actionable failures, live turn activity, and inspectable diagnostics.
//!
//! Errors are classified into the reference semantic classes so severity,
//! deduplication, and recovery guidance are decided once; activity, context
//! usage, debug entries, and links are formatted the way the reference
//! presents them. Every function here is pure: no clock, no filesystem, no
//! network.

use std::collections::VecDeque;

use serde_json::Value;

const MAX_TRACKED_ERRORS: usize = 32;
pub const DEFAULT_ACTIVITY_STATUS: &str = "Generating";

/// Reference turn-error classes. `Benign` classes are the ones the reference
/// resolves into guidance instead of reporting as a defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Cancellation,
    ContextExhaustion,
    Refusal,
    ResponseTooLong,
    Auth,
    RateLimit,
    Transport,
    Model,
    Server,
    Shell,
    Tool,
    Unknown,
}

impl ErrorClass {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Cancellation => "cancellation",
            Self::ContextExhaustion => "context_too_long",
            Self::Refusal => "refusal",
            Self::ResponseTooLong => "response_too_long",
            Self::Auth => "auth",
            Self::RateLimit => "rate_limit",
            Self::Transport => "transport",
            Self::Model => "model",
            Self::Server => "server",
            Self::Shell => "shell",
            Self::Tool => "tool",
            Self::Unknown => "unknown",
        }
    }

    /// Reference `_BENIGN_TURN_ERROR_CODES` plus cancellation: expected
    /// outcomes that carry guidance rather than a defect report.
    #[must_use]
    pub fn is_benign(self) -> bool {
        matches!(
            self,
            Self::RateLimit
                | Self::ContextExhaustion
                | Self::ResponseTooLong
                | Self::Refusal
                | Self::Cancellation
        )
    }

    fn severity(self) -> Severity {
        match self {
            Self::Cancellation => Severity::Notice,
            Self::RateLimit | Self::ContextExhaustion | Self::ResponseTooLong | Self::Refusal => {
                Severity::Warning
            }
            _ => Severity::Error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Notice,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedError {
    pub class: ErrorClass,
    pub severity: Severity,
    pub message: String,
}

const RATE_LIMIT_MESSAGE: &str = "Rate limits exceeded. Please wait a moment before trying again.";
const RATE_LIMIT_UPGRADE_MESSAGE: &str = "Rate limits exceeded. Please wait a moment before trying \
     again, or upgrade to Pro for higher rate limits and uninterrupted access.";
const CONTEXT_TOO_LONG_MESSAGE: &str = "The conversation context exceeds the model's maximum \
     limit. The last messages and output of agent actions went above the allowed \
     size.\n\nTo recover:\n1. Use /rewind to undo recent messages and tool outputs\n2. Then use \
     /compact to summarize the remaining conversation\n\nThis will free up context space so you \
     can continue working.";
const REFUSAL_LEAD: &str = "The model declined to respond and stopped early (refusal).";
const REFUSAL_FALLBACK: &str = "This can happen with certain prompts or content. Try rephrasing \
     your request or starting a new conversation.";

/// Classifies a turn or tool failure and resolves the reference message. The
/// raw payload never reaches the message: only the reference-shaped category
/// and explanation are read out of `details`.
#[must_use]
pub fn classify(
    code: Option<&str>,
    message: &str,
    details: &Value,
    upgrade_available: bool,
) -> ClassifiedError {
    let class = classify_code(code, message);
    let message = match class {
        ErrorClass::RateLimit => if upgrade_available {
            RATE_LIMIT_UPGRADE_MESSAGE
        } else {
            RATE_LIMIT_MESSAGE
        }
        .to_owned(),
        ErrorClass::ContextExhaustion => CONTEXT_TOO_LONG_MESSAGE.to_owned(),
        ErrorClass::Refusal => refusal_message(details),
        _ => message.to_owned(),
    };
    ClassifiedError {
        class,
        severity: class.severity(),
        message,
    }
}

fn classify_code(code: Option<&str>, message: &str) -> ErrorClass {
    match code {
        Some("rate_limit") => return ErrorClass::RateLimit,
        Some("context_too_long") => return ErrorClass::ContextExhaustion,
        Some("response_too_long") => return ErrorClass::ResponseTooLong,
        Some("refusal") => return ErrorClass::Refusal,
        Some("cancelled" | "interrupted") => return ErrorClass::Cancellation,
        Some("unauthorized" | "auth" | "authentication_failed") => return ErrorClass::Auth,
        Some("transport" | "connection_lost") => return ErrorClass::Transport,
        Some("model_error" | "model") => return ErrorClass::Model,
        Some("server_error" | "server") => return ErrorClass::Server,
        Some("shell_failed" | "shell") => return ErrorClass::Shell,
        Some("tool_failed" | "tool") => return ErrorClass::Tool,
        _ => {}
    }
    let lowered = message.to_lowercase();
    if lowered.contains("cancel") || lowered.contains("interrupt") {
        ErrorClass::Cancellation
    } else if lowered.contains("unauthorized") || lowered.contains("api key") {
        ErrorClass::Auth
    } else if lowered.contains("connection") || lowered.contains("transport") {
        ErrorClass::Transport
    } else {
        ErrorClass::Unknown
    }
}

/// Reference `_refusal_message`: the lead, the optional category, then the
/// model explanation or the reference fallback.
fn refusal_message(details: &Value) -> String {
    let mut lead = REFUSAL_LEAD.to_owned();
    if let Some(category) = details.get("category").and_then(Value::as_str) {
        lead.push_str(&format!("\nCategory: {category}."));
    }
    let detail = details
        .get("explanation")
        .and_then(Value::as_str)
        .unwrap_or(REFUSAL_FALLBACK);
    format!("{lead}\n\n{detail}")
}

/// Maps a typed driver failure onto the reference error code, so the shared
/// classifier decides severity and guidance in one place.
#[must_use]
pub fn driver_error_code(error: &vibe_app_server::client::DriverError) -> Option<&'static str> {
    use vibe_app_server::client::DriverError;
    use vibe_core::provider::{ProviderError, TransportError};

    match error {
        DriverError::MissingCredentialEnvironment(_) => Some("auth"),
        DriverError::Transport(_) => Some("transport"),
        DriverError::Provider(ProviderError::Refusal(_)) => Some("refusal"),
        DriverError::Provider(ProviderError::ContextOverflow) => Some("context_too_long"),
        DriverError::Provider(ProviderError::Authentication { .. }) => Some("auth"),
        DriverError::Provider(
            ProviderError::RetryExhausted { status: 429 }
            | ProviderError::HttpStatus { status: 429 },
        ) => Some("rate_limit"),
        DriverError::Provider(ProviderError::Transport(TransportError::ResponseTooLarge {
            ..
        })) => Some("response_too_long"),
        DriverError::Provider(ProviderError::Transport(_)) => Some("transport"),
        DriverError::Provider(_) => Some("model"),
        DriverError::Tool(_) => Some("tool"),
        DriverError::Storage(_) | DriverError::Engine(_) | DriverError::Observation(_) => {
            Some("server")
        }
        DriverError::StaleTurn(_) => Some("cancelled"),
        DriverError::StatePoisoned
        | DriverError::UnsupportedControl(_)
        | DriverError::ImageAttachment(_)
        | DriverError::Compaction(_)
        | DriverError::InvalidSystemTime => None,
    }
}

/// Bounded failure log that suppresses a repeat of the same class and message
/// so a retried turn cannot bury the transcript, while keeping every distinct
/// failure visible.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ErrorLog {
    seen: VecDeque<(ErrorClass, String)>,
}

impl ErrorLog {
    /// Returns `true` when the failure is new and must be surfaced.
    pub fn record(&mut self, error: &ClassifiedError) -> bool {
        let key = (error.class, error.message.clone());
        if self.seen.contains(&key) {
            return false;
        }
        if self.seen.len() == MAX_TRACKED_ERRORS {
            self.seen.pop_front();
        }
        self.seen.push_back(key);
        true
    }

    pub fn clear(&mut self) {
        self.seen.clear();
    }
}

/// Live turn activity: what the runtime is doing, for how long, and what the
/// operator can press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub status: String,
    pub elapsed_seconds: u64,
    pub queued: usize,
}

impl Activity {
    #[must_use]
    pub fn new(status: impl Into<String>, elapsed_seconds: u64, queued: usize) -> Self {
        let status = status.into();
        Self {
            status: if status.is_empty() {
                DEFAULT_ACTIVITY_STATUS.to_owned()
            } else {
                status
            },
            elapsed_seconds,
            queued,
        }
    }

    /// Reference `LoadingWidget._format_hint`.
    #[must_use]
    pub fn hint(&self) -> String {
        let elapsed = format_elapsed(self.elapsed_seconds);
        if self.queued > 0 {
            format!("({elapsed} Esc to interrupt · Ctrl+C to cancel last queued message)")
        } else {
            format!("({elapsed} Esc/Ctrl+C to interrupt)")
        }
    }

    #[must_use]
    pub fn line(&self) -> String {
        format!("⠋ {}… {}", self.status, self.hint())
    }
}

/// Reference `_format_elapsed`.
#[must_use]
pub fn format_elapsed(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let (minutes, rest) = (seconds / 60, seconds % 60);
    if minutes < 60 {
        return format!("{minutes}m{rest}s");
    }
    format!("{}h{}m{rest}s", minutes / 60, minutes % 60)
}

/// Reference `LOG_LEVEL_COLORS`.
#[must_use]
pub fn log_level_color(level: &str) -> &'static str {
    match level {
        "INFO" => "cyan",
        "WARNING" => "yellow",
        "ERROR" => "red",
        "CRITICAL" => "bold red",
        _ => "dim",
    }
}

/// Reference `DebugConsole._format_entry`, without the Rich markup the
/// terminal port expresses as styled spans.
#[must_use]
pub fn debug_log_line(timestamp_seconds: u64, level: &str, message: &str) -> String {
    format!("{} {level:<8} {message}", format_utc(timestamp_seconds))
}

fn format_utc(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        day_seconds / 3_600,
        (day_seconds % 3_600) / 60,
        day_seconds % 60
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u64, u64) {
    let days = days_since_epoch.saturating_add(719_468);
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (
        year,
        u64::try_from(month).unwrap_or_default(),
        u64::try_from(day).unwrap_or_default(),
    )
}

/// Reference `links._SAFE_SCHEMES`: only `http` and `https` may ever be
/// handed to the system opener.
#[must_use]
pub fn is_safe_url(url: &str) -> bool {
    url.split_once("://")
        .is_some_and(|(scheme, rest)| matches!(scheme, "http" | "https") && !rest.is_empty())
}

/// Reference `linkify_urls_in_text`: the byte ranges of the safe URLs inside a
/// transcript line, so activation can only ever target a validated target.
#[must_use]
pub fn safe_link_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let bytes = text.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let Some(offset) = text[cursor..].find("://") else {
            break;
        };
        let separator = cursor.saturating_add(offset);
        let start = text[..separator]
            .rfind(|character: char| !character.is_ascii_alphabetic())
            .map_or(0, |index| index.saturating_add(1));
        let end = text[separator..]
            .find(char::is_whitespace)
            .map_or(text.len(), |index| separator.saturating_add(index));
        let candidate = text[start..end].trim_end_matches(['.', ',', ')', ']', '"', '\'']);
        if is_safe_url(candidate) {
            spans.push((start, start.saturating_add(candidate.len())));
        }
        cursor = end.max(separator.saturating_add(3));
    }
    spans
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn benign_classes_carry_reference_guidance_and_never_leak_the_payload() {
        let rate_limit = classify(Some("rate_limit"), "429 too many", &Value::Null, false);
        assert_eq!(rate_limit.class, ErrorClass::RateLimit);
        assert_eq!(rate_limit.severity, Severity::Warning);
        assert_eq!(rate_limit.message, RATE_LIMIT_MESSAGE);
        assert!(!rate_limit.message.contains("429"));

        let upgrade = classify(Some("rate_limit"), "429", &Value::Null, true);
        assert_eq!(upgrade.message, RATE_LIMIT_UPGRADE_MESSAGE);

        let context = classify(Some("context_too_long"), "overflow", &Value::Null, false);
        assert!(context.message.contains("/rewind"));
        assert!(context.message.contains("/compact"));

        let refusal = classify(
            Some("refusal"),
            "refused",
            &json!({"category": "policy", "explanation": "Not supported.", "prompt": "secret"}),
            false,
        );
        assert_eq!(
            refusal.message,
            "The model declined to respond and stopped early (refusal).\nCategory: policy.\n\nNot supported."
        );
        assert!(!refusal.message.contains("secret"));

        let bare_refusal = classify(Some("refusal"), "refused", &Value::Null, false);
        assert!(bare_refusal.message.ends_with(REFUSAL_FALLBACK));
    }

    #[test]
    fn unknown_codes_fall_back_to_the_message_without_inventing_a_class() {
        let unknown = classify(Some("weird_code"), "connection reset", &Value::Null, false);
        assert_eq!(unknown.class, ErrorClass::Transport);
        assert_eq!(unknown.message, "connection reset");
        assert!(!unknown.class.is_benign());

        let opaque = classify(None, "something broke", &Value::Null, false);
        assert_eq!(opaque.class, ErrorClass::Unknown);
        assert_eq!(opaque.severity, Severity::Error);
        assert_eq!(opaque.message, "something broke");
    }

    #[test]
    fn repeated_failures_are_muted_while_distinct_ones_stay_visible() {
        let mut log = ErrorLog::default();
        let first = classify(Some("tool_failed"), "edit failed", &Value::Null, false);
        let second = classify(Some("tool_failed"), "read failed", &Value::Null, false);
        assert!(log.record(&first));
        assert!(!log.record(&first));
        assert!(log.record(&second));
        log.clear();
        assert!(log.record(&first));
    }

    #[test]
    fn activity_and_elapsed_follow_the_reference_hint_contract() {
        assert_eq!(format_elapsed(9), "9s");
        assert_eq!(format_elapsed(75), "1m15s");
        assert_eq!(format_elapsed(3_725), "1h2m5s");
        assert_eq!(
            Activity::new("Reading file", 9, 0).hint(),
            "(9s Esc/Ctrl+C to interrupt)"
        );
        assert_eq!(
            Activity::new("", 75, 2).hint(),
            "(1m15s Esc to interrupt · Ctrl+C to cancel last queued message)"
        );
        assert_eq!(Activity::new("", 0, 0).status, DEFAULT_ACTIVITY_STATUS);
        assert_eq!(
            Activity::new("Editing files", 3, 0).line(),
            "⠋ Editing files… (3s Esc/Ctrl+C to interrupt)"
        );
    }

    #[test]
    fn debug_entries_pad_their_level_and_render_a_stable_utc_stamp() {
        assert_eq!(
            debug_log_line(1_754_179_200, "ERROR", "transport closed"),
            "2025-08-03 00:00:00 ERROR    transport closed"
        );
        assert_eq!(log_level_color("ERROR"), "red");
        assert_eq!(log_level_color("TRACE"), "dim");
    }

    #[test]
    fn only_http_targets_become_activatable_links() {
        assert!(is_safe_url("https://ratatui.rs"));
        assert!(!is_safe_url("file:///etc/passwd"));
        assert!(!is_safe_url("javascript:alert(1)"));
        assert_eq!(
            safe_link_spans("See https://ratatui.rs for details"),
            vec![(4, 22)]
        );
        assert!(safe_link_spans("Run file:///etc/passwd now").is_empty());
        assert!(safe_link_spans("Try javascript:alert(1) instead").is_empty());
        assert_eq!(
            safe_link_spans("a https://one.example, b http://two.example.").len(),
            2
        );
    }
}
