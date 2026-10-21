use vibe_core::llm::mistral::server_url;

use super::*;

#[test]
fn aliases_are_normalized_as_the_reference_normalizes_them() {
    assert_eq!(normalize_connector_alias("Google Drive"), "Google_Drive");
    assert_eq!(normalize_connector_alias("--x--"), "x");
    assert_eq!(normalize_connector_alias("!!!"), "unnamed");
}

#[test]
fn a_source_status_names_itself_as_it_serializes() {
    for status in McpSourceStatus::ALL {
        assert_eq!(serde_json::json!(status), status.as_str());
    }
}

#[test]
fn the_server_url_is_the_api_base_before_its_version_segment() {
    assert_eq!(
        server_url("https://api.mistral.ai/v1").as_deref(),
        Some("https://api.mistral.ai")
    );
    assert_eq!(
        server_url("http://127.0.0.1:9/v1/chat").as_deref(),
        Some("http://127.0.0.1:9")
    );
    assert_eq!(server_url("https://api.mistral.ai"), None);
}
