//! The shared `cache.toml` under one Vibe home.
//!
//! Reference `FileSystemCacheStore` (`vibe/utils/cache_store.py`): every
//! subsystem that remembers something between launches owns one top-level
//! section of the same file, so a write reads the whole document, merges into
//! its own section, and writes the document back.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::atomic_file::{FileMode, write_atomically};
use crate::observability::{self, LogLevel};
use crate::sync::lock;

/// Reference `_FILE_LOCK`: the read-modify-write of one section must not
/// interleave with another's, or the later write drops the earlier section.
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// The `cache.toml` of one Vibe home, read and written one section at a time.
#[derive(Debug, Clone)]
pub struct CacheStore {
    path: PathBuf,
}

impl CacheStore {
    #[must_use]
    pub fn new(vibe_home: &Path) -> Self {
        Self {
            path: vibe_home.join("cache.toml"),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reference `read_section`: the section's table, or an empty one when
    /// the file cannot be read, does not parse, lacks the section, or holds
    /// something other than a table there.
    #[must_use]
    pub fn read_section(&self, section: &str) -> toml::Table {
        let _guard = lock(&FILE_LOCK);
        match self
            .read_document()
            .and_then(|mut document| document.remove(section))
        {
            Some(toml::Value::Table(table)) => table,
            _ => toml::Table::new(),
        }
    }

    /// Reference `write_section`, which updates the section in place: a key
    /// `data` does not carry keeps whatever the file held, and only a section
    /// that is not a table is replaced outright. A failed write is logged at
    /// debug level and otherwise ignored, as the reference ignores it.
    pub fn write_section(&self, section: &str, data: toml::Table) {
        let _guard = lock(&FILE_LOCK);
        let mut document = self.read_document().unwrap_or_default();
        let slot = document
            .entry(section)
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !slot.is_table() {
            *slot = toml::Value::Table(toml::Table::new());
        }
        if let toml::Value::Table(merged) = slot {
            merged.extend(data);
        }
        let written = toml::to_string_pretty(&toml::Value::Table(document))
            .map_err(|error| error.to_string())
            .and_then(|encoded| {
                if let Some(parent) = self.path.parent() {
                    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                write_atomically(
                    &self.path,
                    "cache.toml",
                    encoded.as_bytes(),
                    FileMode::Private,
                )
                .map_err(|error| format!("{error:?}"))
            });
        if let Err(error) = written {
            observability::log(
                LogLevel::Debug,
                &format!(
                    "Failed to write cache file {}\n{error}",
                    self.path.display()
                ),
            );
        }
    }

    /// Reference `_read_cache`: the whole file, of any size, or nothing when it
    /// cannot be read or does not parse.
    fn read_document(&self) -> Option<toml::Table> {
        toml::from_str(&fs::read_to_string(&self.path).ok()?).ok()
    }
}

#[cfg(test)]
mod cache_store_tests;
