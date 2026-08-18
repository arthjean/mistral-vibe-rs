//! Reading, checking and rewriting one configuration file.
//!
//! Everything here answers about a single document rather than about the stack
//! of them: whether the bytes on disk parse, what they fingerprint to, whether
//! the values they carry are acceptable, and what the file should look like
//! once a migration or a patch has run. The layering above composes documents;
//! this is what a document is worth on its own.
//!
//! [`MAX_CONFIG_BYTES`] bounds every read. A configuration file is operator
//! input, and a file too large to hold has to cost a diagnostic rather than the
//! process.

use std::fs::File;
use std::io::Read as _;
use std::path::Path;

use sha2::{Digest, Sha256};
use toml::Table;
use url::Url;

use super::{ConfigError, ConfigMutation, MAX_CONFIG_BYTES, merge, migration, patch};
use crate::atomic_file::write_atomically;
use crate::text::hex_encode;

pub(super) fn read_table_optional(path: &Path) -> Result<Table, ConfigError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Table::new()),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let metadata = file.metadata().map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge(path.to_path_buf()));
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if contents.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge(path.to_path_buf()));
    }
    contents
        .parse::<Table>()
        .map_err(|source| ConfigError::InvalidToml {
            path: path.to_path_buf(),
            source,
        })
}

pub(super) fn fingerprint_optional(path: &Path) -> Result<Option<String>, ConfigError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge(path.to_path_buf()));
    }
    Ok(Some(hex_digest(&bytes)))
}

pub(super) fn hex_digest(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

/// Applies `mutations` to one target's persisted document.
///
/// `models` is normalized to the alias-keyed map first, so a pointer addressing
/// one model resolves against the same shape the merged document exposes; the
/// caller writes it back as the persisted list. Reference `_base.py` normalizes
/// on layer read and serializes on write for exactly this reason.
pub(super) fn patch_target_document(
    persisted: &Table,
    mutations: &[ConfigMutation],
) -> Result<Table, ConfigError> {
    let mut document = persisted.clone();
    if let Some(models) = document.get(merge::MODELS_FIELD) {
        let normalized = merge::normalize_models(models)?;
        document.insert(merge::MODELS_FIELD.to_owned(), normalized);
    }
    Ok(patch::apply_all(&document, mutations)?)
}

pub(super) fn validate_table(table: &Table) -> Result<(), ConfigError> {
    validate_urls(table, &mut Vec::new())
}

/// Migrates one configuration file in place, reporting the warning a failed
/// write produces.
///
/// A file that needs no migration is not rewritten, so an untouched
/// configuration keeps its formatting and its modification time.
pub(super) fn migrate_file(path: &Path) -> Result<Option<String>, ConfigError> {
    let persisted = read_table_optional(path)?;
    if persisted.is_empty() {
        return Ok(None);
    }
    let mut document = persisted.clone();
    if let Some(models) = document.get(merge::MODELS_FIELD) {
        let normalized = merge::normalize_models(models)?;
        document.insert(merge::MODELS_FIELD.to_owned(), normalized);
    }
    if !migration::migrate_document(&mut document) {
        return Ok(None);
    }
    persist_models_as_list(&mut document, &merge::persisted_model_order(&persisted));
    let encoded = toml::to_string_pretty(&document).map_err(ConfigError::Serialize)?;
    match write_atomically(path, "config", encoded.as_bytes()) {
        Ok(()) => Ok(None),
        // The original file is untouched: the encoded document never replaced
        // it, so the operator keeps a readable configuration.
        Err(error) => Ok(Some(format!(
            "Configuration migration skipped for `{}`: {}",
            path.display(),
            ConfigError::from(error)
        ))),
    }
}

/// Writes an alias-keyed model map back as the persisted `[[models]]` list, so
/// a client that patched the read form does not leave a document the reference
/// extensions cannot parse. Reference `_canonical_toml_document`.
///
/// `persisted_order` is the order the file being rewritten already listed its
/// models in, which the entries keep; anything the patch added follows.
///
/// The reference also drops null-valued fields on the way out; TOML has no
/// null, so a [`Table`] cannot carry one and nothing has to be dropped here.
pub(super) fn persist_models_as_list(table: &mut Table, persisted_order: &[String]) {
    if let Some(models) = table.get(merge::MODELS_FIELD)
        && models.is_table()
    {
        let serialized = merge::serialize_models(models, persisted_order);
        table.insert(merge::MODELS_FIELD.to_owned(), serialized);
    }
}

pub(super) fn validate_urls(table: &Table, path: &mut Vec<String>) -> Result<(), ConfigError> {
    for (key, value) in table {
        path.push(key.clone());
        if is_proxy_key(key)
            && let Some(raw) = value.as_str()
        {
            let parsed = Url::parse(raw).map_err(|_| ConfigError::InvalidSensitiveUrl {
                path: path.join("."),
            })?;
            if !parsed.username().is_empty() || parsed.password().is_some() {
                return Err(ConfigError::SensitiveUrlCredentials {
                    path: path.join("."),
                });
            }
        }
        if let Some(nested) = value.as_table() {
            validate_urls(nested, path)?;
        }
        path.pop();
    }
    Ok(())
}

pub(super) fn is_proxy_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "proxy" | "http_proxy" | "https_proxy" | "proxy_url"
    )
}
