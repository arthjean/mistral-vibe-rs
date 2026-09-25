//! The persistent cache of the tool descriptors an MCP server published.
//!
//! Reference `LegacyMCPDescriptorCache` (`vibe/core/tools/mcp/descriptor_cache.py`)
//! keeps one JSON record per key under `<log root>/mcp-descriptors/legacy`,
//! named by the SHA-256 of the key, so a session that starts within a day of
//! the last discovery publishes the same tools without reaching the server.
//! The key names the server's fingerprint and the descriptor revision its
//! credential resolved to, which is what makes a rejected credential or an
//! edited entry miss rather than serve what the old one discovered. The format
//! is the reference's, so either implementation reads what the other wrote.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::RemoteTool;
use crate::auth::sign_in::UtcTimestamp;

pub const DESCRIPTOR_CACHE_FORMAT: &str = "mistral.vibe.legacy-mcp-descriptors/v1";
const NAMING_VERSION: &str = "legacy-proxy/v1";
/// Reference `descriptor_cache_ttl_s`.
pub const DESCRIPTOR_CACHE_TTL_SECONDS: f64 = 86_400.0;
const MAX_TOOLS: usize = 1_000;
const MAX_RECORD_BYTES: u64 = 2 * 1024 * 1024;
const MAX_FILES: usize = 512;
const MAX_DIRECTORY_BYTES: u64 = 64 * 1024 * 1024;

/// Reference `descriptor_cache_key`.
#[must_use]
pub fn descriptor_cache_key(server_fingerprint: &str, descriptor_revision: &str) -> String {
    super::authorization::python_json(&json!({
        "format": DESCRIPTOR_CACHE_FORMAT,
        "namingVersion": NAMING_VERSION,
        "serverFingerprint": server_fingerprint,
        "descriptorRevision": descriptor_revision,
    }))
}

/// One record, reference `LegacyDescriptorCacheRecord`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DescriptorRecord {
    pub(crate) key: String,
    pub(crate) source_name: String,
    pub(crate) discovered_at: UtcTimestamp,
    pub(crate) last_used_at: UtcTimestamp,
    pub(crate) descriptors: Vec<RemoteTool>,
}

pub struct McpDescriptorCache {
    root: PathBuf,
    ttl_seconds: f64,
    lock: Mutex<()>,
}

impl McpDescriptorCache {
    #[must_use]
    pub fn new(root: PathBuf, ttl_seconds: f64) -> Self {
        Self {
            root,
            ttl_seconds: if ttl_seconds.is_finite() && ttl_seconds >= 0.0 {
                ttl_seconds
            } else {
                0.0
            },
            lock: Mutex::new(()),
        }
    }

    /// Where a session rooted at `session_log_dir` keeps its records:
    /// reference `_legacy_session_runtime.py` resolves the session save
    /// directory and takes its parent.
    #[must_use]
    pub fn root_for(session_log_dir: &Path) -> PathBuf {
        let resolved = session_log_dir
            .canonicalize()
            .unwrap_or_else(|_| session_log_dir.to_path_buf());
        resolved
            .parent()
            .unwrap_or(&resolved)
            .join("mcp-descriptors")
            .join("legacy")
    }

    #[must_use]
    pub const fn ttl_seconds(&self) -> f64 {
        self.ttl_seconds
    }

    /// Reference `_read_sync`: a fresh record for this key and server, which
    /// the read marks as used.
    pub(crate) fn read(
        &self,
        key: &str,
        source_name: &str,
        now: UtcTimestamp,
    ) -> Option<DescriptorRecord> {
        if self.ttl_seconds == 0.0 {
            return None;
        }
        let _guard = self.lock.lock().ok()?;
        let record = self.load(key)?;
        if record.key != key || record.source_name != source_name || !self.fresh(&record, now) {
            return None;
        }
        let touched = DescriptorRecord {
            last_used_at: now,
            ..record
        };
        let _ = self.replace(&self.path(key), &encode(&touched));
        Some(touched)
    }

