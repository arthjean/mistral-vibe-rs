//! Layer composition, one strategy per field.
//!
//! Every top-level key of an overlay document is combined with the accumulated
//! document by the strategy [`registry`] declares for it. A key the registry
//! does not know is replaced, which is what the previous implementation did for
//! every key and what an unknown scalar needs; unlike the reference, such a key
//! survives into the effective document instead of being dropped.

use toml::{Table, Value};

use super::ConfigError;
use super::registry::{self, ENTRY_MERGED_UNION_FIELDS, MergeStrategy};

/// Composes `overlay` onto `target`, applying each field's declared strategy.
pub(super) fn merge_layer(target: &mut Table, overlay: &Table) -> Result<(), ConfigError> {
    for (key, value) in overlay {
        match registry::strategy(key) {
            MergeStrategy::Replace => {
                target.insert(key.clone(), value.clone());
            }
            MergeStrategy::Concat => merge_concat(target, key, value)?,
            MergeStrategy::Union => merge_union(target, key, value)?,
            MergeStrategy::DeepMerge => merge_deep(target, key, value)?,
        }
    }
    Ok(())
}

/// Merges `overlay` into `target` recursively, preserving keys `overlay` omits.
///
/// This is the nested rule only. Top-level composition goes through
/// [`merge_layer`], which consults the registry.
pub(super) fn deep_merge_tables(target: &mut Table, overlay: &Table) {
    for (key, value) in overlay {
        match (target.get_mut(key), value) {
            (Some(Value::Table(target_table)), Value::Table(overlay_table)) => {
                deep_merge_tables(target_table, overlay_table);
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

/// An empty table supplied where a list is expected reaches the merge because a
/// layer document is untyped; the reference coalesces it away rather than
/// failing, and so does this.
fn absent_if_empty_table(value: &Value) -> Option<&Value> {
    match value {
        Value::Table(table) if table.is_empty() => None,
        value => Some(value),
    }
}

fn entries<'a>(
    field: &str,
    value: &'a Value,
    strategy: MergeStrategy,
) -> Result<&'a Vec<Value>, ConfigError> {
    value.as_array().ok_or_else(|| ConfigError::MergeType {
        field: field.to_owned(),
        strategy: strategy.as_str(),
    })
}

fn merge_concat(target: &mut Table, key: &str, value: &Value) -> Result<(), ConfigError> {
    let Some(overlay) = absent_if_empty_table(value) else {
        return Ok(());
    };
    let Some(existing) = target.get(key).and_then(absent_if_empty_table) else {
        target.insert(key.to_owned(), overlay.clone());
        return Ok(());
    };
    let mut merged = entries(key, existing, MergeStrategy::Concat)?.clone();
    merged.extend(
        entries(key, overlay, MergeStrategy::Concat)?
            .iter()
            .cloned(),
    );
    target.insert(key.to_owned(), Value::Array(merged));
    Ok(())
}

fn merge_union(target: &mut Table, key: &str, value: &Value) -> Result<(), ConfigError> {
    let Some(overlay) = absent_if_empty_table(value) else {
        return Ok(());
    };
    let Some(existing) = target.get(key).and_then(absent_if_empty_table) else {
        // A single operand is coalesced through unkeyed, as the reference does:
        // nothing is being combined, so an entry that carries no merge key
        // reaches the consumer that rejects it by name instead of failing the
        // whole configuration load.
        target.insert(key.to_owned(), overlay.clone());
        return Ok(());
    };

    let mut merged: Vec<Value> = Vec::new();
    let mut positions: Vec<String> = Vec::new();
    for entry in entries(key, existing, MergeStrategy::Union)?
        .iter()
        .chain(entries(key, overlay, MergeStrategy::Union)?)
    {
        let identity = union_identity(key, entry)?;
        match positions.iter().position(|seen| seen == &identity) {
            // First-seen order is preserved: a repeated key updates in place.
            Some(index) => merged[index] = combine_union_entry(key, &merged[index], entry),
            None => {
                positions.push(identity);
                merged.push(entry.clone());
            }
        }
    }
    target.insert(key.to_owned(), Value::Array(merged));
    Ok(())
}

/// The higher layer's entry wins whole, except for the fields listed in
/// [`ENTRY_MERGED_UNION_FIELDS`], where it wins field by field.
fn combine_union_entry(field: &str, existing: &Value, overlay: &Value) -> Value {
    if !ENTRY_MERGED_UNION_FIELDS.contains(&field) {
        return overlay.clone();
    }
    match (existing.as_table(), overlay.as_table()) {
        (Some(existing), Some(overlay)) => {
            let mut merged = existing.clone();
            deep_merge_tables(&mut merged, overlay);
            Value::Table(merged)
        }
        _ => overlay.clone(),
    }
}

fn union_identity(field: &str, entry: &Value) -> Result<String, ConfigError> {
    let merge_key = registry::field(field)
        .and_then(|spec| spec.merge_key)
        .unwrap_or("name");
    entry
        .as_table()
        .and_then(|table| {
            table
                .get(merge_key)
                // A connector written by another client may carry `id` instead,
                // which `connector_preferences` already reads.
                .or_else(|| (field == "connectors").then(|| table.get("id")).flatten())
        })
        .and_then(Value::as_str)
        .filter(|identity| !identity.is_empty())
        .map(str::to_owned)
        // The entry itself is never echoed: it can carry a token or a URL with
        // credentials, and an operator only needs the field and the key.
        .ok_or_else(|| ConfigError::MergeKeyMissing {
            field: field.to_owned(),
            merge_key: merge_key.to_owned(),
        })
}

fn merge_deep(target: &mut Table, key: &str, value: &Value) -> Result<(), ConfigError> {
    match (target.get_mut(key), value) {
        (Some(Value::Table(existing)), Value::Table(overlay)) => {
            deep_merge_tables(existing, overlay);
            Ok(())
        }
        (Some(_), _) => Err(ConfigError::MergeType {
            field: key.to_owned(),
            strategy: MergeStrategy::DeepMerge.as_str(),
        }),
        (None, _) => {
            target.insert(key.to_owned(), value.clone());
            Ok(())
        }
    }
}
