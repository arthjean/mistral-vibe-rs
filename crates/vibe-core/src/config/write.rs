//! Writing the files behind the layers: one transactional batch, the
//! addressed patch routed over it, and the migration run once at startup.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use super::document::{
    fingerprint_optional, migrate_file, patch_target_document, persist_models_as_list,
    read_table_optional, validate_table,
};
use super::effective::require_configured_model;
use super::transaction::{
    ConfigFileLock, ConfigJournal, JournalState, PreparedWrite, cleanup_orphan_sidecars,
    cleanup_prepared, commit_prepared, ensure_private_directory, recover_transaction,
    rollback_prepared, sync_directory, write_journal,
};
use super::{
    ConfigChangeEvent, ConfigError, ConfigMutation, ConfigSnapshot, ConfigTarget, LayeredConfig,
    TRANSACTION_FILE, encode, events, merge, patch, registry, source_of,
};
use crate::redaction::redact_table;
use crate::text::sha256_hex;

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigWrite {
    pub target: ConfigTarget,
    pub expected_fingerprint: Option<String>,
    pub mutations: Vec<ConfigMutation>,
}

/// One operation of a `config/patch` request: an addressed change, and the file
/// it goes to when the client picked one.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigPatchOp {
    pub mutation: ConfigMutation,
    /// `None` routes the operation to the target the current selection resolves
    /// to, as the reference routes an operation carrying no `target_layer`.
    pub target: Option<ConfigTarget>,
}

/// What applying a patch did.
///
/// Writes are not atomic across targets: the preflight rejects the whole
/// request, and past that point each target is written independently and its
/// failure is reported rather than raised. Reference
/// `ConfigOrchestrator.apply_patch`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigPatchOutcome {
    /// The configuration as it stands after the patch.
    pub snapshot: ConfigSnapshot,
    /// One entry per target whose write failed, in target order.
    pub failures: Vec<String>,
}

impl LayeredConfig {
    pub fn batch_write(&self, writes: &[ConfigWrite]) -> Result<ConfigSnapshot, ConfigError> {
        if writes.is_empty() {
            return self.load();
        }
        let _guard = self
            .transaction_lock
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?;
        ensure_private_directory(&self.paths.vibe_home)?;
        let _file_guard = ConfigFileLock::acquire(&self.paths.vibe_home)?;
        recover_transaction(&self.paths)?;
        cleanup_orphan_sidecars(&self.paths)?;

        let mut targets = BTreeSet::new();
        let mut prepared = Vec::with_capacity(writes.len());
        let mut ephemeral = None;
        for write in writes {
            if !targets.insert(write.target) {
                return Err(ConfigError::DuplicateTarget(write.target));
            }
            if write.target == ConfigTarget::Project && !self.project_writable() {
                return Err(ConfigError::UntrustedProject);
            }
            // A source the session did not enable is never written to, which is
            // how `persist_allowed` refuses a user write when only the project
            // source is open.
            if let Some(source) = source_of(write.target)
                && !self.sources.contains(&source)
            {
                return Err(ConfigError::PersistenceDisabled(write.target));
            }
            let Some(path) = self.target_path(write.target) else {
                let persisted = self.ephemeral_document()?;
                if Some(sha256_hex(persisted.to_string().as_bytes())) != write.expected_fingerprint
                {
                    return Err(ConfigError::ConcurrentEdit {
                        target: write.target,
                    });
                }
                let table = patch_target_document(&persisted, &write.mutations)?;
                validate_table(&table)?;
                ephemeral = Some(table);
                continue;
            };
            let actual_fingerprint = fingerprint_optional(&path)?;
            if actual_fingerprint != write.expected_fingerprint {
                return Err(ConfigError::ConcurrentEdit {
                    target: write.target,
                });
            }
            let persisted = read_table_optional(&path)?;
            let mut table = patch_target_document(&persisted, &write.mutations)?;
            validate_table(&table)?;
            persist_models_as_list(&mut table, &merge::persisted_model_order(&persisted));
            let encoded = encode::encode_document(&table);
            prepared.push(PreparedWrite::new(path, encoded.into_bytes())?);
        }

        if prepared.is_empty() {
            // Nothing to journal: the write never leaves memory.
            if let Some(table) = ephemeral {
                *self
                    .ephemeral
                    .lock()
                    .map_err(|_| ConfigError::LockPoisoned)? = table;
            }
            drop(_file_guard);
            drop(_guard);
            return self.load();
        }

        let journal_path = self.paths.vibe_home.join(TRANSACTION_FILE);
        let mut journal = ConfigJournal {
            state: JournalState::Prepared,
            entries: prepared.iter().map(PreparedWrite::journal_entry).collect(),
        };
        write_journal(&journal_path, &journal)?;
        for item in &mut prepared {
            item.cleanup_on_drop = false;
        }

        if let Err(error) = commit_prepared(&prepared) {
            if let Err(rollback) = rollback_prepared(&prepared) {
                return Err(ConfigError::RollbackFailed {
                    commit: error.to_string(),
                    rollback: rollback.to_string(),
                });
            }
            fs::remove_file(&journal_path).map_err(|source| ConfigError::Io {
                path: journal_path.clone(),
                source,
            })?;
            return Err(error);
        }
        journal.state = JournalState::Committed;
        write_journal(&journal_path, &journal)?;
        cleanup_prepared(&prepared)?;
        fs::remove_file(&journal_path).map_err(|source| ConfigError::Io {
            path: journal_path,
            source,
        })?;
        sync_directory(&self.paths.vibe_home)?;
        if let Some(table) = ephemeral {
            *self
                .ephemeral
                .lock()
                .map_err(|_| ConfigError::LockPoisoned)? = table;
        }
        drop(_file_guard);
        drop(_guard);
        self.load()
    }

