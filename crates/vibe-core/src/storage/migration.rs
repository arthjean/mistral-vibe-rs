//! Bringing sessions written by an earlier format forward.
//!
//! A legacy session is one JSON file; the current format is a directory holding
//! metadata and an append-only message log. The migration is versioned,
//! retryable and per-file: one unreadable entry is reported as an issue and the
//! sessions beside it still land, because a single corrupt file must not cost
//! an operator their whole history.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::Ordering;

use super::{
    CURRENT_FORMAT_VERSION, LegacySession, MESSAGES_FILE, METADATA_FILE, MIGRATION_SUFFIX,
    MigrationOutcome, SessionMetadata, SessionStore, StorageError, TEMP_SEQUENCE,
    create_private_directory, format_iso_timestamp, session_directory_name, sync_directory,
    validate_session_id,
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
        let directory = session_directory_name(legacy.created_at_ms, &legacy.session_id);
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
            format_version: CURRENT_FORMAT_VERSION,
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
            username: "unknown".to_owned(),
            child_sessions: Vec::new(),
            loops: Vec::new(),
            title: legacy.title,
            title_source: "legacy".to_owned(),
            experiment_state: legacy.experiments,
            message_count: 0,
            last_message_fingerprint: None,
            statistics: legacy.statistics,
            tools_available: Vec::new(),
            config: legacy.config,
            agent_profile: None,
            system_prompt: None,
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
        let mut encoded = serde_json::to_vec_pretty(&metadata).map_err(StorageError::Json)?;
        encoded.push(b'\n');
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
}
