//! The redacted view of the active provider that `providerAuth/read` answers.
//!
//! Reference `vibe/app_server/_provider_auth.py`: a read-only snapshot for
//! `/status` that sends no provider request and carries no credential. The API
//! base is configuration text nobody vetted, so it is rebuilt from its scheme,
//! host, port and path, and the credential the provider would send is
//! redacted from what is left before it is shown.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use toml::Value as TomlValue;
use vibe_core::config::{ConfigSnapshot, DotenvValues};
use vibe_core::pyurl::PyUrl;

use super::WorkspaceService;

/// What a redacted credential reads as.
const REDACTED: &str = "[redacted]";

impl WorkspaceService {
    /// Reference `read_provider_auth`: the view for the configuration as it
    /// stands, with the credential read from the environment the provider
    /// would read it from. A configuration whose active model or provider
    /// does not resolve has no view to give.
    pub fn provider_auth(&self) -> Result<Value, String> {
        let snapshot = self
            .config
            .load()
            .map_err(|error| format!("The configuration could not be read: {error}"))?;
        let environ = DotenvValues::global(&self.paths.vibe_home).environment();
        provider_auth_view(&snapshot, &environ)
            .ok_or_else(|| "The active model's provider could not be resolved".to_owned())
    }
}

/// Reference `build_provider_auth_view`, over the merged environment.
///
/// Only a credential the environment holds is redacted: a key kept in the
/// keyring alone is not read, since reading it can block, as the reference
/// leaves it.
pub(crate) fn provider_auth_view(
    snapshot: &ConfigSnapshot,
    environ: &BTreeMap<String, String>,
) -> Option<Value> {
    let alias = snapshot.config_view()["activeModel"]["alias"]
        .as_str()
        .map(ToOwned::to_owned)?;
    let provider = snapshot.active_provider()?;
    let text = |key: &str| {
        provider
            .get(key)
            .and_then(TomlValue::as_str)
            .unwrap_or_default()
    };
    let secrets = Some(text("api_key_env_var"))
        .filter(|variable| !variable.is_empty())
        .and_then(|variable| environ.get(variable))
        .filter(|value| !value.is_empty())
        .map(String::as_str)
        .into_iter()
        .collect::<Vec<_>>();
    let display_name = snapshot
        .active_model_display_name()
        .filter(|name| !name.is_empty())
        .unwrap_or(alias);
    Some(json!({
        "modelDisplayName": display_name,
        "providerName": text("name"),
        "apiBase": sanitize_api_base(text("api_base"), &secrets),
    }))
}

/// Reference `sanitize_api_base`: the HTTP or HTTPS scheme, the host, an
/// optional port and the path, with every known credential redacted, or
/// `None` when that cannot be shown safely.
///
/// User information, query and fragment are dropped by the rebuild, which is
/// reference `display_url`: the host lowercased and bracketed again when it
/// holds a colon, the port written as the number it parses to. A host
/// `urlsplit` refuses or a port it cannot read makes the value unshowable.
pub(crate) fn sanitize_api_base(api_base: &str, secrets: &[&str]) -> Option<String> {
    let parts = PyUrl::try_split(api_base).ok()?;
    let host = parts.hostname();
    let port = parts.port().ok()?;
    let mut netloc = host.clone().unwrap_or_default();
    if netloc.contains(':') {
        netloc = format!("[{netloc}]");
    }
    if let Some(port) = port {
        netloc = format!("{netloc}:{port}");
    }
    let destination = PyUrl {
        scheme: parts.scheme.clone(),
        netloc,
        path: parts.path.clone(),
        query: String::new(),
        fragment: String::new(),
    }
    .unsplit();
    if !matches!(parts.scheme.as_str(), "http" | "https") || host.is_none() {
        return None;
    }
    let destination = redact_secrets(&destination, secrets);
    (!destination.chars().any(char::is_control)).then_some(destination)
}

/// Reference `_redact_secrets`: each secret, longest first so one holding
/// another redacts whole, matched without regard to case in its raw form or
/// with any of its characters percent-encoded, and a space also as `+`.
fn redact_secrets(value: &str, secrets: &[&str]) -> String {
    let mut ordered = secrets
        .iter()
        .copied()
        .filter(|secret| !secret.is_empty())
        .collect::<Vec<_>>();
    // A stable sort, as Python's `sorted` is, so equal lengths keep their
    // order.
    ordered.sort_by_key(|secret| std::cmp::Reverse(secret.chars().count()));
    let mut redacted = value.to_owned();
    for secret in ordered {
        let Ok(pattern) = regex::Regex::new(&format!("(?i){}", secret_pattern(secret))) else {
            continue;
        };
        redacted = pattern.replace_all(&redacted, REDACTED).into_owned();
    }
    redacted
}

/// Reference `_secret_pattern`: every character as itself or as its UTF-8
/// percent encoding, a space also as `+`.
fn secret_pattern(secret: &str) -> String {
    secret
        .chars()
        .map(|character| {
            let mut buffer = [0; 4];
            let encoded = character
                .encode_utf8(&mut buffer)
                .bytes()
                .map(|byte| format!("%{byte:02x}"))
                .collect::<String>();
            let mut alternatives = vec![regex::escape(character.encode_utf8(&mut buffer)), encoded];
            if character == ' ' {
                alternatives.push(r"\+".to_owned());
            }
            format!("(?:{})", alternatives.join("|"))
        })
        .collect()
}

#[cfg(test)]
mod provider_auth_tests;
