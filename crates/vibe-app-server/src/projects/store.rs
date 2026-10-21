//! The saved links between local directories and Vibe Code projects.
//!
//! Reference `VibeProjectsStore` (`vibe/core/vibe_code_project/project_store.py`):
//! one TOML document, `projects.toml` under the vibe home, holding a
//! `version` and a `projects` array whose entries are either `remote` (a Git
//! checkout and the GitHub remote it was linked under) or `local` (any
//! directory). Every operation reads the file afresh and rewrites it whole,
//! so the terminal, the editor and a session-less client that share a home
//! see each other's links at once.
//!
//! The document is kept as the reference keeps it: an entry this port cannot
//! read is carried through a rewrite untouched, keys stay in the order they
//! were written, and a file that does not parse reads as empty. A rewrite is
//! laid out the way `tomli_w` lays it out, so the file a user opens reads the
//! same whichever implementation wrote it last. The one value a rewrite here
//! cannot carry is an integer outside `i64`, which `tomllib` reads at any
//! size: a file holding one is still read, but refused a rewrite rather than
//! written back changed.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use toml::{Table, Value};
use vibe_core::atomic_file::open_private_for_rewrite;
use vibe_core::config::encode_document;
use vibe_core::platform::resolve_lenient;

use vibe_core::pycompat;

use crate::host::expand_home;

/// The file the links live in, under the vibe home (reference `PROJECTS_FILE`).
pub(crate) const PROJECTS_FILE: &str = "projects.toml";

/// Where this port kept its links before it adopted the reference's store.
const LEGACY_LINKS_FILE: &str = "vibe-code-project-links.json";

const REMOTE_KIND: &str = "remote";
const LOCAL_KIND: &str = "local";

/// One saved link (reference `ProjectLink`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectLink {
    /// A Git checkout, linked under the GitHub remote it had at the time
    /// (reference `RemoteProjectLink`).
    Remote {
        repo_root: PathBuf,
        repo_url: String,
        project_id: String,
        project_name: String,
    },
    /// Any directory, Git or not (reference `LocalProjectLink`).
    Local {
        directory_path: PathBuf,
        project_id: String,
        project_name: String,
    },
}

impl ProjectLink {
    /// The directory the link is keyed on (reference `project_link_path`).
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Remote { repo_root, .. } => repo_root,
            Self::Local { directory_path, .. } => directory_path,
        }
    }

    pub(crate) fn project_id(&self) -> &str {
        match self {
            Self::Remote { project_id, .. } | Self::Local { project_id, .. } => project_id,
        }
    }

    pub(crate) fn project_name(&self) -> &str {
        match self {
            Self::Remote { project_name, .. } | Self::Local { project_name, .. } => project_name,
        }
    }

    const fn is_remote(&self) -> bool {
        matches!(self, Self::Remote { .. })
    }

    fn to_entry(&self) -> Table {
        let fields = match self {
            Self::Remote {
                repo_root,
                repo_url,
                project_id,
                project_name,
            } => vec![
                ("kind", REMOTE_KIND.to_owned()),
                ("repo_root", normalize_path(repo_root)),
                ("repo_url", repo_url.clone()),
                ("project_id", project_id.clone()),
                ("project_name", project_name.clone()),
            ],
            Self::Local {
                directory_path,
                project_id,
                project_name,
            } => vec![
                ("kind", LOCAL_KIND.to_owned()),
                ("directory_path", normalize_path(directory_path)),
                ("project_id", project_id.clone()),
                ("project_name", project_name.clone()),
            ],
        };
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), Value::String(value)))
            .collect()
    }
}

/// A write that did not land, reported as Python reports an `OSError`
/// (`[Errno 13] Permission denied: '<path>'`), which is the message the
/// reference answers with when a store write fails under a method that does
/// not catch it.
#[derive(Debug)]
pub(crate) struct StoreError {
    path: PathBuf,
    source: io::Error,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&pycompat::os_error(&self.source, Some(&self.path)))
    }
}

impl std::error::Error for StoreError {}

/// The store at one path. It holds no state: each call reads the file.
#[derive(Debug, Clone)]
pub(crate) struct ProjectsStore {
    path: PathBuf,
}

impl ProjectsStore {
    /// The store a vibe home keeps.
    pub(crate) fn in_home(vibe_home: &Path) -> Self {
        Self {
            path: vibe_home.join(PROJECTS_FILE),
        }
    }

    /// The link saved for `repo_root`, of either kind.
    pub(crate) fn get_project_link(&self, repo_root: &Path) -> Option<ProjectLink> {
        let key = normalize_path(repo_root);
        self.list_project_links()
            .into_iter()
            .find(|link| normalize_path(link.path()) == key)
    }

