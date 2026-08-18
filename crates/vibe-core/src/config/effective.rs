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

/// The rules the reference applies once the merged document is validated, in
/// the order its validators run: the routed model definition is coerced and
/// injected, the session log directory is resolved, an emptied model set is
/// rejected, the global compaction threshold reaches the models that set none,
/// the routed definition takes the shipped one instead, and an `active_model`
/// naming nothing configured falls back to the first model.
///
/// Every rule is skipped when the key it governs is absent. A stack composed
/// without the shipped defaults, which is what a fixture builds, therefore
/// loads unchanged instead of failing on a document the reference could never
/// produce.
pub(super) fn finalize_effective(
    effective: &mut Table,
    vibe_home: &Path,
    model_order: &[String],
) -> Result<Vec<String>, ConfigError> {
    resolve_session_log_dir(effective, vibe_home, user_home_directory())?;
    // The reference coerces `routed_model_config` in a `BeforeValidator`, so
    // the definition is already typed by the time `_inject_routed_model` reads
    // it, and both run before the model entries are completed.
    let mut warnings: Vec<String> = coerce_routed_model_config(effective).into_iter().collect();
    inject_routed_model(effective);
    require_configured_model(effective)?;
    complete_model_entries(effective);
    complete_compaction_model(effective);
    propagate_auto_compact_threshold(effective);
    complete_routed_threshold(effective);
    warnings.extend(apply_active_model_fallback(effective, model_order));
    // The active-model fallback runs first so the provider comparison reads the
    // model the session will actually use, which is the order the reference's
    // validators run in.
    check_compaction_model_provider(effective)?;
    Ok(warnings)
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
/// model. `auto_compact_threshold` is filled by
/// [`complete_routed_threshold`], after the injected copy has taken the global
/// value the reference gives it.
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
}

