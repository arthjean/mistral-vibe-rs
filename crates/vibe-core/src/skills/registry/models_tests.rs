//! Differential tests for the registry payload models, mirroring the
//! behaviors `tests/skills/registry/test_models.py` asserts upstream: the
//! dual spellings, the sanitizer rules, the name and description resolution
//! chains and the asset decode outcomes.

use serde_json::json;

use super::models::{
    ListVersionsResponse, RegistryAssetContent, RegistrySkillItem, sanitize_skill_name,
};

#[test]
fn camel_and_snake_payloads_populate_the_same_fields() {
    let camel: RegistrySkillItem = serde_json::from_value(json!({
        "skillId": "id-1",
        "version": 4,
        "skill": {"skillName": "n", "skillDescription": "d", "skillBody": "b"},
        "metadata": {"latestVersion": 9, "sharingScope": "org"},
        "versionAttributes": {"aliases": ["main"]},
        "surplusField": {"ignored": true},
    }))
    .expect("the camel payload parses");
    let snake: RegistrySkillItem = serde_json::from_value(json!({
        "skill_id": "id-1",
        "version": 4,
        "skill": {"skill_name": "n", "skill_description": "d", "skill_body": "b"},
        "metadata": {"latest_version": 9, "sharing_scope": "org"},
        "version_attributes": {"aliases": ["main"]},
    }))
    .expect("the snake payload parses");
    for item in [&camel, &snake] {
        assert_eq!(item.skill_id, "id-1");
        assert_eq!(item.version, 4);
        assert_eq!(item.skill.skill_name, "n");
        assert_eq!(item.skill.skill_description, "d");
        assert_eq!(item.skill.skill_body, "b");
        assert_eq!(item.metadata.latest_version, 9);
        assert_eq!(item.metadata.sharing_scope, "org");
        assert_eq!(item.version_attributes.aliases, ["main"]);
    }
}

#[test]
fn the_sanitizer_collapses_trims_lowers_and_caps() {
    assert_eq!(
        sanitize_skill_name("  My Fancy Skill!  "),
        Some("my-fancy-skill".to_owned())
    );
    assert_eq!(
        sanitize_skill_name("a__b..c//d"),
        Some("a-b-c-d".to_owned())
    );
    assert_eq!(
        sanitize_skill_name("--already-good--"),
        Some("already-good".to_owned())
    );
    assert_eq!(sanitize_skill_name("UPPER123"), Some("upper123".to_owned()));
    assert_eq!(sanitize_skill_name(""), None);
    assert_eq!(sanitize_skill_name("!!!"), None);
    // The cap truncates to 64 characters and re-trims a hyphen the cut
    // exposed.
    let long = format!("{}-{}", "a".repeat(63), "b".repeat(10));
    assert_eq!(sanitize_skill_name(&long), Some("a".repeat(63)));
    assert_eq!(sanitize_skill_name(&"x".repeat(80)), Some("x".repeat(64)));
}

#[test]
fn the_name_resolves_through_the_reference_chain() {
    let full: RegistrySkillItem = serde_json::from_value(json!({
        "skillId": "AB-12",
        "skill": {"skillName": "Payload Name"},
        "attributes": {"title": "Title Here"},
        "metadata": {"name": "Metadata Name"},
    }))
    .expect("the item parses");
    assert_eq!(full.resolved_name(), Some("metadata-name".to_owned()));

    let payload_only: RegistrySkillItem = serde_json::from_value(json!({
        "skillId": "AB-12",
        "skill": {"skillName": "Payload Name"},
        "attributes": {"title": "Title Here"},
    }))
    .expect("the item parses");
    assert_eq!(
        payload_only.resolved_name(),
        Some("payload-name".to_owned())
    );

    let title_only: RegistrySkillItem = serde_json::from_value(json!({
        "skillId": "AB-12",
        "attributes": {"title": "Title Here"},
    }))
    .expect("the item parses");
    assert_eq!(title_only.resolved_name(), Some("title-here".to_owned()));

    // The id fallback strips the id's hyphens, prefixes `skill-` and runs
    // through the same sanitizer.
    let id_only: RegistrySkillItem =
        serde_json::from_value(json!({"skillId": "AB-12"})).expect("the item parses");
    assert_eq!(id_only.resolved_name(), Some("skill-ab12".to_owned()));

    // No identifier of any kind resolves to nothing rather than to a
    // generated placeholder.
    let nameless: RegistrySkillItem = serde_json::from_value(json!({})).expect("the item parses");
    assert_eq!(nameless.resolved_name(), None);
}

#[test]
fn the_description_prefers_the_payload_then_the_attributes() {
    let payload: RegistrySkillItem = serde_json::from_value(json!({
        "skill": {"skillDescription": "  from the payload  "},
        "attributes": {"description": "from the attributes"},
    }))
    .expect("the item parses");
    assert_eq!(payload.resolved_description(), "from the payload");

    let attributes: RegistrySkillItem = serde_json::from_value(json!({
        "skill": {"skillDescription": "   "},
        "attributes": {"description": " from the attributes "},
    }))
    .expect("the item parses");
    assert_eq!(attributes.resolved_description(), "from the attributes");

    let neither: RegistrySkillItem = serde_json::from_value(json!({})).expect("the item parses");
    assert_eq!(neither.resolved_description(), "");
}

#[test]
fn asset_content_decodes_text_base64_and_nothing() {
    let text = RegistryAssetContent {
        text_content: Some("héllo".to_owned()),
        ..Default::default()
    };
    assert_eq!(text.to_bytes(), Some("héllo".as_bytes().to_vec()));

    let raw = RegistryAssetContent {
        raw_content: Some("IyEvYmluL3NoCg==".to_owned()),
        ..Default::default()
    };
    assert_eq!(raw.to_bytes(), Some(b"#!/bin/sh\n".to_vec()));

    let invalid = RegistryAssetContent {
        raw_content: Some("!!not-base64!!".to_owned()),
        ..Default::default()
    };
    assert_eq!(invalid.to_bytes(), None);

    assert_eq!(RegistryAssetContent::default().to_bytes(), None);
}

#[test]
fn version_rows_carry_their_aliases_through_to_info() {
    let response: ListVersionsResponse = serde_json::from_value(json!({
        "items": [
            {"version": 1, "versionAttributes": {"aliases": ["stable"]}},
            {"version": 3, "version_attributes": {"aliases": []}},
        ],
    }))
    .expect("the versions response parses");
    let infos: Vec<_> = response
        .items
        .iter()
        .map(super::models::RegistryVersionRow::to_info)
        .collect();
    assert_eq!(infos[0].version, 1);
    assert_eq!(infos[0].aliases, ["stable"]);
    assert_eq!(infos[1].version, 3);
    assert!(infos[1].aliases.is_empty());
}