    /// The link saved for `repo_root` when it is a checkout link.
    pub(crate) fn get_remote_project(&self, repo_root: &Path) -> Option<ProjectLink> {
        self.get_project_link(repo_root)
            .filter(ProjectLink::is_remote)
    }

    /// Every readable link, in the order the file holds them.
    pub(crate) fn list_project_links(&self) -> Vec<ProjectLink> {
        raw_entries(&self.read_document().table)
            .iter()
            .filter_map(parse_link)
            .collect()
    }

    /// Saves `link`, replacing the link of either kind its directory had, at
    /// the end of the list.
    pub(crate) fn upsert_project_link(&self, link: &ProjectLink) -> Result<(), StoreError> {
        let mut document = self.read_document();
        let key = normalize_path(link.path());
        let mut entries = raw_entries(&document.table)
            .into_iter()
            .filter(|entry| !entry_matches(entry, &key, false))
            .collect::<Vec<_>>();
        entries.push(link.to_entry());
        self.write_entries(&mut document, entries)
    }

    /// Drops the link of either kind saved for `repo_root`. The file is
    /// rewritten whether or not one was there.
    pub(crate) fn delete_project_link(&self, repo_root: &Path) -> Result<(), StoreError> {
        self.delete(repo_root, false)
    }

    /// Drops the checkout link saved for `repo_root`, leaving a directory link.
    pub(crate) fn delete_remote_project(&self, repo_root: &Path) -> Result<(), StoreError> {
        self.delete(repo_root, true)
    }

    fn delete(&self, repo_root: &Path, remote_only: bool) -> Result<(), StoreError> {
        let mut document = self.read_document();
        let key = normalize_path(repo_root);
        let entries = raw_entries(&document.table)
            .into_iter()
            .filter(|entry| !entry_matches(entry, &key, remote_only))
            .collect();
        self.write_entries(&mut document, entries)
    }

    /// The document on disk. A missing, unreadable or unparsable file reads
    /// as an empty store, which the next write replaces.
    fn read_document(&self) -> Document {
        match fs::read_to_string(&self.path) {
            Ok(text) => parse_document(&text).unwrap_or_else(empty_document),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.import_legacy_links().unwrap_or_else(empty_document)
            }
            Err(_) => empty_document(),
        }
    }

    /// Brings the links this port saved before it adopted `projects.toml`
    /// into the store, once: only while the store does not exist, and the
    /// old file is removed once they are written.
    fn import_legacy_links(&self) -> Option<Document> {
        let legacy = self.path.with_file_name(LEGACY_LINKS_FILE);
        let text = fs::read_to_string(&legacy).ok()?;
        let saved =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text).ok()?;
        let entries = saved
            .into_iter()
            .filter_map(|(root, link)| {
                let field = |name: &str| link.get(name)?.as_str().map(ToOwned::to_owned);
                let repo_url = field("repoUrl").filter(|url| !url.is_empty())?;
                Some(
                    ProjectLink::Remote {
                        repo_root: PathBuf::from(root),
                        repo_url,
                        project_id: field("projectId")?,
                        project_name: field("projectName")?,
                    }
                    .to_entry(),
                )
            })
            .collect();
        let mut document = empty_document();
        self.write_entries(&mut document, entries).ok()?;
        let _ = fs::remove_file(&legacy);
        Some(document)
    }

    /// Reference `_write_entries`: `version` kept or added, `projects`
    /// replaced in place, the file created owner-only and an existing one
    /// keeping its mode.
    fn write_entries(
        &self,
        document: &mut Document,
        entries: Vec<Table>,
    ) -> Result<(), StoreError> {
        if document.holds_wide_integer {
            return Err(StoreError {
                path: self.path.clone(),
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} holds an integer outside the signed 64-bit range, which a rewrite would change",
                        self.path.display()
                    ),
                ),
            });
        }
        let table = &mut document.table;
        if !table.contains_key("version") {
            table.insert("version".to_owned(), Value::Integer(1));
        }
        // An existing key keeps its position when its value is replaced.
        table.insert(
            "projects".to_owned(),
            Value::Array(entries.into_iter().map(Value::Table).collect()),
        );
        let failed = |path: &Path| {
            let path = path.to_path_buf();
            move |source| StoreError { path, source }
        };
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(failed(parent))?;
        }
        let mut file = open_private_for_rewrite(&self.path).map_err(failed(&self.path))?;
        io::Write::write_all(&mut file, encode_document(table).as_bytes())
            .map_err(failed(&self.path))
    }
}