    /// Brings every writable configuration file forward, once per store.
    ///
    /// This is a startup step, not part of composing a configuration: the
    /// reference migrates its layers in `build_default_orchestrator` while
    /// `ConfigBuilder.build` merges untouched documents, and the committed
    /// corpus records the unmigrated merge. [`Self::load`] therefore never
    /// calls this; the binaries do, once, before their first read.
    ///
    /// A source the session did not enable, or a project file the workspace
    /// does not trust, is never rewritten, and the load then reads the
    /// unmigrated document. A write that fails is reported as a warning, left
    /// on every later snapshot, and leaves the original file intact.
    pub fn migrate_sources(&self) -> Result<Vec<String>, ConfigError> {
        let mut migrated = self
            .migrated
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?;
        if *migrated {
            return Ok(self
                .migration_warnings
                .lock()
                .map_err(|_| ConfigError::LockPoisoned)?
                .clone());
        }
        *migrated = true;
        let _guard = self
            .transaction_lock
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?;
        ensure_private_directory(&self.paths.vibe_home)?;
        let _file_guard = ConfigFileLock::acquire(&self.paths.vibe_home)?;

        let harness = self.harness_files();
        let mut files = Vec::new();
        if harness.persist_allowed() {
            files.push(self.paths.user_config());
        }
        if let Some(path) = harness.trusted_project_config() {
            files.push(path);
        }
        let mut warnings = Vec::new();
        for path in files {
            warnings.extend(migrate_file(&path)?);
        }
        self.migration_warnings
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?
            .clone_from(&warnings);
        Ok(warnings)
    }

    /// Applies addressed operations to the files that back them.
    ///
    /// The merged-configuration preflight runs first and rejects the whole
    /// request, leaving every file byte-identical. Past it the operations are
    /// grouped by target and each group is written on its own, so one target
    /// failing is reported in [`ConfigPatchOutcome::failures`] rather than
    /// undoing the group that succeeded. Reference
    /// `ConfigOrchestrator.apply_patch`.
    ///
    /// A change to the effective document is published to the subscribers
    /// [`Self::subscribe`] registered; a patch that writes the value already in
    /// place publishes nothing.
    pub fn apply_patch(
        &self,
        operations: &[ConfigPatchOp],
        reason: &str,
    ) -> Result<ConfigPatchOutcome, ConfigError> {
        let before = self.load()?;
        if operations.is_empty() {
            return Ok(ConfigPatchOutcome {
                snapshot: before,
                failures: Vec::new(),
            });
        }
        let mutations: Vec<ConfigMutation> = operations
            .iter()
            .map(|operation| operation.mutation.clone())
            .collect();
        // The preflight patches the merged document, which carries models as
        // the alias-keyed map a pointer addresses them through. Every way it can
        // fail is a rejection of the whole request, as the reference rejects on
        // a pointer error and on a validation error alike.
        let reject = |error: &dyn std::fmt::Display| ConfigError::PatchRejected(error.to_string());
        let simulated =
            patch::apply_all(&before.effective, &mutations).map_err(|error| reject(&error))?;
        validate_table(&simulated).map_err(|error| reject(&error))?;
        registry::validate_field_types(&simulated).map_err(|error| reject(&error))?;
        require_configured_model(&simulated).map_err(|error| reject(&error))?;

        let mut grouped: BTreeMap<ConfigTarget, Vec<ConfigMutation>> = BTreeMap::new();
        for operation in operations {
            let target = operation.target.unwrap_or(before.selected_target);
            // Refused before anything is written, so a revoked workspace cannot
            // leave half a patch on disk.
            if target == ConfigTarget::Project && !self.project_writable() {
                return Err(ConfigError::UntrustedProject);
            }
            grouped
                .entry(target)
                .or_default()
                .push(operation.mutation.clone());
        }

        let target_count = grouped.len();
        let mut failures = Vec::new();
        for (target, mutations) in grouped {
            let write = ConfigWrite {
                target,
                expected_fingerprint: before.fingerprints.get(&target).cloned().flatten(),
                mutations,
            };
            if let Err(error) = self.batch_write(&[write]) {
                failures.push(error.to_string());
            }
        }

        let after = self.load()?;
        // The diff runs on the documents as they are, and only the payload is
        // redacted: two different secrets both read `[redacted]`, so diffing the
        // redacted forms would hide a change between them.
        let changed_keys = events::changed_keys(
            &serde_json::to_value(&before.effective).map_err(ConfigError::Json)?,
            &serde_json::to_value(&after.effective).map_err(ConfigError::Json)?,
        );
        if failures.len() < target_count && !changed_keys.is_empty() {
            self.events.publish(&ConfigChangeEvent {
                changed_keys,
                before: redact_table(&before.effective),
                after: redact_table(&after.effective),
                reason: reason.to_owned(),
            });
        }
        Ok(ConfigPatchOutcome {
            snapshot: after,
            failures,
        })
    }

    /// The file backing `target`, or `None` for the in-memory selection.
    fn target_path(&self, target: ConfigTarget) -> Option<PathBuf> {
        match target {
            ConfigTarget::User => Some(self.paths.user_config()),
            ConfigTarget::Project => Some(self.paths.project_config()),
            ConfigTarget::Ephemeral => None,
        }
    }
}
