//! Turning a merged document into the one a session runs on.
//!
//! Layering answers what an operator wrote; it does not answer what the session
//! needs. A model named by an experiment has to exist as an entry, an alias no
//! layer pinned has to fall back to one that does, a routed model has to be
//! injected under the alias the router publishes, a compaction model has to
//! name a provider the document declares, and a relative log directory has to
//! be resolved against a home. All of that runs once, after the merge, from
//! [`finalize_effective`].
//!
//! It is separable from the merge because it reads and rewrites one table and
//! knows nothing about where the layers came from or how they are persisted.

use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;
use toml::{Table, Value};

use super::{
    ACTIVE_MODEL_FIELD, ConfigError, ConfigLayer, ROUTED_DEFAULT_MODEL_FIELD,
    ROUTED_MODEL_CONFIG_FIELD, merge, registry,
};
use crate::matching::NameFilter;
use crate::provider::config::{ACTIVE_MODEL_SELECTOR, UtilityFeature, UtilityModels};

/// The models an experiment adds to the picker without changing the default.
const ROUTED_EXTRA_MODELS_FIELD: &str = "routed_extra_models";
/// The patterns narrowing which configured models may be selected.
const ALLOWED_MODELS_FIELD: &str = "allowed_models";
/// The image-capable model that describes images for a blind active model.
const VISION_MODEL_FIELD: &str = "vision_model";
/// The model conversations are compacted with.
const COMPACTION_MODEL_FIELD: &str = "compaction_model";
/// The per-feature model aliases for secondary completions.
const UTILITY_MODELS_FIELD: &str = "utility_models";
/// The compaction threshold, global and per model.
const AUTO_COMPACT_THRESHOLD_FIELD: &str = "auto_compact_threshold";

/// The rules the reference applies once the merged document is validated, in
/// the order its validators run: the routed definitions are coerced and
/// injected, the session log directory is resolved, an emptied model set is
/// rejected, every entry of `models` resolves its compaction threshold, an
/// `active_model` naming nothing configured is unpinned, an `allowed_models`
/// pattern matching nothing, a pinned model the allowlist excludes and a
/// `[utility_models]` alias that names no selectable model are reported, a
/// compaction model on another provider than the active one is reported, and
/// the vision model is checked.
///
/// `admin` is what the organization's managed layer sets, which reference
/// `validate_merged` reads once the validators ran: an allowlist it enforces
/// that admits no model refuses the configuration, and a global compaction
/// threshold it sets is imposed on every model, a threshold the model declares
/// itself included.
///
/// Every rule is skipped when the key it governs is absent. A stack composed
/// without the shipped defaults, which is what a fixture builds, therefore
/// loads unchanged instead of failing on a document the reference could never
/// produce.
pub(super) fn finalize_effective(
    effective: &mut Table,
    vibe_home: &Path,
    admin: &Table,
) -> Result<Vec<String>, ConfigError> {
    resolve_session_log_dir(effective, vibe_home, crate::platform::user_home())?;
    // The reference coerces both routed fields in a `BeforeValidator`, so the
    // definitions are already typed by the time the injections read them, and
    // both run before the model entries are completed.
    let mut warnings: Vec<String> = coerce_routed_model_config(effective).into_iter().collect();
    let extra_models = coerce_routed_extra_models(effective);
    require_configured_model(effective)?;
    inject_routed_model(effective);
    inject_routed_extra_models(effective, extra_models.as_deref().unwrap_or_default());
    complete_model_entries(effective);
    complete_model_field(effective, COMPACTION_MODEL_FIELD);
    complete_model_field(effective, VISION_MODEL_FIELD);
    apply_auto_compact_threshold(effective);
    publish_routed_extra_models(effective, extra_models);
    warnings.extend(apply_active_model_fallback(effective));
    warnings.extend(unmatched_allowed_models(effective));
    warnings.extend(disallowed_active_model(effective));
    warnings.extend(unknown_utility_models(effective));
    // The active-model fallback runs first so the provider comparison reads the
    // model the session will actually use, which is the order the reference's
    // validators run in.
    warnings.extend(cross_provider_compaction_model(effective)?);
    check_vision_model(effective)?;
    if admin.contains_key(ALLOWED_MODELS_FIELD) {
        require_admin_allowlist_match(effective)?;
    }
    if admin.contains_key(AUTO_COMPACT_THRESHOLD_FIELD) {
        impose_global_threshold(effective);
    }
    Ok(warnings)
}

