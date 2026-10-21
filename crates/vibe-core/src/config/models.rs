//! Which model a session runs on: the configured entries, the allowlist
//! narrowing them, and the alias and entry every reader resolves.
//!
//! Reference `available_models`, `resolve_default_model_alias` and
//! `get_active_model`. The rules run over a merged document, which carries
//! `models` keyed by alias, or over one layer's, which lists them.

use toml::{Table, Value};

use super::{ACTIVE_MODEL_FIELD, ConfigLayer, ROUTED_DEFAULT_MODEL_FIELD, merge, registry};
use crate::matching::NameFilter;

/// The patterns narrowing which configured models may be selected.
pub(super) const ALLOWED_MODELS_FIELD: &str = "allowed_models";

/// Reference `available_models`: the aliases of the configured models whose
/// name `allowed_models` admits, in the order they are configured. An empty
/// list admits every model, and so does a list that admits none, which the
/// load reports as a warning rather than leaving no model to select.
///
/// The reference fails closed instead where the organization's managed layer
/// enforces the list, and the load refuses such a list before any reader sees
/// it (`require_admin_allowlist_match`), so a loaded document never reaches
/// the degraded branch under an enforced list.
#[must_use]
pub fn available_model_aliases(effective: &Table) -> Vec<&str> {
    let Some(models) = effective.get(merge::MODELS_FIELD).and_then(Value::as_table) else {
        return Vec::new();
    };
    let every = models.keys().map(String::as_str).collect::<Vec<_>>();
    let patterns = allowed_model_patterns(effective);
    if patterns.is_empty() {
        return every;
    }
    let allowed = allowed_aliases(models, &patterns);
    if allowed.is_empty() { every } else { allowed }
}

/// The aliases of the models whose name one of `patterns` admits.
pub(super) fn allowed_aliases<'a>(models: &'a Table, patterns: &[String]) -> Vec<&'a str> {
    let filter = NameFilter::new(patterns);
    models
        .iter()
        .filter(|(alias, entry)| filter.matches(model_name(alias, entry)))
        .map(|(alias, _)| alias.as_str())
        .collect()
}

/// The API name of a merged entry, which is what `allowed_models` matches. An
/// entry is completed from its layers before any reader runs; a hand-built one
/// naming nothing is addressed by its alias.
fn model_name<'a>(alias: &'a str, entry: &'a Value) -> &'a str {
    entry.get("name").and_then(Value::as_str).unwrap_or(alias)
}

/// The `allowed_models` entries, blank ones included: the filter skips them.
pub(super) fn allowed_model_patterns(effective: &Table) -> Vec<String> {
    effective
        .get(ALLOWED_MODELS_FIELD)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// Reference `resolve_default_model_alias`: the routed alias when it names an
/// available model, the shipped default alias when that one is available or
/// nothing is, and the first available model otherwise.
#[must_use]
pub fn default_model_alias(effective: &Table) -> Option<&str> {
    let models = effective.get(merge::MODELS_FIELD)?.as_table()?;
    let available = available_model_aliases(effective);
    let routed = effective
        .get(ROUTED_DEFAULT_MODEL_FIELD)
        .and_then(Value::as_str)
        .filter(|alias| !alias.is_empty() && available.contains(alias));
    routed
        .or_else(|| {
            (available.contains(&registry::DEFAULT_ACTIVE_MODEL_ALIAS) || available.is_empty())
                .then_some(registry::DEFAULT_ACTIVE_MODEL_ALIAS)
                .filter(|alias| models.contains_key(*alias))
        })
        .or_else(|| available.first().copied())
}

/// Reference `get_active_model`'s alias step: the pinned alias while
/// `allowed_models` admits it, or the resolved default when the operator
/// pinned nothing or pinned a model the list excludes. A pin naming no
/// configured model resolves to nothing, where the reference raises; the load
/// unpins such a pin, so only a table built by hand reaches that branch.
///
/// Every reader of the active model goes through this rather than through
/// `active_model` itself, because the merged document carries the reference's
/// unpinned sentinel where an installation was never pinned.
#[must_use]
pub fn active_model_alias(effective: &Table) -> Option<&str> {
    let pinned = effective
        .get(ACTIVE_MODEL_FIELD)
        .and_then(Value::as_str)
        .filter(|alias| *alias != registry::UNPINNED_ACTIVE_MODEL);
    if let Some(alias) = pinned {
        let configured = effective
            .get(merge::MODELS_FIELD)
            .and_then(Value::as_table)
            .is_some_and(|models| models.contains_key(alias));
        if !configured {
            return None;
        }
        if available_model_aliases(effective).contains(&alias) {
            return Some(alias);
        }
    }
    default_model_alias(effective)
}

/// The entry the active model resolves to.
///
/// Reference `get_active_model`, which `_check_compaction_model_provider`
/// compares against, so the unpinned sentinel resolves here rather than
/// reading as an alias no model carries.
pub(super) fn active_model_entry(effective: &Table) -> Option<&Table> {
    model_entry(
        effective.get(merge::MODELS_FIELD)?,
        active_model_alias(effective)?,
    )
}

/// The entry for `alias` in a `models` value, in either persisted shape: the
/// alias-keyed table a merge composes, or the `[[models]]` list a layer
/// carries, where an entry answers to its alias, or to its name when it
/// declares none. Reference `_default_alias_to_name`, which is also how
/// [`merge`] keys a listed entry.
pub(super) fn model_entry<'a>(models: &'a Value, alias: &str) -> Option<&'a Table> {
    match models {
        Value::Table(models) => models.get(alias)?.as_table(),
        Value::Array(entries) => entries.iter().filter_map(Value::as_table).find(|entry| {
            entry
                .get("alias")
                .or_else(|| entry.get("name"))
                .and_then(Value::as_str)
                == Some(alias)
        }),
        _ => None,
    }
}

/// The aliases in the order the layers declare them, lowest layer first.
///
/// The merged model map is keyed rather than ordered, so the order a persisted
/// list was written in is recovered here; it is what "the first configured
/// model" means when an `active_model` has to fall back.
pub(super) fn model_order(layers: &[ConfigLayer]) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    for layer in layers {
        let aliases = match layer.values.get("models") {
            Some(Value::Array(entries)) => entries
                .iter()
                .filter_map(|entry| {
                    let table = entry.as_table()?;
                    table
                        .get("alias")
                        .or_else(|| table.get("name"))?
                        .as_str()
                        .map(str::to_owned)
                })
                .collect::<Vec<_>>(),
            Some(Value::Table(entries)) => entries.keys().cloned().collect(),
            _ => Vec::new(),
        };
        for alias in aliases {
            if !order.contains(&alias) {
                order.push(alias);
            }
        }
    }
    order
}
