//! How an audio request names its sender.
//!
//! Both reference audio clients identify themselves as the Mistral client does
//! (`get_user_agent("mistral")`, `vibe/utils/http.py:215-220`) and attach the
//! mapping their metadata getter returns: an empty mapping for a caller that
//! supplies none, which is what the editor adapter does, and
//! `build_audio_request_metadata` for the terminal
//! (`vibe/cli/audio_request_metadata.py:11-25`). The realtime request carries
//! it JSON-encoded in `x-metadata`, the speech request in its body.

use serde_json::{Map, Value};

/// The version every audio request reports, the workspace's own.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Reference `get_user_agent(Backend.MISTRAL)`.
#[must_use]
pub fn user_agent() -> String {
    format!("mistral-client-python/Mistral-Vibe/{}", VERSION)
}

/// What one audio request attaches as its metadata, in the order the
/// reference builds it.
pub type RequestMetadata = Vec<(String, String)>;

/// Where an audio client reads its metadata from at the moment it sends.
pub type MetadataGetter = std::sync::Arc<dyn Fn() -> RequestMetadata + Send + Sync>;

/// A getter for the empty mapping, which is the reference's `dict` default.
#[must_use]
pub fn no_metadata() -> MetadataGetter {
    std::sync::Arc::new(Vec::new)
}

/// Reference `build_audio_request_metadata`.
#[must_use]
pub fn audio_request_metadata(
    session_id: &str,
    parent_session_id: Option<&str>,
) -> RequestMetadata {
    let mut metadata = vec![
        ("os".to_owned(), vibe_core::telemetry::platform_id()),
        ("version".to_owned(), VERSION.to_owned()),
        ("session_id".to_owned(), session_id.to_owned()),
        ("call_type".to_owned(), "secondary_call".to_owned()),
        ("call_source".to_owned(), "vibe_code".to_owned()),
    ];
    if let Some(os_version) = vibe_core::telemetry::platform_version() {
        metadata.push(("os_version".to_owned(), os_version));
    }
    if let Some(parent) = parent_session_id {
        metadata.push(("parent_session_id".to_owned(), parent.to_owned()));
    }
    metadata
}

/// The metadata as a JSON object, for a request body.
#[must_use]
pub fn metadata_object(metadata: &RequestMetadata) -> Value {
    Value::Object(
        metadata
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect::<Map<_, _>>(),
    )
}

/// The metadata as Python's `json.dumps` writes it, which is the header value
/// the reference sends: `", "` and `": "` separators, insertion order, and
/// every character outside ASCII escaped.
#[must_use]
pub fn metadata_header(metadata: &RequestMetadata) -> String {
    let members = metadata
        .iter()
        .map(|(key, value)| format!("{}: {}", python_string(key), python_string(value)))
        .collect::<Vec<_>>();
    format!("{{{}}}", members.join(", "))
}

fn python_string(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len() + 2);
    encoded.push('"');
    for character in text.chars() {
        match character {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\t' => encoded.push_str("\\t"),
            '\u{08}' => encoded.push_str("\\b"),
            '\u{0c}' => encoded.push_str("\\f"),
            character if character.is_ascii() && !character.is_ascii_control() => {
                encoded.push(character);
            }
            character => {
                let mut units = [0_u16; 2];
                for unit in character.encode_utf16(&mut units) {
                    encoded.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    encoded.push('"');
    encoded
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn the_header_is_written_as_python_dumps_it() {
        let metadata = vec![
            ("os".to_owned(), "linux".to_owned()),
            ("note".to_owned(), "é\"q".to_owned()),
        ];
        assert_eq!(
            metadata_header(&metadata),
            "{\"os\": \"linux\", \"note\": \"\\u00e9\\\"q\"}"
        );
        assert_eq!(metadata_header(&Vec::new()), "{}");
    }

    #[test]
    fn the_terminal_metadata_names_its_session_and_call() {
        let metadata = audio_request_metadata("session-1", Some("parent-1"));
        let keys = metadata
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            &keys[..5],
            ["os", "version", "session_id", "call_type", "call_source"]
        );
        assert_eq!(keys.last(), Some(&"parent_session_id"));
    }
}
