//! Reading `hooks.toml`.
//!
//! Reference `vibe/core/hooks/config.py` validates each file with Pydantic in
//! its lax mode: a root that ignores unknown keys, then one `HookConfig` per
//! `[[hooks]]` entry. An invalid entry is skipped with an issue that names it,
//! a file that does not parse is skipped whole, and a name a previous file
//! already declared is skipped as a duplicate, so the project files, which are
//! read first, win over the user file.
//!
//! The issue messages are the reference's: Pydantic's messages, joined the way
//! `_format_validation_error` joins them, behind the entry's label. The
//! coercions are Pydantic's lax ones for the values TOML can hold: a number
//! from a numeric string or a boolean, a boolean from `0`, `1`, `0.0`, `1.0`
//! or one of twelve words.

use std::path::{Path, PathBuf};

use toml::Value;

use super::models::{
    DEFAULT_HOOK_TIMEOUT, HookConfig, HookConfigIssue, HookConfigResult, HookType,
};
use crate::mcp::render::{python_float, python_string};

/// Loads one hook file. A missing file is no hooks and no issue. Reference
/// `load_hooks_file`.
#[must_use]
pub fn load_hooks_file(path: &Path) -> HookConfigResult {
    load_hooks_file_with(path, false)
}

/// Loads one hook file, in the reference's strict mode when `strict` is set:
/// the root and every entry then forbid keys they do not declare, and the
/// root's `hooks` must already be a list. A plugin's `hooks.toml` is read this
/// way. Reference `load_hooks_file(path, strict=True)`.
#[must_use]
pub fn load_hooks_file_with(path: &Path, strict: bool) -> HookConfigResult {
    let mut result = HookConfigResult::default();
    if !path.is_file() {
        return result;
    }
    let data = match std::fs::read(path) {
        Ok(bytes) => {
            match crate::workspace::text_file::decode(&bytes)
                .text
                .parse::<toml::Table>()
            {
                Ok(table) => table,
                Err(error) => {
                    result.issues.push(issue(
                        path,
                        format!("Failed to parse: {}", toml_error_message(&error)),
                    ));
                    return result;
                }
            }
        }
        Err(error) => {
            result.issues.push(issue(
                path,
                format!("Failed to parse: {}", os_error(&error, path)),
            ));
            return result;
        }
    };
    if strict {
        let extra: Vec<FieldError> = data
            .keys()
            .filter(|key| key.as_str() != "hooks")
            .map(|key| (leak_location(key), EXTRA_FORBIDDEN.to_owned()))
            .collect();
        if !extra.is_empty() {
            result
                .issues
                .push(issue(path, format_errors(&extra, "hooks file")));
            return result;
        }
    }
    let entries = match data.get("hooks") {
        None => return result,
        Some(Value::Array(entries)) => entries,
        Some(_) => {
            result.issues.push(issue(
                path,
                "hooks: Input should be a valid list".to_owned(),
            ));
            return result;
        }
    };
    for (index, entry) in entries.iter().enumerate() {
        let validated = if strict {
            validate_strict_entry(entry)
        } else {
            validate_entry(entry)
        };
        match validated {
            Ok(hook) => result.hooks.push(hook),
            Err(errors) => {
                let label = entry_label(entry, index);
                result.issues.push(issue(
                    path,
                    format!("{label} - {}", format_errors(&errors, "hook")),
                ));
            }
        }
    }
    result
}

/// Loads every hook file in order, dropping a name an earlier file already
/// declared. Reference `load_hooks_from_fs`, fed the files
/// [`crate::config::harness::HarnessFiles::hook_files`] lists.
#[must_use]
pub fn load_hooks_from_fs(files: &[PathBuf]) -> HookConfigResult {
    let mut all = HookConfigResult::default();
    let mut seen = std::collections::BTreeSet::new();
    for path in files {
        let result = load_hooks_file(path);
        all.issues.extend(result.issues);
        for hook in result.hooks {
            if !seen.insert(hook.name.clone()) {
                all.issues.push(issue(
                    path,
                    format!("Duplicate hook name: {}", python_string(&hook.name)),
                ));
                continue;
            }
            all.hooks.push(hook);
        }
    }
    all
}

fn issue(path: &Path, message: String) -> HookConfigIssue {
    HookConfigIssue {
        file: path.to_path_buf(),
        message,
    }
}

/// Python's `str(OSError)`, which is what a read failure reports.
fn os_error(error: &std::io::Error, path: &Path) -> String {
    let description = error.to_string();
    let description = description
        .split_once(" (os error")
        .map_or(description.as_str(), |(text, _)| text);
    match error.raw_os_error() {
        Some(code) => format!(
            "[Errno {code}] {description}: {}",
            python_string(&path.to_string_lossy())
        ),
        None => description.to_owned(),
    }
}

/// The parser's message, with the position the way `tomllib` states one.
fn toml_error_message(error: &toml::de::Error) -> String {
    let message = error.message().trim_end_matches('.');
    let mut message = message.to_owned();
    if let Some(first) = message.get(..1) {
        let upper = first.to_uppercase();
        message.replace_range(..1, &upper);
    }
    message
}