/// Reference `_coerce_routed_extra_models`: the experiments layer carries the
/// extra models as the JSON text of a list, a file carries them as a list, and
/// an entry that is not a model is dropped rather than failing the load, so a
/// malformed rollout payload never stops a session from starting.
///
/// The entries come back as written, without the per-entry defaults: they are
/// injected into `models` first, where the completion every entry goes through
/// fills them, and published on the field afterwards by
/// [`publish_routed_extra_models`]. `None` where no layer set the field.
fn coerce_routed_extra_models(effective: &mut Table) -> Option<Vec<Table>> {
    let raw = effective.remove(ROUTED_EXTRA_MODELS_FIELD)?;
    let items = match raw {
        Value::String(text) => serde_json::from_str::<JsonValue>(&text)
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default(),
        Value::Array(entries) => entries
            .into_iter()
            .filter_map(|entry| serde_json::to_value(entry).ok())
            .collect(),
        _ => Vec::new(),
    };
    Some(items.iter().filter_map(validate_model_definition).collect())
}

/// Publishes the coerced extra models on their own field, each completed as the
/// reference `ModelConfig` completes it, the unset compaction threshold
/// included: the field is not an entry of `models`, so the threshold is never
/// resolved on it. A field no layer set stays absent.
fn publish_routed_extra_models(effective: &mut Table, models: Option<Vec<Table>>) {
    let Some(models) = models else {
        return;
    };
    let published = models
        .into_iter()
        .map(|mut entry| {
            complete_model_definition(&mut entry);
            Value::Table(entry)
        })
        .collect();
    effective.insert(
        ROUTED_EXTRA_MODELS_FIELD.to_owned(),
        Value::Array(published),
    );
}

/// Reference `_inject_routed_extra_models`: each extra model joins `models`
/// under its alias without touching the resolved default. An entry without an
/// alias is skipped, since the empty alias is the unpinned sentinel and would
/// otherwise hijack the default, and an entry the operator already defines
/// keeps every key the operator wrote.
fn inject_routed_extra_models(effective: &mut Table, extra: &[Table]) {
    let Some(models) = effective
        .get_mut(merge::MODELS_FIELD)
        .and_then(Value::as_table_mut)
    else {
        return;
    };
    for entry in extra {
        let Some(alias) = entry
            .get("alias")
            .and_then(Value::as_str)
            .filter(|alias| !alias.is_empty())
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        let injected = overridden_by(entry, models.get(&alias).and_then(Value::as_table));
        models.insert(alias, Value::Table(injected));
    }
}

/// A routed definition with the keys an operator wrote for the same alias laid
/// over it, which is how the reference copies a `ModelConfig` with the fields
/// the operator's entry set.
fn overridden_by(routed: &Table, existing: Option<&Table>) -> Table {
    let mut injected = routed.clone();
    if let Some(existing) = existing {
        for (key, value) in existing {
            injected.insert(key.clone(), value.clone());
        }
    }
    injected
}

/// Reference `_coerce_routed_model_config`: the experiments layer carries the
/// routed definition as the JSON text of a model, which is read back as the
/// model itself.
///
/// The reference returns `None` for text that does not validate, dropping the
/// value silently. This port drops it too and records why: an operator reading
/// `validationWarnings` is the only way an unusable rollout payload is visible
/// from here, where upstream the rollout owner reads it from the service.
fn coerce_routed_model_config(effective: &mut Table) -> Option<String> {
    let raw = effective
        .get(ROUTED_MODEL_CONFIG_FIELD)?
        .as_str()?
        .to_owned();
    let coerced = serde_json::from_str::<JsonValue>(&raw)
        .ok()
        .and_then(|value| validate_model_definition(&value))
        .map(|mut entry| {
            complete_model_definition(&mut entry);
            entry
        });
    match coerced {
        Some(entry) => {
            effective.insert(ROUTED_MODEL_CONFIG_FIELD.to_owned(), Value::Table(entry));
            None
        }
        None => {
            effective.remove(ROUTED_MODEL_CONFIG_FIELD);
            Some("Routed model definition is not a model configuration; ignoring it.".to_owned())
        }
    }
}

