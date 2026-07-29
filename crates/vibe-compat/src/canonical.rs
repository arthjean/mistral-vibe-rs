use std::path::Path;

use regex::Regex;
use serde_json::Value;
use thiserror::Error;

use crate::model::{CanonicalRule, OracleOutcome, VolatilityEvidence};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CanonicalizationError {
    #[error("outcome serialization failed: {0}")]
    Serialize(String),
    #[error("declared canonicalization pointer does not resolve: {0}")]
    MissingPointer(String),
}

pub fn canonicalize(
    outcome: &OracleOutcome,
    rules: &[CanonicalRule],
) -> Result<OracleOutcome, CanonicalizationError> {
    let mut value = serde_json::to_value(outcome)
        .map_err(|error| CanonicalizationError::Serialize(error.to_string()))?;
    for rule in rules {
        let selected = value
            .pointer_mut(&rule.pointer)
            .ok_or_else(|| CanonicalizationError::MissingPointer(rule.pointer.clone()))?;
        *selected = Value::String(rule.placeholder.clone());
    }
    serde_json::from_value(value)
        .map_err(|error| CanonicalizationError::Serialize(error.to_string()))
}

pub fn volatility_evidence(
    first: &OracleOutcome,
    second: &OracleOutcome,
    rules: &[CanonicalRule],
) -> Result<Vec<VolatilityEvidence>, CanonicalizationError> {
    let first = serde_json::to_value(first)
        .map_err(|error| CanonicalizationError::Serialize(error.to_string()))?;
    let second = serde_json::to_value(second)
        .map_err(|error| CanonicalizationError::Serialize(error.to_string()))?;
    rules
        .iter()
        .map(|rule| {
            let left = first
                .pointer(&rule.pointer)
                .ok_or_else(|| CanonicalizationError::MissingPointer(rule.pointer.clone()))?;
            let right = second
                .pointer(&rule.pointer)
                .ok_or_else(|| CanonicalizationError::MissingPointer(rule.pointer.clone()))?;
            Ok(VolatilityEvidence {
                pointer: rule.pointer.clone(),
                placeholder: rule.placeholder.clone(),
                changed_between_runs: left != right,
            })
        })
        .collect()
}

pub fn redact(
    outcome: &mut OracleOutcome,
    substitutions: &[(&Path, &str)],
) -> Result<(), CanonicalizationError> {
    let patterns = secret_patterns()?;
    let mut value = serde_json::to_value(&*outcome)
        .map_err(|error| CanonicalizationError::Serialize(error.to_string()))?;
    redact_value(&mut value, None, substitutions, &patterns);
    *outcome = serde_json::from_value(value)
        .map_err(|error| CanonicalizationError::Serialize(error.to_string()))?;
    Ok(())
}

