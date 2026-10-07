//! What a plugin catalog publishes in place of a value that may be a secret.
//!
//! Reference `vibe/core/plugins/_redaction.py`. Positions that may carry a
//! credential (environment values, header values, argument values, a URL's
//! user information, query and fragment) are published by name or rewritten,
//! never verbatim.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

pub const REDACTED: &str = "<redacted>";
pub const PLUGIN_PLACEHOLDER: &str = "<plugin>";

#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static OPTION_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^--?[^\s=]+").expect("option pattern compiles"));
#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)[a-z][a-z0-9+.\-]*://[^\s"'<>]+"#).expect("url pattern compiles")
});

/// The names of a mapping, sorted. Reference `redact_names`.
#[must_use]
pub fn redact_names<'a>(values: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut names: Vec<String> = values.into_iter().cloned().collect();
    names.sort();
    names
}

/// The executable and every option name survive; every operand and option
/// value is replaced. Reference `redact_argv`.
#[must_use]
pub fn redact_argv(argv: &[String]) -> Vec<String> {
    let Some((first, rest)) = argv.split_first() else {
        return Vec::new();
    };
    let mut redacted = vec![first.clone()];
    for argument in rest {
        if !OPTION_NAME.is_match(argument) {
            redacted.push(REDACTED.to_owned());
        } else if let Some((name, _)) = argument.split_once('=') {
            redacted.push(format!("{name}={REDACTED}"));
        } else {
            redacted.push(argument.clone());
        }
    }
    redacted
}

/// The parts [`redact_argv`] replaces: operands and option values.
#[must_use]
pub fn argv_values(argv: &[String]) -> Vec<String> {
    argv.iter()
        .skip(1)
        .filter_map(|argument| {
            if !OPTION_NAME.is_match(argument) {
                Some(argument.clone())
            } else {
                argument.split_once('=').map(|(_, value)| value.to_owned())
            }
        })
        .collect()
}

/// A URL without its user information, query and fragment. Reference
/// `redact_url`, which rebuilds the network location from the host and port
/// alone.
#[must_use]
pub fn redact_url(url: &str) -> String {
    let parts = crate::pyurl::PyUrl::split(url);
    let host = parts.hostname().unwrap_or_default();
    // A port Python cannot read raises there; the host alone is kept here.
    let netloc = match parts.port() {
        Ok(Some(port)) => format!("{host}:{port}"),
        _ => host,
    };
    crate::pyurl::PyUrl {
        scheme: parts.scheme.clone(),
        netloc,
        path: parts.path.clone(),
        query: String::new(),
        fragment: String::new(),
    }
    .unsplit()
}

/// Free text with every URL rewritten and every known secret removed,
/// longest first. Reference `redact_failure`.
#[must_use]
pub fn redact_failure<'a>(message: &str, secrets: impl IntoIterator<Item = &'a str>) -> String {
    let mut redacted = URL
        .replace_all(message, |captures: &regex::Captures<'_>| {
            redact_url(&captures[0])
        })
        .into_owned();
    let mut unique: Vec<&str> = secrets
        .into_iter()
        .filter(|secret| !secret.is_empty())
        .collect();
    unique.sort_unstable();
    unique.dedup();
    unique.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    for secret in unique {
        redacted = redacted.replace(secret, REDACTED);
    }
    redacted
}

/// Every value of a mapping replaced. Reference `redact_values`.
#[must_use]
pub fn redact_values(values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    values
        .keys()
        .map(|name| (name.clone(), REDACTED.to_owned()))
        .collect()
}