/// Fills a routed definition with the per-entry defaults, the same completion
/// an entry of `models` goes through.
///
/// The reference reads `routed_model_config` as a `ModelConfig`, so the field
/// it publishes carries every default the model declares rather than only the
/// keys the rollout payload spelled. `cached_input_price` is the one exception
/// and it is a representation gap rather than a value gap: it defaults to null
/// upstream and TOML carries no null, so an entry that sets none stays without
/// the key, exactly as `registry::MODEL_DEFAULTS` documents for every other
/// model. Its compaction threshold stays unset: the copy injected into
/// `models` resolves one, and the field itself keeps the `ModelConfig` default.
fn complete_model_definition(entry: &mut Table) {
    // Unreachable in a passing suite: `registry_tests` parses the literal.
    let Some(defaults) = serde_json::from_str::<JsonValue>(registry::MODEL_DEFAULTS)
        .ok()
        .and_then(|value| Value::try_from(value).ok())
        .and_then(|value| value.as_table().cloned())
    else {
        return;
    };
    for (key, value) in &defaults {
        entry.entry(key.clone()).or_insert_with(|| value.clone());
    }
    settle_thinking(entry);
}

/// One JSON object read as reference `ModelConfig`, or `None` where the model
/// would refuse it.
///
/// The definition arrives as service-supplied JSON rather than as an operator's
/// TOML, so it is the one model this port reads through the reference's own
/// field types instead of through TOML's: the rollout payload the oracle
/// records writes its prices as JSON strings, which the model reads as the
/// numbers they spell. A key the model does not declare is dropped, as
/// `extra="ignore"` drops it, and the field defaults are left to the completion
/// every other model entry goes through.
pub(super) fn validate_model_definition(value: &JsonValue) -> Option<Table> {
    let object = value.as_object()?;
    let mut entry = Table::new();
    for (key, value) in object {
        // Reference `_default_alias_to_name` replaces a null alias with the
        // name before the field is read, so a null alias is an absent one.
        if key == "alias" && value.is_null() {
            continue;
        }
        if let Some(value) = coerce_model_field(key, value)? {
            entry.insert(key.clone(), value);
        }
    }
    // The two fields the model requires without a default.
    if !entry.contains_key("name") || !entry.contains_key("provider") {
        return None;
    }
    // Reference `_default_alias_to_name`, which every `ModelConfig` carries: a
    // definition naming no alias is addressed by its name.
    if let Some(name) = entry.get("name").cloned()
        && !entry.contains_key("alias")
    {
        entry.insert("alias".to_owned(), name);
    }
    Some(entry)
}

/// One `ModelConfig` field read as the model types it, reference
/// `vibe/core/config/models.py:415` under pydantic's lax mode: `None` where the
/// value does not read, and `Some(None)` where it carries nothing, which is an
/// undeclared key or the null an optional field accepts and TOML cannot hold.
fn coerce_model_field(key: &str, value: &JsonValue) -> Option<Option<Value>> {
    let coerced = match key {
        // A string field reads a string and nothing else, which is the one
        // coercion lax mode refuses.
        "name" | "provider" | "alias" => value.as_str().map(|text| Value::String(text.to_owned())),
        "temperature" | "input_price" | "output_price" => as_number(value).map(Value::Float),
        "cached_input_price" => {
            if value.is_null() {
                return Some(None);
            }
            as_number(value).map(Value::Float)
        }
        "auto_compact_threshold" => as_integer(value).map(Value::Integer),
        // `Field(default=None, ge=1)`: null is the unknown window, and a
        // window below one is refused.
        "max_context_length" => {
            if value.is_null() {
                return Some(None);
            }
            as_integer(value)
                .filter(|window| *window >= 1)
                .map(Value::Integer)
        }
        "thinking_levels" => value
            .as_array()?
            .iter()
            .map(|level| {
                level
                    .as_str()
                    .filter(|level| registry::THINKING_VALUES.contains(level))
                    .map(|level| Value::String(level.to_owned()))
            })
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        "supports_images" => as_flag(value).map(Value::Boolean),
        "thinking" => value
            .as_str()
            .filter(|level| registry::THINKING_VALUES.contains(level))
            .map(|level| Value::String(level.to_owned())),
        _ => return Some(None),
    };
    coerced.map(Some)
}

