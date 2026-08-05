//! The registry's own invariants, and the proof that generating the published
//! schema from it changed no key surface.

use std::collections::{BTreeMap, BTreeSet};

use super::registry::{
    FIELDS, FieldDefault, FieldKind, MergeStrategy, default_document, field, json_schema,
    schema_version,
};
use super::*;

/// The fields that publish no default. `compaction_model` has none upstream
/// either, `tools` is filled by tool discovery rather than by a declaration,
/// and the three legacy proxy keys are unset until an operator writes one.
const WITHOUT_DEFAULT: [&str; 5] = [
    "compaction_model",
    "tools",
    "proxy",
    "tls_ca_path",
    "dotenv_path",
];

#[test]
fn every_entry_declares_one_name_and_a_usable_strategy() {
    let mut seen = BTreeSet::new();
    for spec in FIELDS {
        assert!(
            seen.insert(spec.name),
            "duplicate registry entry for field `{}`",
            spec.name
        );
        assert!(
            !spec.name.is_empty(),
            "a registry entry declares an empty name"
        );
        match spec.strategy {
            // A union entry without its merge key cannot be merged at all, so
            // the declaration is rejected here rather than at load time.
            MergeStrategy::Union => {
                let merge_key = spec.merge_key.unwrap_or_default();
                assert!(
                    !merge_key.is_empty(),
                    "union field `{}` declares no merge key",
                    spec.name
                );
            }
            _ => assert_eq!(
                spec.merge_key, None,
                "field `{}` declares a merge key without the union strategy",
                spec.name
            ),
        }
        if spec.kind == FieldKind::Enum {
            assert!(
                !spec.choices.is_empty(),
                "enum field `{}` declares no choices",
                spec.name
            );
        }
    }
    assert_eq!(
        seen.len(),
        FIELDS.len(),
        "the name index is shorter than the declaration table"
    );
}

#[test]
fn declared_defaults_and_schema_literals_are_well_formed() {
    for spec in FIELDS {
        if !spec.schema_extra.is_empty() {
            let parsed = serde_json::from_str::<JsonValue>(spec.schema_extra)
                .unwrap_or_else(|error| panic!("field `{}` schema extra: {error}", spec.name));
            assert!(
                parsed.is_object(),
                "field `{}` schema extra must be a JSON object",
                spec.name
            );
        }
        if spec.published {
            assert!(
                !spec.description.is_empty(),
                "published field `{}` carries no description",
                spec.name
            );
        }
        match (spec.kind, spec.default) {
            (FieldKind::Bool, FieldDefault::Bool(_))
            | (FieldKind::Int, FieldDefault::Int(_))
            | (FieldKind::Float, FieldDefault::Float(_))
            | (FieldKind::List, FieldDefault::Strings(_))
            | (FieldKind::Str | FieldKind::Enum, FieldDefault::Str(_))
            | (FieldKind::Complex, FieldDefault::Json(_))
            | (_, FieldDefault::None) => {}
            (kind, default) => panic!(
                "field `{}` declares a {default:?} default for a {} field",
                spec.name,
                kind.as_str()
            ),
        }
        if let FieldDefault::Json(literal) = spec.default {
            let parsed = serde_json::from_str::<JsonValue>(literal)
                .unwrap_or_else(|error| panic!("field `{}` default literal: {error}", spec.name));
            assert!(
                spec.default.to_toml().is_some(),
                "field `{}` declares a default TOML cannot carry: {parsed}",
                spec.name
            );
        }
        assert_eq!(
            spec.default == FieldDefault::None,
            WITHOUT_DEFAULT.contains(&spec.name),
            "field `{}` gained or lost its declared default",
            spec.name
        );
    }
}

#[test]
fn every_reference_field_reaches_the_default_document() {
    let document = default_document();
    for spec in FIELDS {
        let declared = document.contains_key(spec.name);
        let expected = !spec.local && spec.default != FieldDefault::None;
        assert_eq!(
            declared,
            expected,
            "field `{}` is {} the shipped default document",
            spec.name,
            if declared {
                "wrongly in"
            } else {
                "missing from"
            }
        );
    }
    // A locally declared key never reaches the document: it would make the
    // shipped defaults incomparable to the reference ones.
    for name in ["thinking", "notifications", "proxy"] {
        assert!(
            !document.contains_key(name),
            "`{name}` leaked into defaults"
        );
    }
}