/// The key a link is stored and looked up under (reference `_normalize_path`):
/// the path with `~` expanded, made absolute and resolved.
pub(crate) fn normalize_path(path: &Path) -> String {
    resolve_lenient(&expand_home(path))
        .to_string_lossy()
        .into_owned()
}

// --------------------------------------------------------------------------
// Entries
// --------------------------------------------------------------------------

fn empty_document() -> Document {
    Document {
        table: Table::from_iter([
            ("version".to_owned(), Value::Integer(1)),
            ("projects".to_owned(), Value::Array(Vec::new())),
        ]),
        holds_wide_integer: false,
    }
}

/// Reference `_raw_project_entries`: the tables of `projects`, anything else
/// in it dropped.
fn raw_entries(document: &Table) -> Vec<Table> {
    match document.get("projects") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::Table(entry) => Some(entry.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Reference `_parse_project_link`: a `local` entry, or else a `remote` one,
/// each with every field a string.
fn parse_link(entry: &Table) -> Option<ProjectLink> {
    let text = |key: &str| entry.get(key)?.as_str().map(ToOwned::to_owned);
    let resolve = |value: String| resolve_lenient(&expand_home(Path::new(&value)));
    if text("kind").as_deref() == Some(LOCAL_KIND) {
        return Some(ProjectLink::Local {
            directory_path: resolve(text("directory_path")?),
            project_id: text("project_id")?,
            project_name: text("project_name")?,
        });
    }
    if text("kind").as_deref() != Some(REMOTE_KIND) {
        return None;
    }
    Some(ProjectLink::Remote {
        repo_root: resolve(text("repo_root")?),
        repo_url: text("repo_url")?,
        project_id: text("project_id")?,
        project_name: text("project_name")?,
    })
}

/// Reference `_entry_matches_key`: an entry this store cannot read never
/// matches, so it survives every rewrite.
fn entry_matches(entry: &Table, key: &str, remote_only: bool) -> bool {
    parse_link(entry).is_some_and(|link| {
        (!remote_only || link.is_remote()) && normalize_path(link.path()) == key
    })
}

// --------------------------------------------------------------------------
// The document
// --------------------------------------------------------------------------

/// The store as read.
struct Document {
    table: Table,
    /// Whether the file holds an integer outside `i64`. `tomllib` reads one
    /// of any size and `tomli_w` writes it back, but a [`Value`] cannot hold
    /// it, so it reads as a `0` no link field accepts and the file is refused
    /// a rewrite instead of having that value changed.
    holds_wide_integer: bool,
}

/// Parses `text`, restoring the order every key was first written in from
/// the spans the parser records.
fn parse_document(text: &str) -> Option<Document> {
    let parsed = toml::de::DeTable::parse(text).ok()?;
    let mut holds_wide_integer = false;
    let table = table_of(parsed.get_ref(), &mut holds_wide_integer);
    Some(Document {
        table,
        holds_wide_integer,
    })
}

fn table_of(table: &toml::de::DeTable<'_>, wide: &mut bool) -> Table {
    let mut entries = table
        .iter()
        .map(|(key, value)| {
            (
                key.span().start,
                key.get_ref().to_string(),
                item_of(value.get_ref(), wide),
            )
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|(position, _, _)| *position);
    entries
        .into_iter()
        .map(|(_, key, value)| (key, value))
        .collect()
}

/// The value `value` reads as, setting `wide` for an integer outside `i64`:
/// the parser has checked its digits, so overflow is the only way it fails.
fn item_of(value: &toml::de::DeValue<'_>, wide: &mut bool) -> Value {
    use toml::de::DeValue;
    match value {
        DeValue::String(text) => Value::String(text.to_string()),
        DeValue::Integer(integer) => Value::Integer(
            i64::from_str_radix(integer.as_str(), integer.radix()).unwrap_or_else(|_| {
                *wide = true;
                0
            }),
        ),
        DeValue::Float(float) => Value::Float(parse_float(float.as_str())),
        DeValue::Boolean(flag) => Value::Boolean(*flag),
        DeValue::Datetime(datetime) => Value::Datetime(*datetime),
        DeValue::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| item_of(item.get_ref(), wide))
                .collect(),
        ),
        DeValue::Table(table) => Value::Table(table_of(table, wide)),
    }
}

fn parse_float(text: &str) -> f64 {
    let text = text.replace('_', "");
    match text.trim_start_matches('+') {
        "inf" => f64::INFINITY,
        "-inf" => f64::NEG_INFINITY,
        "nan" | "-nan" => f64::NAN,
        other => other.parse().unwrap_or_default(),
    }
}

#[cfg(test)]
mod store_tests;