/// The compaction threshold the routed definition itself publishes, which is
/// the shipped one rather than the operator's global value.
///
/// The reference splits the two: `_apply_global_auto_compact_threshold`
/// rewrites the entries of `models`, and the copy `_inject_routed_model` put
/// there is one of them, but `routed_model_config` is a field of the schema
/// rather than an entry of the map, so it keeps the default `ModelConfig`
/// declares. Measured against the pinned reference: a document setting
/// `auto_compact_threshold = 50000` publishes 50000 on the injected model and
/// 200000 on `routed_model_config`. Running after the propagation is what keeps
/// the two apart, since a threshold written before the injection would travel
/// into the map with the definition.
fn complete_routed_threshold(effective: &mut Table) {
    let Some(entry) = effective
        .get_mut(ROUTED_MODEL_CONFIG_FIELD)
        .and_then(Value::as_table_mut)
    else {
        return;
    };
    entry
        .entry("auto_compact_threshold".to_owned())
        .or_insert(Value::Integer(registry::DEFAULT_AUTO_COMPACT_THRESHOLD));
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
fn validate_model_definition(value: &JsonValue) -> Option<Table> {
    let object = value.as_object()?;
    let mut entry = Table::new();
    for (key, value) in object {
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

/// Reference `_inject_routed_model`: an unpinned installation whose experiment
/// routed it onto an alias the configuration does not declare gets the
/// definition the same experiment supplied.
///
/// A pinned `active_model` skips the injection outright, because the routed
/// alias can never be selected for that installation.
fn inject_routed_model(effective: &mut Table) {
    if effective
        .get(ACTIVE_MODEL_FIELD)
        .and_then(Value::as_str)
        .is_some_and(|alias| alias != registry::UNPINNED_ACTIVE_MODEL)
    {
        return;
    }
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
    if models.contains_key(&alias) {
        return;
    }
    models.insert(alias, Value::Table(entry));
}

/// Reference `resolve_default_model_alias`: the routed alias when it names a
/// configured model, the shipped default alias when that one is configured, and
/// the first configured model otherwise.
#[must_use]
pub fn default_model_alias(effective: &Table) -> Option<&str> {
    let models = effective.get(merge::MODELS_FIELD)?.as_table()?;
    let routed = effective
        .get(ROUTED_DEFAULT_MODEL_FIELD)
        .and_then(Value::as_str)
        .filter(|alias| !alias.is_empty() && models.contains_key(*alias));
    routed
        .or_else(|| {
            models
                .contains_key(registry::DEFAULT_ACTIVE_MODEL_ALIAS)
                .then_some(registry::DEFAULT_ACTIVE_MODEL_ALIAS)
        })
        .or_else(|| models.keys().next().map(String::as_str))
}

/// Reference `get_active_model`'s alias step: the pinned alias, or the resolved
/// default when the operator pinned nothing.
///
/// Every reader of the active model goes through this rather than through
/// `active_model` itself, because the merged document carries the reference's
/// unpinned sentinel where an installation was never pinned.
#[must_use]
pub fn active_model_alias(effective: &Table) -> Option<&str> {
    effective
        .get(ACTIVE_MODEL_FIELD)
        .and_then(Value::as_str)
        .filter(|alias| *alias != registry::UNPINNED_ACTIVE_MODEL)
        .or_else(|| default_model_alias(effective))
}

/// Reference `_check_compaction_model_provider`: a configured compaction model
/// must name a provider the configuration declares, and it must be the provider
/// the active model is served from.
///
/// Both refusals name the alias, because that is what an operator wrote in the
/// file. A configuration that names no compaction model is left alone, and so is
/// one whose active model has no resolvable provider: the reference gives up on
/// the comparison there rather than reporting the compaction model for it.
fn check_compaction_model_provider(effective: &Table) -> Result<(), ConfigError> {
    let Some(compaction) = effective.get("compaction_model").and_then(Value::as_table) else {
        return Ok(());
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
    let active = active_model_entry(effective);
    let Some(active_provider) = active
        .as_ref()
        .and_then(|entry| entry.get("provider"))
        .and_then(Value::as_str)
        .filter(|name| declares_provider(effective, name))
    else {
        return Ok(());
    };
    if active_provider != provider {
        return Err(ConfigError::CompactionModelProviderMismatch {
            alias,
            provider,
            active_provider: active_provider.to_owned(),
        });
    }
    Ok(())
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
        }
    }
}

/// Fills the compaction model the same way, and borrows its alias from its name
/// where it declares none.
///
/// Reference `_default_alias_to_name`, bound to `ModelConfig`, which is what
/// `compaction_model` is typed as: a `[compaction_model]` table carrying a name
/// and a provider is published with an alias, the `ModelConfig` field defaults
/// and the global compaction threshold, exactly as an entry of `models` is.
fn complete_compaction_model(effective: &mut Table) {
    // Unreachable in a passing suite: `registry_tests` parses the literal.
    let Some(defaults) = serde_json::from_str::<JsonValue>(registry::MODEL_DEFAULTS)
        .ok()
        .and_then(|value| Value::try_from(value).ok())
        .and_then(|value| value.as_table().cloned())
    else {
        return;
    };
    let global = effective
        .get("auto_compact_threshold")
        .and_then(Value::as_integer);
    let Some(compaction) = effective
        .get_mut("compaction_model")
        .and_then(Value::as_table_mut)
    else {
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
    if let Some(global) = global {
        compaction
            .entry("auto_compact_threshold".to_owned())
            .or_insert(Value::Integer(global));
    }
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

/// The operator's home directory, or [`None`] when the environment names none.
///
/// The shell policy expands a `~` operand through this before positioning it
/// against the workspace roots, so `cat ~/.ssh/id_rsa` is measured where it
/// actually reads rather than under a literal `~` directory in the workspace.
pub(crate) fn user_home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .or_else(|| {
                let mut home = PathBuf::from(std::env::var_os("HOMEDRIVE")?);
                home.push(std::env::var_os("HOMEPATH")?);
                Some(home)
            })
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
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

/// Reference `_apply_global_auto_compact_threshold`: a model that declares no
/// threshold inherits the global one, and a model that declares its own keeps it.
fn propagate_auto_compact_threshold(effective: &mut Table) {
    let Some(global) = effective
        .get("auto_compact_threshold")
        .and_then(Value::as_integer)
    else {
        return;
    };
    let Some(models) = effective.get_mut("models").and_then(Value::as_table_mut) else {
        return;
    };
    for (_, entry) in models.iter_mut() {
        if let Some(model) = entry.as_table_mut()
            && !model.contains_key("auto_compact_threshold")
        {
            model.insert("auto_compact_threshold".to_owned(), Value::Integer(global));
        }
    }
}

/// Reference `_apply_active_model_fallback`: an `active_model` naming nothing
/// configured selects the first configured model and records a warning instead
/// of failing the load.
///
/// The unpinned sentinel names nothing on purpose and is left alone, exactly as
/// the reference guard leaves it: it is resolved by [`active_model_alias`] when
/// the alias is read, never by rewriting the document.
fn apply_active_model_fallback(effective: &mut Table, model_order: &[String]) -> Option<String> {
    let models = effective
        .get(merge::MODELS_FIELD)
        .and_then(Value::as_table)?;
    if models.is_empty() {
        return None;
    }
    let active = effective
        .get(ACTIVE_MODEL_FIELD)
        .and_then(Value::as_str)
        .filter(|alias| !alias.is_empty())?;
    if models.contains_key(active) {
        return None;
    }
    let unknown = active.to_owned();
    let fallback = model_order
        .iter()
        .find(|alias| models.contains_key(alias.as_str()))
        .cloned()
        .or_else(|| models.keys().next().cloned())?;
    effective.insert("active_model".to_owned(), Value::String(fallback.clone()));
    Some(format!(
        "Active model `{unknown}` is not configured; falling back to `{fallback}`."
    ))
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