/// One validation error: where it happened, and Pydantic's message.
type FieldError = (&'static str, String);

const FIELD_REQUIRED: &str = "Field required";
const VALID_STRING: &str = "Input should be a valid string";
const VALID_NUMBER: &str = "Input should be a valid number";
const UNPARSABLE_NUMBER: &str =
    "Input should be a valid number, unable to parse string as a number";
const VALID_BOOLEAN: &str = "Input should be a valid boolean";
const UNINTERPRETABLE_BOOLEAN: &str = "Input should be a valid boolean, unable to interpret input";
const VALID_TYPE: &str = "Input should be 'post_agent', 'pre_tool' or 'post_tool'";
const VALID_ENTRY: &str = "Input should be a valid dictionary or instance of HookConfig";
const EXTRA_FORBIDDEN: &str = "Extra inputs are not permitted";
const DECLARED_FIELDS: [&str; 7] = [
    "name",
    "type",
    "command",
    "match",
    "timeout",
    "strict",
    "description",
];

/// A location for an undeclared key. Locations are `'static` because the
/// declared ones are; an undeclared key is rare enough to leak its name.
fn leak_location(key: &str) -> &'static str {
    Box::leak(key.to_owned().into_boxed_str())
}

/// Reference `_StrictHookConfig`: the lax entry model that also forbids keys it
/// does not declare.
fn validate_strict_entry(entry: &Value) -> Result<HookConfig, Vec<FieldError>> {
    let extra: Vec<FieldError> = entry
        .as_table()
        .map(|table| {
            table
                .keys()
                .filter(|key| !DECLARED_FIELDS.contains(&key.as_str()))
                .map(|key| (leak_location(key), EXTRA_FORBIDDEN.to_owned()))
                .collect()
        })
        .unwrap_or_default();
    match validate_entry(entry) {
        Ok(hook) if extra.is_empty() => Ok(hook),
        Ok(_) => Err(extra),
        Err(mut errors) => {
            errors.extend(extra);
            Err(errors)
        }
    }
}

/// Reference `_format_validation_error`: `loc: msg` per error, joined by
/// ` ; `, with the root label standing in for an empty location.
fn format_errors(errors: &[FieldError], root_label: &str) -> String {
    errors
        .iter()
        .map(|(location, message)| {
            let location = if location.is_empty() {
                root_label
            } else {
                location
            };
            format!("{location}: {message}")
        })
        .collect::<Vec<_>>()
        .join(" ; ")
}

/// Reference `_hook_entry_label`: the entry's name when it has a truthy one,
/// its position otherwise.
fn entry_label(entry: &Value, index: usize) -> String {
    match entry.as_table().and_then(|table| table.get("name")) {
        Some(name) if truthy(name) => python_str(name),
        _ => format!("hooks[{index}]"),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::String(text) => !text.is_empty(),
        Value::Integer(number) => *number != 0,
        Value::Float(number) => *number != 0.0,
        Value::Boolean(flag) => *flag,
        Value::Array(items) => !items.is_empty(),
        Value::Table(table) => !table.is_empty(),
        Value::Datetime(_) => true,
    }
}

/// Python's `str` of the value `tomllib` parses this TOML into.
fn python_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => python_repr(other),
    }
}

