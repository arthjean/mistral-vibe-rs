//! The global dotenv file.
//!
//! Reference `load_dotenv_values` reads `{vibe_home}/.env` at startup so an API
//! key kept there is visible to the process, with a non-empty value already in
//! the environment winning and a FIFO accepted in place of a regular file.
//!
//! The reference mutates `os.environ`; this port cannot. `std::env::set_var` is
//! `unsafe` under edition 2024 and `unsafe_code` is forbidden workspace-wide,
//! so the file's values are resolved through [`DotenvValues::variable`] and
//! [`DotenvValues::environment`] instead. Every credential reader and the
//! `VIBE_*` environment layer go through them, which is where the reference
//! makes the values observable.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const DOTENV_FILE: &str = ".env";

/// The variables a dotenv file declares, and how they combine with the process
/// environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DotenvValues {
    values: BTreeMap<String, String>,
}

impl DotenvValues {
    /// Reads `{vibe_home}/.env`. A missing, unreadable or malformed file yields
    /// no values rather than an error: startup proceeds either way.
    #[must_use]
    pub fn global(vibe_home: &Path) -> Self {
        Self::load(&vibe_home.join(DOTENV_FILE))
    }

    /// Reads one dotenv file.
    ///
    /// The path is opened rather than stat-ed, so a FIFO standing in for the
    /// file is read like any other source.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let Ok(contents) = fs::read_to_string(path) else {
            return Self::default();
        };
        Self::parse(&contents)
    }

    /// Parses dotenv text: `KEY=value`, an optional `export` prefix, single or
    /// double quotes. A line without a separator or with an unusable key is
    /// skipped, and an empty value leaves the variable unset.
    #[must_use]
    pub fn parse(contents: &str) -> Self {
        let mut values = BTreeMap::new();
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let candidate = trimmed.strip_prefix("export ").unwrap_or(trimmed).trim();
            let Some((key, raw)) = candidate.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if !is_variable_name(key) {
                continue;
            }
            let value = unquote(raw.trim());
            if value.is_empty() {
                continue;
            }
            values.insert(key.to_owned(), value);
        }
        Self { values }
    }

    /// The value `name` resolves to: the process value when it is set and not
    /// empty, otherwise the file's.
    #[must_use]
    pub fn variable(&self, name: &str) -> Option<String> {
        resolve(std::env::var(name).ok(), self.values.get(name))
    }

    /// The process environment with the file's variables filled in where the
    /// process has none, which is the environment the reference leaves behind
    /// after loading the file.
    #[must_use]
    pub fn environment(&self) -> BTreeMap<String, String> {
        let mut merged = self.values.clone();
        for (key, value) in std::env::vars() {
            if !value.is_empty() {
                merged.insert(key, value);
            }
        }
        merged
    }
}

/// The precedence rule: an explicit non-empty process value wins over the file,
/// and an empty or absent one falls back on it. Reference `load_dotenv_values`,
/// which skips a key `os.environ` already answers with a truthy value.
pub(super) fn resolve(process: Option<String>, file: Option<&String>) -> Option<String> {
    process
        .filter(|value| !value.is_empty())
        .or_else(|| file.cloned())
}

/// Whether `name` is usable as an environment variable name. A line whose key
/// carries a space or a quote is a malformed line, not a variable.
fn is_variable_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn unquote(value: &str) -> String {
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return value[1..value.len() - 1].replace("\\'", "'");
    }
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        return value[1..value.len() - 1]
            .replace("\\n", "\n")
            .replace("\\r", "\r")
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
    }
    value.to_owned()
}