    /// Reference `_write_sync`.
    pub(crate) fn write(
        &self,
        key: &str,
        source_name: &str,
        descriptors: &[RemoteTool],
        now: UtcTimestamp,
    ) -> bool {
        if self.ttl_seconds == 0.0 || descriptors.len() > MAX_TOOLS {
            return false;
        }
        let record = DescriptorRecord {
            key: key.to_owned(),
            source_name: source_name.to_owned(),
            discovered_at: now,
            last_used_at: now,
            descriptors: descriptors.to_vec(),
        };
        let payload = encode(&record);
        if payload.len() as u64 > MAX_RECORD_BYTES {
            return false;
        }
        let Ok(_guard) = self.lock.lock() else {
            return false;
        };
        if self.replace(&self.path(key), &payload).is_err() {
            return false;
        }
        self.evict();
        true
    }

    /// Reference `_touch_sync`: marks a record the memory layer served.
    pub(crate) fn touch(
        &self,
        key: &str,
        source_name: &str,
        discovered_at: UtcTimestamp,
        now: UtcTimestamp,
    ) {
        if self.ttl_seconds == 0.0 {
            return;
        }
        let Ok(_guard) = self.lock.lock() else {
            return;
        };
        let Some(record) = self.load(key) else {
            return;
        };
        if record.source_name != source_name
            || record.discovered_at != discovered_at
            || !self.fresh(&record, now)
        {
            return;
        }
        let touched = DescriptorRecord {
            last_used_at: now,
            ..record
        };
        let _ = self.replace(&self.path(key), &encode(&touched));
    }

    pub(crate) fn fresh(&self, record: &DescriptorRecord, now: UtcTimestamp) -> bool {
        record.discovered_at <= now
            && record.last_used_at <= now
            && now.seconds_since(record.discovered_at) < self.ttl_seconds
    }

    fn path(&self, key: &str) -> PathBuf {
        self.root.join(format!(
            "{}.json",
            hex::encode(Sha256::digest(key.as_bytes()))
        ))
    }

    fn load(&self, key: &str) -> Option<DescriptorRecord> {
        load_path(&self.path(key))
    }

    fn replace(&self, path: &Path, payload: &[u8]) -> std::io::Result<()> {
        ensure_private_directory(&self.root)?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let temporary = self.root.join(format!(
            ".{name}.{}-{}.tmp",
            std::process::id(),
            UtcTimestamp::now().micros_since_epoch()
        ));
        let written = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            restrict(&temporary);
            std::io::Write::write_all(&mut file, payload)?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        written?;
        restrict(path);
        Ok(())
    }

    /// Reference `_evict`: drops what cannot be read, then the least recently
    /// used records until the directory is within its bounds.
    fn evict(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        let mut records = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            match load_path(&path) {
                Some(record) => records.push((record.last_used_at, path, size)),
                None => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        records.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.file_name().cmp(&right.1.file_name()))
        });
        let mut total = records.iter().map(|record| record.2).sum::<u64>();
        let mut records = records.into_iter();
        let mut remaining = records.len();
        while remaining > MAX_FILES || total > MAX_DIRECTORY_BYTES {
            let Some((_, path, size)) = records.next() else {
                break;
            };
            let _ = std::fs::remove_file(path);
            total = total.saturating_sub(size);
            remaining -= 1;
        }
    }
}

