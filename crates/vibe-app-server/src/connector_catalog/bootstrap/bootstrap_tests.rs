use serde_json::json;

use super::*;

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