/// A number as lax mode reads one: the JSON number itself, the boolean read as
/// one or zero, or a string spelling one, surrounding space included.
fn as_number(value: &JsonValue) -> Option<f64> {
    match value {
        JsonValue::Number(number) => number.as_f64(),
        JsonValue::Bool(flag) => Some(f64::from(u8::from(*flag))),
        JsonValue::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// An integer as lax mode reads one: a number carrying no fraction, in the
/// range the field holds. A string spells a decimal or it is not one, which is
/// why `"12.0"` reads as twelve where `"12e0"` reads as nothing.
fn as_integer(value: &JsonValue) -> Option<i64> {
    if let JsonValue::Number(number) = value
        && number.is_i64()
    {
        return number.as_i64();
    }
    if let JsonValue::String(text) = value
        && text.contains(['e', 'E'])
    {
        return None;
    }
    as_number(value)
        .filter(|number| {
            number.is_finite()
                && number.fract() == 0.0
                && *number >= i64::MIN as f64
                && *number <= i64::MAX as f64
        })
        .map(|number| number as i64)
}

/// A boolean as lax mode reads one: the JSON boolean, the number one or zero,
/// or one of the words pydantic spells them with, which it reads without
/// trimming and without regard to case.
fn as_flag(value: &JsonValue) -> Option<bool> {
    match value {
        JsonValue::Bool(flag) => Some(*flag),
        JsonValue::Number(_) => match as_number(value)? {
            1.0 => Some(true),
            0.0 => Some(false),
            _ => None,
        },
        JsonValue::String(text) => match text.to_ascii_lowercase().as_str() {
            "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
            "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Reference `_inject_routed_model`: the alias an experiment routed the
/// installation onto joins `models` with the definition the same experiment
/// supplied, and an entry the operator already defines for that alias keeps
/// every key the operator wrote. A pinned installation gets the entry too: it
/// stays selectable from the picker even though it is not the default.
fn inject_routed_model(effective: &mut Table) {
    let Some(alias) = effective
        .get(ROUTED_DEFAULT_MODEL_FIELD)
        .and_then(Value::as_str)
        .filter(|alias| !alias.is_empty())
        .map(ToOwned::to_owned)
    else {
        return;
    };
    let Some(entry) = effective
        .get(ROUTED_MODEL_CONFIG_FIELD)
        .and_then(Value::as_table)
        .filter(|entry| entry.get("alias").and_then(Value::as_str) == Some(alias.as_str()))
        .cloned()
    else {
        return;
    };
    let Some(models) = effective
        .get_mut(merge::MODELS_FIELD)
        .and_then(Value::as_table_mut)
    else {
        return;
    };
    let injected = overridden_by(&entry, models.get(&alias).and_then(Value::as_table));
    models.insert(alias, Value::Table(injected));
}

/// Reference `available_models`: the aliases of the configured models whose
/// name `allowed_models` admits, in the order they are configured. An empty
/// list admits every model, and so does a list that admits none, which the
/// load reports as a warning rather than leaving no model to select.
///
/// The reference fails closed instead where the organization's managed layer
/// enforces the list, and the load refuses such a list before any reader sees
/// it ([`require_admin_allowlist_match`]), so a loaded document never reaches
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
fn allowed_aliases<'a>(models: &'a Table, patterns: &[String]) -> Vec<&'a str> {
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
fn allowed_model_patterns(effective: &Table) -> Vec<String> {
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

/// Reference `_warn_cross_provider_compaction_model`: a configured compaction
/// model must name a provider the configuration declares, and one served from
/// another provider than the active model is reported, since compaction rides
/// the active model's connection and [`usable_compaction_model`] compacts with
/// the active model instead.
///
/// The refusal and the warning name the alias, because that is what an
/// operator wrote in the file. A configuration that names no compaction model
/// is left alone, and so is one whose active model has no resolvable provider:
/// the reference counts the two as sharing one there.
fn cross_provider_compaction_model(effective: &Table) -> Result<Option<String>, ConfigError> {
    let Some(compaction) = effective
        .get(COMPACTION_MODEL_FIELD)
        .and_then(Value::as_table)
    else {
        return Ok(None);
    };
    let alias = compaction
        .get("alias")
        .or_else(|| compaction.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let provider = compaction
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if !declares_provider(effective, &provider) {
        return Err(ConfigError::CompactionModelProviderMissing { alias, provider });
    }
    if shares_active_provider(effective, &provider) {
        return Ok(None);
    }
    let active = active_model_entry(effective);
    let field = |key: &str| {
        active
            .as_ref()
            .and_then(|entry| entry.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let (active_alias, active_provider) = (
        active_model_alias(effective).unwrap_or_default().to_owned(),
        field("provider"),
    );
    Ok(Some(format!(
        "Compaction model `{alias}` is served by provider `{provider}`, not by \
         `{active_provider}` like the active model; compacting with `{active_alias}` instead."
    )))
}

/// Reference `_shares_active_provider`: whether `provider` is the one the active
/// model is served from. An active model whose provider does not resolve shares
/// it with anything, which is the reference's answer when looking it up raises.
fn shares_active_provider(effective: &Table, provider: &str) -> bool {
    active_model_entry(effective)
        .as_ref()
        .and_then(|entry| entry.get("provider"))
        .and_then(Value::as_str)
        .filter(|name| declares_provider(effective, name))
        .is_none_or(|active| active == provider)
}

/// The configured compaction model a compaction may run on: reference
/// `get_compaction_model` takes it while it shares the active model's provider
/// and compacts with the active model otherwise, so `None` means "the active
/// model".
pub(super) fn usable_compaction_model(effective: &Table) -> Option<&Table> {
    effective
        .get(COMPACTION_MODEL_FIELD)
        .and_then(Value::as_table)
        .filter(|entry| {
            let provider = entry
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or_default();
            shares_active_provider(effective, provider)
        })
}

/// Whether `providers` holds an entry named `name`.
fn declares_provider(effective: &Table, name: &str) -> bool {
    effective
        .get("providers")
        .and_then(Value::as_array)
        .is_some_and(|entries| {
            entries
                .iter()
                .filter_map(Value::as_table)
                .any(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
        })
}

/// The merged entry the active model resolves to, in either persisted shape.
///
/// Reference `_check_compaction_model_provider` compares against
/// `get_active_model`, so the sentinel resolves here rather than reading as an
/// alias no model carries.
fn active_model_entry(effective: &Table) -> Option<Table> {
    let alias = active_model_alias(effective)?;
    match effective.get(merge::MODELS_FIELD) {
        Some(Value::Table(models)) => models.get(alias).and_then(Value::as_table).cloned(),
        Some(Value::Array(models)) => models
            .iter()
            .filter_map(Value::as_table)
            .find(|entry| {
                ["alias", "name"]
                    .into_iter()
                    .any(|key| entry.get(key).and_then(Value::as_str) == Some(alias))
            })
            .cloned(),
        _ => None,
    }
}

/// Fills each merged model entry with the per-entry defaults the reference
/// `ModelConfig` supplies, so a sparse entry reaches a consumer complete.
fn complete_model_entries(effective: &mut Table) {
    // Unreachable in a passing suite: `registry_tests` parses the literal.
    let Some(defaults) = serde_json::from_str::<JsonValue>(registry::MODEL_DEFAULTS)
        .ok()
        .and_then(|value| Value::try_from(value).ok())
        .and_then(|value| value.as_table().cloned())
    else {
        return;
    };
    let Some(models) = effective.get_mut("models").and_then(Value::as_table_mut) else {
        return;
    };
    for (_, entry) in models.iter_mut() {
        if let Some(model) = entry.as_table_mut() {
            for (key, value) in &defaults {
                model.entry(key.clone()).or_insert_with(|| value.clone());
            }
            settle_thinking(model);
        }
    }
}

/// Reference `ModelConfig`'s thinking validators
/// (`vibe/core/config/models.py:495-523`): the offered levels lose their
/// duplicates in first-seen order, an emptied set offers `off` alone, and a
/// stored level the set does not offer is read as the set's default one, `high`
/// when offered and the last level otherwise. Re-derived on every load, so the
/// stored level stays in the operator's file and applies again once the set
/// widens.
fn settle_thinking(model: &mut Table) {
    let Some(Value::Array(levels)) = model.get_mut("thinking_levels") else {
        return;
    };
    let mut offered: Vec<Value> = Vec::new();
    for level in levels.drain(..) {
        if !offered.contains(&level) {
            offered.push(level);
        }
    }
    if offered.is_empty() {
        offered.push(Value::String("off".to_owned()));
    }
    let high = Value::String("high".to_owned());
    let fallback = if offered.contains(&high) {
        Some(high)
    } else {
        offered.last().cloned()
    };
    *levels = offered;
    let stored = model.get("thinking").cloned();
    let offered_now = model
        .get("thinking_levels")
        .and_then(Value::as_array)
        .is_some_and(|levels| stored.as_ref().is_some_and(|level| levels.contains(level)));
    if !offered_now && let Some(fallback) = fallback {
        model.insert("thinking".to_owned(), fallback);
    }
}

/// Fills a model field the same way, and borrows its alias from its name where
/// it declares none.
///
/// Reference `_default_alias_to_name`, bound to `ModelConfig`, which is what
/// `compaction_model` and `vision_model` are typed as: a table carrying a name
/// and a provider is published with an alias and the `ModelConfig` field
/// defaults. Its compaction threshold stays unset: the reference resolves the
/// threshold on the entries of `models` only, so a model field keeps the
/// `ModelConfig` default it was declared with.
fn complete_model_field(effective: &mut Table, field: &str) {
    // Unreachable in a passing suite: `registry_tests` parses the literal.
    let Some(defaults) = serde_json::from_str::<JsonValue>(registry::MODEL_DEFAULTS)
        .ok()
        .and_then(|value| Value::try_from(value).ok())
        .and_then(|value| value.as_table().cloned())
    else {
        return;
    };
    let Some(compaction) = effective.get_mut(field).and_then(Value::as_table_mut) else {
        return;
    };
    if !compaction.contains_key("alias")
        && let Some(name) = compaction.get("name").and_then(Value::as_str)
    {
        let name = name.to_owned();
        compaction.insert("alias".to_owned(), Value::String(name));
    }
    for (key, value) in &defaults {
        compaction
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }
    settle_thinking(compaction);
}

/// Reference `SessionLoggingConfig`: an unset directory falls back to the vibe
/// home's session log directory, and the result is expanded and absolutized.
///
/// `home` is passed in rather than read here so the branch that cannot resolve
/// one is reachable from a test without mutating the process environment, which
/// `unsafe_code` being forbidden rules out.
pub(super) fn resolve_session_log_dir(
    effective: &mut Table,
    vibe_home: &Path,
    home: Option<PathBuf>,
) -> Result<(), ConfigError> {
    const FIELD: &str = "session_logging.save_dir";
    let Some(logging) = effective
        .get_mut("session_logging")
        .and_then(Value::as_table_mut)
    else {
        return Ok(());
    };
    let configured = logging
        .get("save_dir")
        .and_then(Value::as_str)
        .unwrap_or("");
    let candidate = if configured.is_empty() {
        vibe_home.join("logs").join("session")
    } else {
        expand_home(Path::new(configured), home)
            .ok_or(ConfigError::UnresolvablePath { field: FIELD })?
    };
    let resolved = absolutize(candidate).ok_or(ConfigError::UnresolvablePath { field: FIELD })?;
    let Some(rendered) = resolved.to_str() else {
        return Err(ConfigError::UnresolvablePath { field: FIELD });
    };
    logging.insert("save_dir".to_owned(), Value::String(rendered.to_owned()));
    Ok(())
}

/// Replaces a leading `~` with the user's home directory, as the reference
/// `expanduser` does. `None` when the path needs a home directory the caller
/// could not resolve.
fn expand_home(path: &Path, home: Option<PathBuf>) -> Option<PathBuf> {
    let mut components = path.components();
    let Some(first) = components.next() else {
        return Some(path.to_path_buf());
    };
    if first.as_os_str() != "~" {
        return Some(path.to_path_buf());
    }
    let mut home = home?;
    home.extend(components);
    Some(home)
}

/// Anchors a relative path on the working directory without touching the
/// filesystem, so a directory that does not exist yet still resolves.
fn absolutize(path: PathBuf) -> Option<PathBuf> {
    if path.is_absolute() {
        return Some(path);
    }
    Some(std::env::current_dir().ok()?.join(path))
}

/// Reference `_non_empty`: a layer that empties the model set leaves a
/// configuration no turn can run under.
pub(super) fn require_configured_model(effective: &Table) -> Result<(), ConfigError> {
    match effective.get("models") {
        Some(Value::Table(models)) if models.is_empty() => Err(ConfigError::NoConfiguredModel),
        Some(Value::Array(models)) if models.is_empty() => Err(ConfigError::NoConfiguredModel),
        _ => Ok(()),
    }
}

/// Reference `_apply_auto_compact_threshold`: every entry of `models` resolves
/// the threshold it compacts at.
///
/// A threshold some layer set is a cap, honored while the model can reach it:
/// at or above a declared context window the provider refuses the request
/// before compaction ever runs, so such a cap is ignored, and `0` stays a cap
/// meaning "never compact". A model without a usable cap of its own takes the
/// global one under the same condition, then 80% of its window, then the
/// shipped threshold. [`registry::UNSET_AUTO_COMPACT_THRESHOLD`] is how "no layer
/// set one" survives the merge, on the global key and on every entry.
fn apply_auto_compact_threshold(effective: &mut Table) {
    let global_cap = effective
        .get(AUTO_COMPACT_THRESHOLD_FIELD)
        .and_then(Value::as_integer)
        .unwrap_or(registry::UNSET_AUTO_COMPACT_THRESHOLD);
    let global_set = global_cap != registry::UNSET_AUTO_COMPACT_THRESHOLD;
    let global_value = if global_set {
        global_cap
    } else {
        registry::DEFAULT_AUTO_COMPACT_THRESHOLD
    };
    let Some(models) = effective
        .get_mut(merge::MODELS_FIELD)
        .and_then(Value::as_table_mut)
    else {
        return;
    };
    for (_, entry) in models.iter_mut() {
        let Some(model) = entry.as_table_mut() else {
            continue;
        };
        let window = model.get("max_context_length").and_then(Value::as_integer);
        let below_window = |cap: i64| window.is_none_or(|window| cap < window);
        let cap = model
            .get(AUTO_COMPACT_THRESHOLD_FIELD)
            .and_then(Value::as_integer)
            .unwrap_or(registry::UNSET_AUTO_COMPACT_THRESHOLD);
        if cap != registry::UNSET_AUTO_COMPACT_THRESHOLD && below_window(cap) {
            continue;
        }
        let threshold = match window {
            _ if global_set && below_window(global_value) => global_value,
            Some(window) if window != 0 => window_threshold(window),
            _ => global_value,
        };
        model.insert(
            AUTO_COMPACT_THRESHOLD_FIELD.to_owned(),
            Value::Integer(threshold),
        );
    }
}

/// The share of a context window a model compacts at, truncated as the
/// reference's `int(window * AUTO_COMPACT_WINDOW_RATIO)` truncates it.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "the reference computes in binary floating point and truncates toward zero; \
              a window is a token count far inside the exactly representable range"
)]
fn window_threshold(window: i64) -> i64 {
    (window as f64 * registry::AUTO_COMPACT_WINDOW_RATIO) as i64
}

/// Reference `_apply_active_model_fallback`: an `active_model` naming nothing
/// configured is unpinned and a warning names the model the installation now
/// resolves to, instead of failing the load.
///
/// The document is rewritten to the unpinned sentinel rather than to that
/// model, so a routed default still selects the model on read and the view
/// reports the installation as unpinned. The sentinel itself names nothing on
/// purpose and is left alone.
fn apply_active_model_fallback(effective: &mut Table) -> Option<String> {
    let models = effective
        .get(merge::MODELS_FIELD)
        .and_then(Value::as_table)?;
    let active = effective
        .get(ACTIVE_MODEL_FIELD)
        .and_then(Value::as_str)
        .filter(|alias| !alias.is_empty())?;
    if models.contains_key(active) {
        return None;
    }
    let unknown = active.to_owned();
    let fallback = default_model_alias(effective)
        .unwrap_or(registry::DEFAULT_ACTIVE_MODEL_ALIAS)
        .to_owned();
    effective.insert(
        ACTIVE_MODEL_FIELD.to_owned(),
        Value::String(registry::UNPINNED_ACTIVE_MODEL.to_owned()),
    );
    Some(format!(
        "Active model `{unknown}` is not configured; using the default model `{fallback}`."
    ))
}

/// Reference `_warn_unmatched_allowed_models`: one warning per non-blank
/// `allowed_models` pattern that admits no configured model, in list order.
fn unmatched_allowed_models(effective: &Table) -> Vec<String> {
    let Some(models) = effective.get(merge::MODELS_FIELD).and_then(Value::as_table) else {
        return Vec::new();
    };
    allowed_model_patterns(effective)
        .into_iter()
        .filter(|pattern| !pattern.trim().is_empty())
        .filter(|pattern| allowed_aliases(models, std::slice::from_ref(pattern)).is_empty())
        .map(|pattern| format!("Allowed model `{pattern}` matches no configured model."))
        .collect()
}

/// Reference `_warn_disallowed_active_model`: a pinned model `allowed_models`
/// excludes never runs, and the load says which model runs instead.
fn disallowed_active_model(effective: &Table) -> Option<String> {
    effective
        .get(merge::MODELS_FIELD)
        .and_then(Value::as_table)?;
    let pinned = effective
        .get(ACTIVE_MODEL_FIELD)
        .and_then(Value::as_str)
        .filter(|alias| !alias.is_empty())?;
    if available_model_aliases(effective).contains(&pinned) {
        return None;
    }
    let fallback = default_model_alias(effective).unwrap_or(registry::DEFAULT_ACTIVE_MODEL_ALIAS);
    Some(format!(
        "Active model `{pinned}` is excluded by `allowed_models`; using the default model \
         `{fallback}`."
    ))
}

/// Reference `_warn_unknown_utility_models`: a `[utility_models]` alias that
/// names no selectable model leaves its feature on automatic selection, which
/// the load reports, feature by feature in the order the reference declares
/// them. `active` selects the active model unless a model carries that alias.
fn unknown_utility_models(effective: &Table) -> Vec<String> {
    let utility = UtilityModels::from_table(
        effective
            .get(UTILITY_MODELS_FIELD)
            .and_then(Value::as_table),
    );
    let configured = effective.get(merge::MODELS_FIELD).and_then(Value::as_table);
    let available = available_model_aliases(effective);
    UtilityFeature::ALL
        .into_iter()
        .filter_map(|feature| {
            let alias = utility.alias_for(feature);
            let declared = configured.is_some_and(|models| models.contains_key(alias));
            if alias.is_empty()
                || available.contains(&alias)
                || (alias == ACTIVE_MODEL_SELECTOR && !declared)
            {
                return None;
            }
            let reason = if declared {
                "is excluded by `allowed_models`"
            } else {
                "names no configured model"
            };
            Some(format!(
                "Utility model `{alias}` for `{}` {reason}; that feature selects its model \
                 automatically.",
                feature.key()
            ))
        })
        .collect()
}

/// Reference `VibeConfigSchema.validate_merged`: an allowlist the
/// organization's managed layer enforces must fail closed, so one that admits
/// none of the configured models refuses the configuration rather than letting
/// a model the operator configured run.
fn require_admin_allowlist_match(effective: &Table) -> Result<(), ConfigError> {
    let patterns = allowed_model_patterns(effective);
    let Some(models) = effective.get(merge::MODELS_FIELD).and_then(Value::as_table) else {
        return Ok(());
    };
    if patterns.is_empty() || !allowed_aliases(models, &patterns).is_empty() {
        return Ok(());
    }
    Err(ConfigError::AdminAllowlistMatchesNothing)
}

/// Reference `_check_vision_model`: a configured vision model must be able to
/// see images and must name a provider the configuration declares. Unlike the
/// compaction model it may cross providers, since describing an image is a
/// completion of its own.
fn check_vision_model(effective: &Table) -> Result<(), ConfigError> {
    let Some(vision) = effective.get(VISION_MODEL_FIELD).and_then(Value::as_table) else {
        return Ok(());
    };
    let alias = vision
        .get("alias")
        .or_else(|| vision.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if vision.get("supports_images").and_then(Value::as_bool) != Some(true) {
        return Err(ConfigError::VisionModelCannotSee { alias });
    }
    let provider = vision
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if !declares_provider(effective, &provider) {
        return Err(ConfigError::VisionModelProviderMissing { alias, provider });
    }
    Ok(())
}

/// Reference `VibeConfigSchema.validate_merged`: a compaction threshold the
/// organization's managed layer sets is imposed on every model, overriding the
/// threshold a model declares for itself.
fn impose_global_threshold(effective: &mut Table) {
    let Some(global) = effective
        .get("auto_compact_threshold")
        .and_then(Value::as_integer)
    else {
        return;
    };
    let Some(models) = effective
        .get_mut(merge::MODELS_FIELD)
        .and_then(Value::as_table_mut)
    else {
        return;
    };
    for (_, entry) in models.iter_mut() {
        if let Some(model) = entry.as_table_mut() {
            model.insert("auto_compact_threshold".to_owned(), Value::Integer(global));
        }
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