fn ensure_private_directory(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Reference `_Record.model_dump_json(by_alias=True, exclude_none=False)`.
fn encode(record: &DescriptorRecord) -> Vec<u8> {
    let descriptors = record
        .descriptors
        .iter()
        .map(|descriptor| {
            let mut entry = Map::new();
            entry.insert("name".to_owned(), json!(descriptor.name));
            entry.insert("description".to_owned(), json!(descriptor.description));
            entry.insert("inputSchema".to_owned(), descriptor.input_schema.clone());
            entry.insert(
                "outputSchema".to_owned(),
                descriptor.output_schema.clone().unwrap_or(Value::Null),
            );
            Value::Object(entry)
        })
        .collect::<Vec<_>>();
    json!({
        "format": DESCRIPTOR_CACHE_FORMAT,
        "key": record.key,
        "sourceName": record.source_name,
        "discoveredAt": pydantic_timestamp(record.discovered_at),
        "lastUsedAt": pydantic_timestamp(record.last_used_at),
        "descriptors": descriptors,
    })
    .to_string()
    .into_bytes()
}

/// Pydantic's JSON spelling of an aware UTC datetime.
fn pydantic_timestamp(timestamp: UtcTimestamp) -> String {
    let iso = timestamp.to_iso8601();
    iso.strip_suffix("+00:00")
        .map_or(iso.clone(), |head| format!("{head}Z"))
}

/// Reference `_Record` in strict mode: exactly the declared fields, of the
/// declared types, with timestamps that carry an offset.
fn load_path(path: &Path) -> Option<DescriptorRecord> {
    if std::fs::metadata(path).ok()?.len() > MAX_RECORD_BYTES {
        return None;
    }
    let value = serde_json::from_slice::<Value>(&std::fs::read(path).ok()?).ok()?;
    let object = value.as_object()?;
    const FIELDS: [&str; 6] = [
        "format",
        "key",
        "sourceName",
        "discoveredAt",
        "lastUsedAt",
        "descriptors",
    ];
    if object.len() != FIELDS.len() || !FIELDS.iter().all(|field| object.contains_key(*field)) {
        return None;
    }
    if object.get("format")?.as_str()? != DESCRIPTOR_CACHE_FORMAT {
        return None;
    }
    let key = object.get("key")?.as_str().filter(|key| !key.is_empty())?;
    let source_name = object
        .get("sourceName")?
        .as_str()
        .filter(|name| !name.is_empty())?;
    let discovered_at = aware_timestamp(object.get("discoveredAt")?.as_str()?)?;
    let last_used_at = aware_timestamp(object.get("lastUsedAt")?.as_str()?)?;
    let descriptors = object
        .get("descriptors")?
        .as_array()?
        .iter()
        .map(descriptor)
        .collect::<Option<Vec<_>>>()?;
    if descriptors.len() > MAX_TOOLS {
        return None;
    }
    Some(DescriptorRecord {
        key: key.to_owned(),
        source_name: source_name.to_owned(),
        discovered_at,
        last_used_at,
        descriptors,
    })
}

fn descriptor(value: &Value) -> Option<RemoteTool> {
    let object = value.as_object()?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "name" | "description" | "inputSchema" | "outputSchema"
        )
    }) {
        return None;
    }
    let name = object
        .get("name")?
        .as_str()
        .filter(|name| !name.is_empty())?;
    let description = match object.get("description") {
        None | Some(Value::Null) => None,
        Some(Value::String(description)) => Some(description.clone()),
        Some(_) => return None,
    };
    let input_schema = object
        .get("inputSchema")
        .filter(|schema| schema.is_object())?;
    let output_schema = match object.get("outputSchema") {
        None | Some(Value::Null) => None,
        Some(schema @ Value::Object(_)) => Some(schema.clone()),
        Some(_) => return None,
    };
    Some(RemoteTool {
        name: name.to_owned(),
        description,
        input_schema: input_schema.clone(),
        output_schema,
        annotations: Value::Null,
    })
}

/// A timestamp with an explicit offset; a naive one is refused, as the
/// reference refuses to compare it.
fn aware_timestamp(text: &str) -> Option<UtcTimestamp> {
    let has_offset = text.ends_with(['Z', 'z'])
        || text
            .get(10..)
            .is_some_and(|time| time.contains('+') || time.contains('-'));
    if !has_offset {
        return None;
    }
    UtcTimestamp::parse_iso8601(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> RemoteTool {
        RemoteTool {
            name: name.to_owned(),
            description: None,
            input_schema: json!({"type": "object"}),
            output_schema: None,
            annotations: Value::Null,
        }
    }

    #[test]
    fn a_record_round_trips_and_expires_after_its_ttl() {
        let root = tempfile::tempdir().expect("cache root");
        let cache = McpDescriptorCache::new(root.path().join("legacy"), 60.0);
        let key = descriptor_cache_key("fingerprint", "mcp-auth-descriptor:abc:0");
        let now = UtcTimestamp::now();
        assert!(cache.write(&key, "demo", &[tool("echo")], now));
        let record = cache
            .read(&key, "demo", now.plus_seconds(1.0))
            .expect("fresh");
        assert_eq!(record.descriptors, vec![tool("echo")]);
        assert!(cache.read(&key, "other", now).is_none());
        assert!(cache.read(&key, "demo", now.plus_seconds(61.0)).is_none());
    }

    #[test]
    fn the_key_is_the_reference_canonical_json() {
        assert_eq!(
            descriptor_cache_key("f", "r"),
            r#"{"descriptorRevision":"r","format":"mistral.vibe.legacy-mcp-descriptors/v1","namingVersion":"legacy-proxy/v1","serverFingerprint":"f"}"#
        );
    }
}
