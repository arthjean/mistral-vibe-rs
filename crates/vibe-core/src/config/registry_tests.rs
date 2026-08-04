//! The registry's own invariants, and the proof that generating the published
//! schema from it changed no key surface.

use std::collections::BTreeSet;

use super::registry::{FIELDS, FieldDefault, FieldKind, MergeStrategy, field, json_schema};
use super::*;

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
            (FieldKind::Bool, FieldDefault::Bool(_) | FieldDefault::None)
            | (FieldKind::List, FieldDefault::Strings(_) | FieldDefault::None)
            | (FieldKind::Str | FieldKind::Enum, FieldDefault::Str(_) | FieldDefault::None)
            | (_, FieldDefault::None) => {}
            (kind, default) => panic!(
                "field `{}` declares a {default:?} default for a {} field",
                spec.name,
                kind.as_str()
            ),
        }
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

#[test]
fn the_generated_schema_reproduces_the_surface_it_replaced() {
    let generated = LayeredConfig::schema();
    assert_eq!(
        generated,
        LayeredConfig::schema_before_the_registry(),
        "generating the schema from the registry changed the published surface"
    );
    let published = FIELDS.iter().filter(|spec| spec.published).count();
    assert_eq!(
        generated["properties"]
            .as_object()
            .map(serde_json::Map::len),
        Some(published)
    );
}

#[test]
fn the_schema_is_emitted_identically_on_every_call() {
    let first = serde_json::to_string(&json_schema()).expect("schema serializes");
    let second = serde_json::to_string(&json_schema()).expect("schema serializes");
    assert_eq!(
        first, second,
        "a client cannot cache a non-deterministic schema"
    );
}
