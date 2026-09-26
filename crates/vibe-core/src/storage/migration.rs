//! Bringing sessions written by an earlier format forward.
//!
//! Two earlier formats kept a session in one JSON file. The reference's
//! (`vibe/core/session/session_migration.py`) is `<prefix>_*.json` holding
//! `metadata` and `messages`, and becomes the directory named after the file's
//! stem; this port's own first format is a camel-case record. Either way the
//! migration is per-file: one unreadable entry is reported as an issue and the
//! sessions beside it still land, because a single corrupt file must not cost
//! an operator their whole history.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::Ordering;

use serde_json::Value;

use super::{
    LegacySession, MESSAGES_FILE, METADATA_FILE, MIGRATION_SUFFIX, MigrationOutcome,
    SessionMetadata, SessionStore, StorageError, TEMP_SEQUENCE, create_private_directory,
    format_iso_timestamp, session_directory_name, sync_directory, validate_session_id,
};
use crate::atomic_file::create_private_file;

impl SessionStore {
    pub(super) fn recover_migration_directories(&self) -> Result<(), StorageError> {
        let entries = fs::read_dir(&self.root).map_err(|source| StorageError::Io {
            path: self.root.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| StorageError::Io {
                path: self.root.clone(),
                source,
            })?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(".migrating-")
            {
                fs::remove_dir_all(entry.path()).map_err(|source| StorageError::Io {
                    path: entry.path(),
                    source,
                })?;
            }
        }
        Ok(())
    }

    pub(super) fn migrate_legacy_file(
        &self,
        path: &Path,
    ) -> Result<MigrationOutcome, StorageError> {
        let bytes = fs::read(path).map_err(|source| StorageError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if let Ok(Value::Object(record)) = serde_json::from_slice::<Value>(&bytes)
            && let (Some(Value::Object(metadata)), Some(Value::Array(messages))) =
                (record.get("metadata"), record.get("messages"))
        {
            return self.migrate_reference_file(path, metadata, messages);
        }
        let legacy: LegacySession =
            serde_json::from_slice(&bytes).map_err(|source| StorageError::CorruptLegacy {
                path: path.to_path_buf(),
                source,
            })?;
        validate_session_id(&legacy.session_id)?;
        if self
            .valid_metadata()?
            .iter()
            .any(|metadata| metadata.id == legacy.session_id)
        {
            return Ok(MigrationOutcome::Skipped);
        }
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory =
            session_directory_name(&self.prefix, legacy.created_at_ms, &legacy.session_id);
        let staging = self
            .root
            .join(format!(".migrating-{sequence}-{}", legacy.session_id));
        create_private_directory(&staging)?;
        let working_directory = legacy.working_directory.unwrap_or_else(|| ".".to_owned());
        let mut environment = BTreeMap::new();
        environment.insert(
            "working_directory".to_owned(),
            Some(working_directory.clone()),
        );
        let mut metadata = SessionMetadata {
            id: legacy.session_id.clone(),
            directory: staging
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned(),
            start_time: format_iso_timestamp(legacy.created_at_ms),
            end_time: Some(format_iso_timestamp(legacy.updated_at_ms)),
            git_commit: None,
            git_branch: None,
            environment,
            origin_directory: Some(working_directory.clone()),
            username: "unknown".to_owned(),
            child_sessions: Vec::new(),
            loops: Vec::new(),
            title: legacy.title,
            title_source: "manual".to_owned(),
            bumped_at: None,
            pinned_at: None,
            experiment_state: legacy.experiments,
            import_provenance: None,
            created_worktree: None,
            message_count: 0,
            last_message_fingerprint: None,
            statistics: legacy.statistics,
            tools_available: Vec::new(),
            config: legacy.config,
            agent_profile: None,
            system_prompt: None,
            extra: serde_json::Map::new(),
            created_at_ms: legacy.created_at_ms,
            updated_at_ms: legacy.updated_at_ms,
            working_directory,
            parent_session_id: legacy.parent_session_id,
        };
        create_private_file(&staging.join(MESSAGES_FILE)).map_err(|source| StorageError::Io {
            path: staging.join(MESSAGES_FILE),
            source,
        })?;
        for message in &legacy.messages {
            self.append_message_to_path(&staging.join(MESSAGES_FILE), &mut metadata, message)?;
        }
        let metadata_path = staging.join(METADATA_FILE);
        let encoded = serde_json::to_vec_pretty(&metadata).map_err(StorageError::Json)?;
        let mut metadata_file =
            create_private_file(&metadata_path).map_err(|source| StorageError::Io {
                path: metadata_path.clone(),
                source,
            })?;
        metadata_file
            .write_all(&encoded)
            .and_then(|()| metadata_file.sync_all())
            .map_err(|source| StorageError::Io {
                path: metadata_path,
                source,
            })?;
        sync_directory(&staging)?;
        let destination = self.root.join(&directory);
        fs::rename(&staging, &destination).map_err(|source| StorageError::Io {
            path: destination.clone(),
            source,
        })?;
        sync_directory(&self.root)?;
        let backup = path.with_extension(
            path.extension()
                .and_then(|extension| extension.to_str())
                .map_or_else(
                    || "legacy.bak".to_owned(),
                    |extension| format!("{extension}{MIGRATION_SUFFIX}"),
                ),
        );
        fs::rename(path, &backup).map_err(|source| StorageError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        sync_directory(&self.root)?;
        Ok(MigrationOutcome::Migrated)
    }

    /// Reference `migrate_sessions`: the file's stem names the directory, the
    /// two halves are written as they were saved, and the file goes once they
    /// are. A directory already holding the stem leaves the file for later.
    fn migrate_reference_file(
        &self,
        path: &Path,
        metadata: &serde_json::Map<String, Value>,
        messages: &[Value],
    ) -> Result<MigrationOutcome, StorageError> {
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            return Ok(MigrationOutcome::Skipped);
        };
        if !stem.starts_with(&format!("{}_", self.prefix)) {
            return Ok(MigrationOutcome::Skipped);
        }
        let destination = self.root.join(stem);
        if destination.exists() {
            return Ok(MigrationOutcome::Skipped);
        }
        create_private_directory(&destination)?;
        let encoded = serde_json::to_vec_pretty(&Value::Object(metadata.clone()))
            .map_err(StorageError::Json)?;
        write_new_file(&destination.join(METADATA_FILE), &encoded)?;
        let mut log = Vec::new();
        for message in messages {
            log.extend(serde_json::to_vec(message).map_err(StorageError::Json)?);
            log.push(b'\n');
        }
        write_new_file(&destination.join(MESSAGES_FILE), &log)?;
        sync_directory(&destination)?;
        fs::remove_file(path).map_err(|source| StorageError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        sync_directory(&self.root)?;
        Ok(MigrationOutcome::Migrated)
    }
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let mut file = create_private_file(path).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| StorageError::Io {
            path: path.to_path_buf(),
            source,
        })
}