/// Python's `repr` of the value `tomllib` parses this TOML into.
fn python_repr(value: &Value) -> String {
    match value {
        Value::String(text) => python_string(text),
        Value::Integer(number) => number.to_string(),
        Value::Float(number) => python_float(*number),
        Value::Boolean(true) => "True".to_owned(),
        Value::Boolean(false) => "False".to_owned(),
        Value::Datetime(datetime) => datetime.to_string(),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Table(table) => format!(
            "{{{}}}",
            table
                .iter()
                .map(|(key, item)| format!("{}: {}", python_string(key), python_repr(item)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Reference `HookConfig.model_validate` in lax mode.
fn validate_entry(entry: &Value) -> Result<HookConfig, Vec<FieldError>> {
    let Some(table) = entry.as_table() else {
        return Err(vec![("", VALID_ENTRY.to_owned())]);
    };
    let mut errors = Vec::new();
    let name = required_string(table, "name", &mut errors);
    let hook_type = match table.get("type") {
        None => {
            errors.push(("type", FIELD_REQUIRED.to_owned()));
            None
        }
        Some(value) => {
            let parsed = value.as_str().and_then(HookType::parse);
            if parsed.is_none() {
                errors.push(("type", VALID_TYPE.to_owned()));
            }
            parsed
        }
    };
    let command = required_string(table, "command", &mut errors);
    if command
        .as_deref()
        .is_some_and(|command| command.trim().is_empty())
    {
        errors.push((
            "command",
            "Value error, command must not be empty".to_owned(),
        ));
    }
    let matcher = optional_string(table, "match", &mut errors);
    if matcher
        .as_deref()
        .is_some_and(|matcher| matcher.trim().is_empty())
    {
        errors.push(("match", "Value error, match must not be empty".to_owned()));
    }
    let timeout = match table.get("timeout") {
        None => Some(DEFAULT_HOOK_TIMEOUT),
        Some(value) => match lax_float(value) {
            Ok(timeout) => Some(timeout),
            Err(message) => {
                errors.push(("timeout", message.to_owned()));
                None
            }
        },
    };
    let strict = match table.get("strict") {
        None => Some(false),
        Some(value) => match lax_bool(value) {
            Ok(strict) => Some(strict),
            Err(message) => {
                errors.push(("strict", message.to_owned()));
                None
            }
        },
    };
    let description = optional_string(table, "description", &mut errors);
    let (Some(name), Some(hook_type), Some(command), Some(timeout), Some(strict)) =
        (name, hook_type, command, timeout, strict)
    else {
        return Err(errors);
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    // Reference `_apply_defaults_and_constraints`, which runs only once every
    // field validated.
    if hook_type == HookType::PostAgent && matcher.is_some() {
        return Err(vec![(
            "",
            "Value error, match is only valid for tool hooks (pre_tool / post_tool)".to_owned(),
        )]);
    }
    if hook_type == HookType::PostAgent && strict {
        return Err(vec![(
            "",
            "Value error, strict is only valid for tool hooks (pre_tool / post_tool)".to_owned(),
        )]);
    }
    Ok(HookConfig {
        name,
        hook_type,
        command,
        matcher,
        timeout,
        strict,
        description,
    })
}

fn required_string(
    table: &toml::Table,
    key: &'static str,
    errors: &mut Vec<FieldError>,
) -> Option<String> {
    match table.get(key) {
        None => {
            errors.push((key, FIELD_REQUIRED.to_owned()));
            None
        }
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => {
            errors.push((key, VALID_STRING.to_owned()));
            None
        }
    }
}

fn optional_string(
    table: &toml::Table,
    key: &'static str,
    errors: &mut Vec<FieldError>,
) -> Option<String> {
    match table.get(key) {
        None => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => {
            errors.push((key, VALID_STRING.to_owned()));
            None
        }
    }
}

/// Pydantic's lax `float`: numbers, booleans, and the strings Python's
/// `float()` reads.
fn lax_float(value: &Value) -> Result<f64, &'static str> {
    match value {
        Value::Float(number) => Ok(*number),
        #[expect(
            clippy::cast_precision_loss,
            reason = "Python's float() rounds the same way"
        )]
        Value::Integer(number) => Ok(*number as f64),
        Value::Boolean(flag) => Ok(if *flag { 1.0 } else { 0.0 }),
        Value::String(text) => python_float_from_str(text).ok_or(UNPARSABLE_NUMBER),
        _ => Err(VALID_NUMBER),
    }
}

/// Python's `float(str)`: surrounding whitespace, a sign, `inf`, `infinity`
/// and `nan` in any case, and underscores between digits.
fn python_float_from_str(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    let characters = trimmed.chars().collect::<Vec<_>>();
    let mut cleaned = String::with_capacity(trimmed.len());
    for (index, character) in characters.iter().enumerate() {
        if *character == '_' {
            let digit_before = index
                .checked_sub(1)
                .and_then(|before| characters.get(before))
                .is_some_and(char::is_ascii_digit);
            let digit_after = characters
                .get(index.saturating_add(1))
                .is_some_and(char::is_ascii_digit);
            if !(digit_before && digit_after) {
                return None;
            }
            continue;
        }
        cleaned.push(*character);
    }
    let lowered = cleaned.to_ascii_lowercase();
    let unsigned = lowered.trim_start_matches(['+', '-']);
    if unsigned.len().saturating_add(1) < lowered.len() {
        return None;
    }
    if matches!(unsigned, "inf" | "infinity" | "nan") {
        return lowered.parse::<f64>().ok();
    }
    // Rust accepts exactly the decimal forms Python does once the special
    // words are handled, and nothing hexadecimal.
    if unsigned.is_empty()
        || !unsigned.chars().all(|character| {
            character.is_ascii_digit() || matches!(character, '.' | 'e' | '+' | '-')
        })
    {
        return None;
    }
    lowered.parse::<f64>().ok()
}

/// Pydantic's lax `bool`.
fn lax_bool(value: &Value) -> Result<bool, &'static str> {
    match value {
        Value::Boolean(flag) => Ok(*flag),
        Value::Integer(0) => Ok(false),
        Value::Integer(1) => Ok(true),
        Value::Integer(_) => Err(UNINTERPRETABLE_BOOLEAN),
        Value::Float(number) if *number == 0.0 => Ok(false),
        Value::Float(number) if *number == 1.0 => Ok(true),
        Value::String(text) => match text.to_ascii_lowercase().as_str() {
            "0" | "off" | "f" | "false" | "n" | "no" => Ok(false),
            "1" | "on" | "t" | "true" | "y" | "yes" => Ok(true),
            _ => Err(UNINTERPRETABLE_BOOLEAN),
        },
        _ => Err(VALID_BOOLEAN),
    }
}
