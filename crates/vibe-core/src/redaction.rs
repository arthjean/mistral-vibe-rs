//! What each boundary removes before it publishes anything, and why the three
//! answers are not one.
//!
//! Three surfaces of this crate hide secrets, and it is tempting to read that
//! as one rule written three times. It is not. Each one guards a different
//! namespace against a different reader, and each reproduces a different
//! reference behavior:
//!
//! | Boundary | Owner | Reads | Replaces |
//! |---|---|---|---|
//! | The configuration a client is shown | this module, applied by [`crate::config`] | TOML keys an operator wrote | the value, with [`REDACTED`] |
//! | A span leaving the process | [`crate::tracing::redaction`] | OpenTelemetry attribute keys | the value, with its own marker, plus a regex sweep of what it keeps |
//! | An integration's error text | [`crate::integrations::redact`] | free-form message text | the whole message |
//!
//! The three key vocabularies are deliberately disjoint. A TOML key is a name
//! an operator chose; an OTel attribute key is a semantic-convention name this
//! crate emits; an integration error is prose from a remote server with no keys
//! at all. Merging the lists would widen each boundary past what its reference
//! does, and `crates/vibe-core/src/config/surface_parity_tests.rs` measures the
//! first one against the reference. So this module owns the configuration rule
//! and names the other two rather than absorbing them; what it prevents is a
//! fourth appearing without anyone noticing the first three.

use serde_json::Value as JsonValue;
use toml::{Table, Value};

/// What a removed configuration value reads as.
///
/// A client renders this verbatim, and the parity harness renders it for a
/// divergence under a sensitive key, so both sides spell it from here.
pub const REDACTED: &str = "[redacted]";

/// Whether a configuration key names something that must not be published.
///
/// The key is normalized to its alphanumerics first, so `api_key`, `apiKey`
/// and `API-KEY` are one name. Matching on a fragment rather than on the whole
/// key is what makes `mistral_api_key_env_var` sensitive without the list
/// having to enumerate every provider that might carry one.
///
/// The fragment rule over-matches, and deliberately: `max_tokens` carries
/// `token` and is withheld even though a token budget is not a credential.
/// Withholding a harmless number costs a client one field it can ask for by
/// name; a rule tightened to whole words would publish any key whose secret
/// half is glued to another word. `a_benign_key_caught_by_a_fragment_is_still_
/// withheld` records the trade so it is a decision rather than a surprise.
#[must_use]
pub fn is_sensitive_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    SENSITIVE_FRAGMENTS
        .iter()
        .any(|fragment| normalized.contains(fragment))
}

/// The words that make a configuration key sensitive.
///
/// `proxy` is here because a proxy URL carries credentials in its userinfo
/// often enough that publishing one is publishing them.
const SENSITIVE_FRAGMENTS: &[&str] = &[
    "password",
    "secret",
    "token",
    "apikey",
    "authorization",
    "credential",
    "privatekey",
    "accesskey",
    "proxy",
];

/// The document with every sensitive value replaced, ready to publish.
#[must_use]
pub fn redact_table(table: &Table) -> JsonValue {
    let mut object = serde_json::Map::new();
    for (key, value) in table {
        let redacted = if is_sensitive_key(key) {
            JsonValue::String(REDACTED.to_owned())
        } else {
            redact_value(value)
        };
        object.insert(key.clone(), redacted);
    }
    JsonValue::Object(object)
}

/// One value, with any table nested inside it redacted too.
#[must_use]
pub fn redact_value(value: &Value) -> JsonValue {
    match value {
        Value::String(value) => JsonValue::String(value.clone()),
        Value::Integer(value) => JsonValue::from(*value),
        Value::Float(value) => JsonValue::from(*value),
        Value::Boolean(value) => JsonValue::from(*value),
        Value::Datetime(value) => JsonValue::String(value.to_string()),
        Value::Array(values) => JsonValue::Array(values.iter().map(redact_value).collect()),
        Value::Table(values) => redact_table(values),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule reads a key by its letters and digits alone, so the same name
    /// in any spelling is judged the same way.
    #[test]
    fn a_sensitive_name_is_recognized_in_every_spelling() {
        for spelling in ["api_key", "apiKey", "API-KEY", "mistral_api_key_env_var"] {
            assert!(is_sensitive_key(spelling), "{spelling}");
        }
        for benign in ["theme", "model", "keyboard", "temperature"] {
            assert!(!is_sensitive_key(benign), "{benign}");
        }
    }

    /// The fragment rule errs toward withholding, and this is what that costs.
    ///
    /// `max_tokens` is a budget, not a credential, and it is still withheld
    /// because the rule matches `token` anywhere in the name. Recorded rather
    /// than fixed: tightening the rule to whole words would publish a key whose
    /// secret half is glued to another word, which is the failure that matters.
    #[test]
    fn a_benign_key_caught_by_a_fragment_is_still_withheld() {
        assert!(is_sensitive_key("max_tokens"));
        assert!(is_sensitive_key("proxy_timeout"));
    }

    /// The three boundaries stay separate on purpose.
    ///
    /// This does not assert that one is right and another wrong: it asserts
    /// that they are still three, and that each still answers for its own
    /// namespace. A change that quietly folded one into another would make a
    /// boundary publish or withhold more than the reference it reproduces.
    #[test]
    fn the_three_redaction_boundaries_remain_distinct() {
        // Configuration keys: this module's rule, and it does not claim to
        // judge the span vocabulary.
        assert!(is_sensitive_key("private_key"));
        assert!(!is_sensitive_key("gen_ai.input.messages"));

        // Span attributes: the tracing policy's, keyed on names this crate
        // emits rather than on names an operator wrote.
        let attributes = crate::tracing::redaction::RedactionPolicy::Key.redact_attributes(vec![
            opentelemetry::KeyValue::new("gen_ai.input.messages", "a prompt"),
            opentelemetry::KeyValue::new("gen_ai.request.model", "a model"),
        ]);
        let rendered = attributes
            .iter()
            .map(|attribute| {
                (
                    attribute.key.as_str().to_owned(),
                    attribute.value.to_string(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            rendered.get("gen_ai.input.messages").map(String::as_str),
            Some(crate::tracing::redaction::REDACTED_VALUE)
        );
        assert_eq!(
            rendered.get("gen_ai.request.model").map(String::as_str),
            Some("a model"),
        );

        // Integration errors: free text, replaced whole rather than per key.
        assert_eq!(
            crate::integrations::redact("failed: authorization: Bearer abc"),
            "[redacted sensitive error]"
        );
        assert_eq!(
            crate::integrations::redact("the connector is unreachable"),
            "the connector is unreachable"
        );

        // And the three markers are distinct, which is what makes a redacted
        // value traceable to the boundary that removed it.
        assert_ne!(REDACTED, crate::tracing::redaction::REDACTED_VALUE);
    }
}
