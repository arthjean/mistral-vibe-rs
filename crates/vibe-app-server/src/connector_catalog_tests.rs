use serde_json::json;

use super::*;

#[test]
fn aliases_are_normalized_as_the_reference_normalizes_them() {
    assert_eq!(normalize_connector_alias("Google Drive"), "Google_Drive");
    assert_eq!(normalize_connector_alias("--x--"), "x");
    assert_eq!(normalize_connector_alias("!!!"), "unnamed");
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

#[test]
fn a_bootstrap_resolves_to_id_ordered_connectors_with_unique_aliases() {
    let payload = json!({"connectors": [
        {"id": "b", "name": "Drive", "status": {"is_ready": true},
         "tools": [{"name": "search", "inputSchema": {}}, {"name": "list"}]},
        {"id": "a", "name": "Drive", "auth_action": {"type": "oauth"},
         "bootstrap_errors": ["token_expired: detail", "free text"]},
        {"id": "dup"}, {"id": "dup"},
        {"id": 3},
    ]});
    let catalog = resolve_catalog(&payload, "fp").unwrap();
    let aliases = catalog
        .connectors
        .iter()
        .map(|c| c.alias.as_str())
        .collect::<Vec<_>>();
    assert_eq!(aliases, ["Drive", "Drive_2"]);
    assert_eq!(catalog.connectors[0].auth_action, "oauth");
    assert_eq!(
        catalog.connectors[0].diagnostics,
        [
            "Connector bootstrap issue: token_expired",
            "Connector failed to bootstrap."
        ]
    );
    let tools = catalog.connectors[1]
        .tools
        .iter()
        .map(|t| t.raw_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(tools, ["list", "search"]);
    assert_eq!(catalog.revision.len(), 64);
}

#[test]
fn a_broken_envelope_is_the_one_fatal_payload() {
    assert!(resolve_catalog(&json!([]), "fp").is_err());
    assert!(resolve_catalog(&json!({"connectors": 1}), "fp").is_err());
    assert!(
        resolve_catalog(&json!({}), "fp")
            .unwrap()
            .connectors
            .is_empty()
    );
}

#[test]
fn a_cached_catalog_round_trips_while_it_is_fresh() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(CACHE_FILE);
    let catalog = resolve_catalog(
        &json!({"connectors": [{"id": "x", "name": "X", "bootstrap_errors": "boom"}]}),
        "fp",
    )
    .unwrap();
    write_cache(&path, &catalog, 1_000).unwrap();
    let (read, stored_at) = read_cache(&path, "fp", 1_100).unwrap();
    assert_eq!((read, stored_at), (catalog, 1_000));
    assert!(read_cache(&path, "fp", 1_000 + CACHE_TTL_SECONDS).is_none());
    assert!(read_cache(&path, "other", 1_100).is_none());
}