fn redact_value(
    value: &mut Value,
    key: Option<&str>,
    substitutions: &[(&Path, &str)],
    patterns: &[(Regex, &'static str)],
) {
    match value {
        Value::Object(map) => {
            for (child_key, child) in map {
                redact_value(child, Some(child_key), substitutions, patterns);
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item, key, substitutions, patterns);
            }
        }
        Value::String(text) => {
            for (path, replacement) in substitutions {
                if let Some(raw) = path.to_str() {
                    *text = text.replace(raw, replacement);
                }
            }
            let normalized_key = key.unwrap_or_default().to_ascii_lowercase();
            if normalized_key.contains("token")
                || normalized_key.contains("password")
                || normalized_key.contains("secret")
                || normalized_key.contains("credential")
                || normalized_key == "authorization"
                || normalized_key == "api_key"
                || normalized_key == "apikey"
            {
                *text = "<REDACTED>".to_owned();
            } else if normalized_key.contains("proxy") {
                *text = "<REDACTED_PROXY_URL>".to_owned();
            } else if text.starts_with("Bearer ") || text.starts_with("sk-") {
                *text = "<REDACTED>".to_owned();
            } else if text.contains("://") && text.contains('@') {
                *text = "<REDACTED_PROXY_URL>".to_owned();
            } else if text.contains("PRIVATE KEY-----") {
                *text = "<REDACTED_PRIVATE_KEY>".to_owned();
            } else {
                for (pattern, replacement) in patterns {
                    *text = pattern.replace_all(text, *replacement).into_owned();
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn secret_patterns() -> Result<Vec<(Regex, &'static str)>, CanonicalizationError> {
    [
        (r"AKIA[0-9A-Z]{16}", "<REDACTED_AWS_KEY>"),
        (
            r"(?i)(?:gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9_-]{16,})",
            "<REDACTED_TOKEN>",
        ),
        (
            r"(?i)Bearer\s+[A-Za-z0-9._~+/=-]{8,}",
            "Bearer <REDACTED>",
        ),
        (
            r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            "<REDACTED_JWT>",
        ),
        (r"https?://[^\s/@:]+:[^@\s/]+@[^\s]+", "<REDACTED_PROXY_URL>"),
        (r#"/(?:home|Users)/[^/\s"']+"#, "<HOME>"),
        (r#"[A-Za-z]:\\Users\\[^\\\s"']+"#, "<HOME>"),
        (
            r#"(?i)(?:api[_-]?key|token|secret|password|authorization)\s*[:=]\s*["']?[A-Za-z0-9._~+/=-]{8,}"#,
            "<REDACTED_SECRET_ASSIGNMENT>",
        ),
    ]
    .into_iter()
    .map(|(pattern, replacement)| {
        Regex::new(pattern)
            .map(|regex| (regex, replacement))
            .map_err(|error| CanonicalizationError::Serialize(error.to_string()))
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome() -> OracleOutcome {
        let mut value = OracleOutcome::empty(vec!["driver".to_owned()]);
        value.json_frames = vec![serde_json::json!({
            "timestamp": 12,
            "uuid": "abc",
            "events": [{"code": "conflict"}, {"code": "forbidden"}]
        })];
        value
    }

    #[test]
    fn only_declared_fields_are_canonicalized() {
        let rules = vec![
            CanonicalRule {
                pointer: "/jsonFrames/0/timestamp".to_owned(),
                placeholder: "<TIMESTAMP>".to_owned(),
            },
            CanonicalRule {
                pointer: "/jsonFrames/0/uuid".to_owned(),
                placeholder: "<UUID>".to_owned(),
            },
        ];
        let canonical = canonicalize(&outcome(), &rules).expect("valid rules");
        assert_eq!(canonical.json_frames[0]["timestamp"], "<TIMESTAMP>");
        assert_eq!(canonical.json_frames[0]["uuid"], "<UUID>");
        assert_eq!(canonical.json_frames[0]["events"][0]["code"], "conflict");
    }

    #[test]
    fn event_order_and_error_codes_remain_semantic() {
        let first = canonicalize(&outcome(), &[]).expect("no rules");
        let mut second = outcome();
        second.json_frames[0]["events"]
            .as_array_mut()
            .expect("events array")
            .reverse();
        assert_ne!(first, second);
    }

    #[test]
    fn secrets_and_host_paths_are_redacted_before_persistence() {
        let mut value = OracleOutcome::empty(vec!["/home/test/project".to_owned()]);
        value.json_frames = vec![serde_json::json!({
            "apiKey": "sk-sensitive",
            "authorization": "Bearer sensitive",
            "proxy": "https://user:password@proxy.example",
            "path": "/home/test/project/file",
            "neutral": "AKIA1234567890ABCDEF github_pat_1234567890abcdefghijklmnop"
        })];
        redact(&mut value, &[(Path::new("/home/test"), "<HOME>")]).expect("redaction succeeds");
        let encoded = serde_json::to_string(&value).expect("redacted outcome serializes");
        assert!(!encoded.contains("sensitive"));
        assert!(!encoded.contains("/home/test"));
        assert!(!encoded.contains("user:password"));
        assert!(!encoded.contains("AKIA1234567890ABCDEF"));
        assert!(!encoded.contains("github_pat_"));
        assert!(encoded.contains("<HOME>"));
    }
}
