use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::{ConfigError, ConfigFileLock, ensure_private_directory};
use crate::atomic_file::write_atomically;

const PROXY_ENV_FILE: &str = ".env";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProxyKey {
    Http,
    Https,
    All,
    No,
    SslCertFile,
    SslCertDir,
}

impl ProxyKey {
    pub const ALL: [Self; 6] = [
        Self::Http,
        Self::Https,
        Self::All,
        Self::No,
        Self::SslCertFile,
        Self::SslCertDir,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "HTTP_PROXY",
            Self::Https => "HTTPS_PROXY",
            Self::All => "ALL_PROXY",
            Self::No => "NO_PROXY",
            Self::SslCertFile => "SSL_CERT_FILE",
            Self::SslCertDir => "SSL_CERT_DIR",
        }
    }

    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Http => "Proxy URL for HTTP requests",
            Self::Https => "Proxy URL for HTTPS requests",
            Self::All => "Proxy URL for all requests (fallback)",
            Self::No => "Comma-separated list of hosts to bypass proxy",
            Self::SslCertFile => "Path to custom SSL certificate file",
            Self::SslCertDir => "Path to directory containing SSL certificates",
        }
    }
}

impl std::fmt::Display for ProxyKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for ProxyKey {
    type Error = ProxyKeyError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.to_ascii_uppercase().as_str() {
            "HTTP_PROXY" => Ok(Self::Http),
            "HTTPS_PROXY" => Ok(Self::Https),
            "ALL_PROXY" => Ok(Self::All),
            "NO_PROXY" => Ok(Self::No),
            "SSL_CERT_FILE" => Ok(Self::SslCertFile),
            "SSL_CERT_DIR" => Ok(Self::SslCertDir),
            _ => Err(ProxyKeyError(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown proxy key `{0}`")]
pub struct ProxyKeyError(String);

#[derive(Debug, Clone)]
pub struct ProxyEnvironmentStore {
    path: PathBuf,
}

impl ProxyEnvironmentStore {
    #[must_use]
    pub fn new(vibe_home: impl Into<PathBuf>) -> Self {
        Self {
            path: vibe_home.into().join(PROXY_ENV_FILE),
        }
    }

    pub fn read(&self) -> Result<BTreeMap<ProxyKey, String>, ConfigError> {
        read_values(&self.path)
    }

    pub fn write(&self, changes: &BTreeMap<ProxyKey, Option<String>>) -> Result<(), ConfigError> {
        if changes.is_empty() {
            return Ok(());
        }
        for (key, value) in changes {
            if value
                .as_deref()
                .is_some_and(|value| value.contains(['\n', '\r', '\0']))
            {
                return Err(ConfigError::InvalidProxyValue(*key));
            }
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| ConfigError::InvalidPath(self.path.clone()))?;
        ensure_private_directory(parent)?;
        let _lock = ConfigFileLock::acquire(parent)?;
        let existing = read_contents(&self.path)?;
        let encoded = apply_changes(&existing, changes);
        write_atomically(&self.path, "env", encoded.as_bytes()).map_err(ConfigError::from)
    }
}

fn read_values(path: &Path) -> Result<BTreeMap<ProxyKey, String>, ConfigError> {
    let contents = read_contents(path)?;
    let mut values = BTreeMap::new();
    for line in contents.lines() {
        let candidate = line.trim().strip_prefix("export ").unwrap_or(line.trim());
        let Some((key, raw)) = candidate.split_once('=') else {
            continue;
        };
        if let Ok(key) = ProxyKey::try_from(key.trim()) {
            values.insert(key, unquote(raw.trim()));
        }
    }
    Ok(values)
}

fn read_contents(path: &Path) -> Result<String, ConfigError> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(source) => Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
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

fn apply_changes(existing: &str, changes: &BTreeMap<ProxyKey, Option<String>>) -> String {
    let mut written = BTreeSet::new();
    let mut lines = Vec::new();
    for line in existing.lines() {
        let candidate = line.trim().strip_prefix("export ").unwrap_or(line.trim());
        let key = candidate
            .split_once('=')
            .and_then(|(key, _)| ProxyKey::try_from(key.trim()).ok());
        let Some(key) = key.filter(|key| changes.contains_key(key)) else {
            lines.push(line.to_owned());
            continue;
        };
        if written.insert(key)
            && let Some(value) = changes.get(&key).and_then(|value| value.as_deref())
        {
            lines.push(encode(key, value));
        }
    }
    for (key, value) in changes {
        if written.insert(*key)
            && let Some(value) = value
        {
            lines.push(encode(*key, value));
        }
    }
    let mut encoded = lines.join("\n");
    if !encoded.is_empty() {
        encoded.push('\n');
    }
    encoded
}

fn encode(key: ProxyKey, value: &str) -> String {
    format!("{}='{}'", key.as_str(), value.replace('\'', "\\'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_unmanaged_values() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join(PROXY_ENV_FILE);
        fs::write(&path, "CUSTOM=value\nexport http_proxy='old'\n").expect("dotenv fixture");
        let store = ProxyEnvironmentStore::new(temporary.path());
        store
            .write(&BTreeMap::from([
                (ProxyKey::Http, Some("http://new".to_owned())),
                (ProxyKey::No, Some("localhost".to_owned())),
            ]))
            .expect("proxy update");

        assert_eq!(
            store.read().expect("proxy values"),
            BTreeMap::from([
                (ProxyKey::Http, "http://new".to_owned()),
                (ProxyKey::No, "localhost".to_owned()),
            ])
        );
        let persisted = fs::read_to_string(path).expect("persisted dotenv");
        assert!(persisted.contains("CUSTOM=value"));
        assert!(persisted.contains("HTTP_PROXY='http://new'"));
    }

    #[test]
    fn invalid_value_does_not_mutate_the_file() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join(PROXY_ENV_FILE);
        fs::write(&path, "HTTP_PROXY='old'\n").expect("dotenv fixture");
        let store = ProxyEnvironmentStore::new(temporary.path());

        let error = store
            .write(&BTreeMap::from([(
                ProxyKey::Http,
                Some("bad\nvalue".to_owned()),
            )]))
            .expect_err("invalid proxy value");

        assert!(matches!(
            error,
            ConfigError::InvalidProxyValue(ProxyKey::Http)
        ));
        assert_eq!(
            fs::read_to_string(path).expect("unchanged dotenv"),
            "HTTP_PROXY='old'\n"
        );
    }
}