#[test]
fn lookup_falls_back_to_replace_for_an_unregistered_key() {
    assert!(field("future_key").is_none());
    assert_eq!(registry::strategy("future_key"), MergeStrategy::Replace);
    assert_eq!(registry::strategy("disabled_tools"), MergeStrategy::Concat);
    assert_eq!(
        field("mcp_servers").and_then(|spec| spec.merge_key),
        Some("name")
    );
}

/// The defaults US-063 moved onto the reference values, and the two fields that
/// gained one. Each is a behavior change recorded in `CHANGELOG.md`, not a
/// regression, and listing them here is what keeps a fourth one from slipping
/// through unnoticed.
fn realigned_defaults() -> BTreeMap<&'static str, (Option<JsonValue>, JsonValue)> {
    BTreeMap::from([
        (
            "active_model",
            (Some(json!("")), json!("mistral-medium-3.5")),
        ),
        ("theme", (Some(json!("system")), json!("auto"))),
        ("show_thinking_nodes", (Some(json!(true)), json!(false))),
        ("mcp_servers", (None, json!([]))),
        ("connectors", (None, json!([]))),
    ])
}

#[test]
fn every_field_published_before_the_registry_keeps_its_shape() {
    let generated = LayeredConfig::schema();
    let previous = LayeredConfig::schema_before_the_registry();
    let realigned = realigned_defaults();
    let properties = previous["properties"]
        .as_object()
        .expect("the previous schema declares properties");
    for (name, before) in properties {
        let after = generated["properties"]
            .get(name)
            .unwrap_or_else(|| panic!("field `{name}` is no longer published"));
        for key in ["type", "enum", "items", "format"] {
            assert_eq!(
                before.get(key),
                after.get(key),
                "field `{name}` changed the `{key}` a persisted value validates against"
            );
        }
        match realigned.get(name.as_str()) {
            Some((was, now)) => {
                assert_eq!(before.get("default"), was.as_ref(), "`{name}`");
                assert_eq!(after.get("default"), Some(now), "`{name}`");
            }
            None => assert_eq!(
                before.get("default"),
                after.get("default"),
                "field `{name}` changed its default without being recorded"
            ),
        }
    }
}

#[test]
fn the_schema_publishes_every_declared_field() {
    let generated = LayeredConfig::schema();
    let properties = generated["properties"]
        .as_object()
        .expect("the schema declares properties");
    assert_eq!(
        properties.len(),
        FIELDS.len(),
        "the published surface and the registry disagree on their length"
    );
    for spec in FIELDS {
        assert!(
            spec.published,
            "field `{}` is declared but withheld",
            spec.name
        );
        let property = properties
            .get(spec.name)
            .unwrap_or_else(|| panic!("field `{}` is missing from the schema", spec.name));
        assert_eq!(
            property.get("description").and_then(JsonValue::as_str),
            Some(spec.description),
            "field `{}` publishes another description",
            spec.name
        );
        assert!(
            property.get("type").is_some() || property.get("enum").is_some(),
            "field `{}` publishes no type",
            spec.name
        );
        if spec.kind == FieldKind::Enum {
            assert_eq!(
                property.get("enum").and_then(JsonValue::as_array),
                Some(&spec.choices.iter().map(|choice| json!(choice)).collect()),
                "field `{}` publishes another choice set",
                spec.name
            );
        }
        assert_eq!(
            property.get("default").cloned(),
            spec.default.to_json(),
            "field `{}` publishes another default",
            spec.name
        );
    }
}

#[test]
fn the_schema_is_emitted_identically_on_every_call() {
    let first = serde_json::to_string(&json_schema()).expect("schema serializes");
    let second = serde_json::to_string(&json_schema()).expect("schema serializes");
    assert_eq!(
        first, second,
        "a client cannot cache a non-deterministic schema"
    );
    assert_eq!(
        schema_version(),
        LayeredConfig::schema_version(),
        "the token a client caches by is not stable"
    );
    let (algorithm, digest) = schema_version()
        .split_once(':')
        .expect("the version names its algorithm");
    assert_eq!(algorithm, "sha256");
    assert_eq!(digest.len(), 64, "{digest}");
}
