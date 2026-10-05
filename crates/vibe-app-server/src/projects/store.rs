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
//! same whichever implementation wrote it last.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

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
        let text = |value: &str| Item::String(value.to_owned());
        match self {
            Self::Remote {
                repo_root,
                repo_url,
                project_id,
                project_name,
            } => vec![
                ("kind".to_owned(), text(REMOTE_KIND)),
                ("repo_root".to_owned(), text(&normalize_path(repo_root))),
                ("repo_url".to_owned(), text(repo_url)),
                ("project_id".to_owned(), text(project_id)),
                ("project_name".to_owned(), text(project_name)),
            ],
            Self::Local {
                directory_path,
                project_id,
                project_name,
            } => vec![
                ("kind".to_owned(), text(LOCAL_KIND)),
                (
                    "directory_path".to_owned(),
                    text(&normalize_path(directory_path)),
                ),
                ("project_id".to_owned(), text(project_id)),
                ("project_name".to_owned(), text(project_name)),
            ],
        }
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
        let Some(code) = self.source.raw_os_error() else {
            return write!(formatter, "{}", self.source);
        };
        let described = self.source.to_string();
        let reason = described
            .strip_suffix(&format!(" (os error {code})"))
            .unwrap_or(&described);
        write!(
            formatter,
            "[Errno {code}] {reason}: '{}'",
            self.path.display()
        )
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
        raw_entries(&self.read_document())
            .iter()
            .filter_map(parse_link)
            .collect()
    }

    /// Saves `link`, replacing the link of either kind its directory had, at
    /// the end of the list.
    pub(crate) fn upsert_project_link(&self, link: &ProjectLink) -> Result<(), StoreError> {
        let mut document = self.read_document();
        let key = normalize_path(link.path());
        let mut entries = raw_entries(&document)
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
        let entries = raw_entries(&document)
            .into_iter()
            .filter(|entry| !entry_matches(entry, &key, remote_only))
            .collect();
        self.write_entries(&mut document, entries)
    }

    /// The document on disk. A missing, unreadable or unparsable file reads
    /// as an empty store, which the next write replaces.
    fn read_document(&self) -> Table {
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
    fn import_legacy_links(&self) -> Option<Table> {
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
    fn write_entries(&self, document: &mut Table, entries: Vec<Table>) -> Result<(), StoreError> {
        if !document.iter().any(|(key, _)| key == "version") {
            document.push(("version".to_owned(), Item::Integer(1)));
        }
        let projects = Item::Array(entries.into_iter().map(Item::Table).collect());
        match document.iter_mut().find(|(key, _)| key == "projects") {
            Some((_, value)) => *value = projects,
            None => document.push(("projects".to_owned(), projects)),
        }
        let failed = |path: &Path| {
            let path = path.to_path_buf();
            move |source| StoreError { path, source }
        };
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(failed(parent))?;
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path).map_err(failed(&self.path))?;
        io::Write::write_all(&mut file, dump(document).as_bytes()).map_err(failed(&self.path))
    }
}

/// The key a link is stored and looked up under (reference `_normalize_path`):
/// the path with `~` expanded, made absolute and resolved.
pub(crate) fn normalize_path(path: &Path) -> String {
    resolve_path(&expand_home(path))
        .to_string_lossy()
        .into_owned()
}

/// Python's `Path.resolve()` without `strict`: absolute against the working
/// directory, every component that exists resolved through its symbolic
/// links, the rest appended as written with `..` taken lexically.
pub(crate) fn resolve_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                let candidate = resolved.join(name);
                resolved = fs::canonicalize(&candidate).unwrap_or(candidate);
            }
        }
    }
    resolved
}

// --------------------------------------------------------------------------
// Entries
// --------------------------------------------------------------------------

fn empty_document() -> Table {
    vec![
        ("version".to_owned(), Item::Integer(1)),
        ("projects".to_owned(), Item::Array(Vec::new())),
    ]
}

/// Reference `_raw_project_entries`: the tables of `projects`, anything else
/// in it dropped.
fn raw_entries(document: &Table) -> Vec<Table> {
    match lookup(document, "projects") {
        Some(Item::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Item::Table(entry) => Some(entry.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Reference `_parse_project_link`: a `local` entry, or else a `remote` one,
/// each with every field a string.
fn parse_link(entry: &Table) -> Option<ProjectLink> {
    let text = |key: &str| match lookup(entry, key) {
        Some(Item::String(value)) => Some(value.clone()),
        _ => None,
    };
    let resolve = |value: String| resolve_path(&expand_home(Path::new(&value)));
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

fn lookup<'a>(table: &'a Table, key: &str) -> Option<&'a Item> {
    table
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

// --------------------------------------------------------------------------
// The document
// --------------------------------------------------------------------------

/// A TOML table with its keys in document order, as Python's `tomllib`
/// returns it.
type Table = Vec<(String, Item)>;

#[derive(Debug, Clone, PartialEq)]
enum Item {
    String(String),
    Integer(i128),
    Float(f64),
    Boolean(bool),
    Datetime(toml::value::Datetime),
    Array(Vec<Item>),
    Table(Table),
}

/// Parses `text`, restoring the order every key was first written in from
/// the spans the parser records.
fn parse_document(text: &str) -> Option<Table> {
    let parsed = toml::de::DeTable::parse(text).ok()?;
    Some(table_of(parsed.get_ref()))
}

fn table_of(table: &toml::de::DeTable<'_>) -> Table {
    let mut entries = table
        .iter()
        .map(|(key, value)| {
            (
                key.span().start,
                key.get_ref().to_string(),
                item_of(value.get_ref()),
            )
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|(position, _, _)| *position);
    entries
        .into_iter()
        .map(|(_, key, value)| (key, value))
        .collect()
}

fn item_of(value: &toml::de::DeValue<'_>) -> Item {
    use toml::de::DeValue;
    match value {
        DeValue::String(text) => Item::String(text.to_string()),
        DeValue::Integer(integer) => Item::Integer(
            i128::from_str_radix(integer.as_str(), integer.radix()).unwrap_or_default(),
        ),
        DeValue::Float(float) => Item::Float(parse_float(float.as_str())),
        DeValue::Boolean(flag) => Item::Boolean(*flag),
        DeValue::Datetime(datetime) => Item::Datetime(*datetime),
        DeValue::Array(items) => {
            Item::Array(items.iter().map(|item| item_of(item.get_ref())).collect())
        }
        DeValue::Table(table) => Item::Table(table_of(table)),
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

// --------------------------------------------------------------------------
// Writing, as `tomli_w` writes
// --------------------------------------------------------------------------

/// The width past which `tomli_w` stops writing an array of tables inline.
const MAX_LINE_LENGTH: usize = 100;
const INDENT: &str = "    ";

/// `tomli_w.dumps` with its defaults: no multiline strings, a four-space
/// indent.
fn dump(document: &Table) -> String {
    let mut output = String::new();
    table_chunks(document, "", false, &mut output);
    output
}

/// `gen_table_chunks`: the table's plain values first, then its sub-tables
/// and arrays of tables, each after a blank line.
fn table_chunks(table: &Table, name: &str, inside_array: bool, output: &mut String) {
    let mut literals = Vec::new();
    let mut tables = Vec::new();
    for (key, value) in table {
        match value {
            Item::Table(child) => tables.push((key, child, false)),
            Item::Array(items) if is_array_of_tables(items) && !all_inline(items) => {
                for item in items {
                    if let Item::Table(child) = item {
                        tables.push((key, child, true));
                    }
                }
            }
            _ => literals.push((key, value)),
        }
    }
    let mut written = false;
    if inside_array || (!name.is_empty() && (!literals.is_empty() || tables.is_empty())) {
        written = true;
        if inside_array {
            let _ = writeln!(output, "[[{name}]]");
        } else {
            let _ = writeln!(output, "[{name}]");
        }
    }
    for (key, value) in &literals {
        written = true;
        let _ = writeln!(output, "{} = {}", key_part(key), literal(value, 0));
    }
    for (key, child, in_array) in tables {
        if written {
            output.push('\n');
        } else {
            written = true;
        }
        let part = key_part(key);
        let display = if name.is_empty() {
            part
        } else {
            format!("{name}.{part}")
        };
        table_chunks(child, &display, in_array, output);
    }
}

/// `is_aot`: a non-empty array holding only tables.
fn is_array_of_tables(items: &[Item]) -> bool {
    !items.is_empty() && items.iter().all(|item| matches!(item, Item::Table(_)))
}

/// `is_suitable_inline_table`, for every table of the array.
fn all_inline(items: &[Item]) -> bool {
    items.iter().all(|item| match item {
        Item::Table(table) => {
            let rendered = format!("{INDENT}{},", inline_table(table));
            rendered.chars().count() <= MAX_LINE_LENGTH && !rendered.contains('\n')
        }
        _ => false,
    })
}

fn literal(value: &Item, nesting: usize) -> String {
    match value {
        Item::Boolean(flag) => if *flag { "true" } else { "false" }.to_owned(),
        Item::Integer(integer) => integer.to_string(),
        Item::Float(float) => python_float(*float),
        Item::Datetime(datetime) => python_datetime(datetime),
        Item::String(text) => basic_string(text),
        Item::Array(items) => inline_array(items, nesting),
        Item::Table(table) => inline_table(table),
    }
}

fn inline_table(table: &Table) -> String {
    if table.is_empty() {
        return "{}".to_owned();
    }
    let fields = table
        .iter()
        .map(|(key, value)| format!("{} = {}", key_part(key), literal(value, 0)))
        .collect::<Vec<_>>();
    format!("{{ {} }}", fields.join(", "))
}

fn inline_array(items: &[Item], nesting: usize) -> String {
    if items.is_empty() {
        return "[]".to_owned();
    }
    let indent = INDENT.repeat(nesting + 1);
    let closing = INDENT.repeat(nesting);
    let lines = items
        .iter()
        .map(|item| format!("{indent}{}", literal(item, nesting + 1)))
        .collect::<Vec<_>>();
    format!("[\n{},\n{closing}]", lines.join(",\n"))
}

/// `format_key_part`: bare when every character may be, quoted otherwise.
fn key_part(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    if bare {
        key.to_owned()
    } else {
        basic_string(key)
    }
}

/// `format_string` without multiline: a basic string escaping the quote, the
/// backslash and every control character but the tab.
fn basic_string(text: &str) -> String {
    let mut output = String::with_capacity(text.len() + 2);
    output.push('"');
    for character in text.chars() {
        match character {
            '\u{8}' => output.push_str("\\b"),
            '\n' => output.push_str("\\n"),
            '\u{c}' => output.push_str("\\f"),
            '\r' => output.push_str("\\r"),
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\t' => output.push('\t'),
            control if (control as u32) < 0x20 || control == '\u{7f}' => {
                let _ = write!(output, "\\u{:04x}", control as u32);
            }
            other => output.push(other),
        }
    }
    output.push('"');
    output
}

/// `str(float)`: the shortest representation that reads back, in fixed
/// notation while the decimal exponent lies in `[-4, 16)` and in scientific
/// notation with a signed two-digit exponent outside it.
fn python_float(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .unwrap_or((scientific.as_str(), "0"));
    let exponent = exponent.parse::<i32>().unwrap_or_default();
    if (-4..16).contains(&exponent) {
        let fixed = format!("{value}");
        if fixed.contains('.') {
            fixed
        } else {
            format!("{fixed}.0")
        }
    } else {
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exponent.unsigned_abs())
    }
}

/// `str()` of the `date`, `time` or `datetime` `tomllib` reads, which is
/// what `tomli_w` writes back: a space between date and time, microseconds
/// only when there are any, and an offset as `+HH:MM`.
fn python_datetime(datetime: &toml::value::Datetime) -> String {
    let mut output = String::new();
    if let Some(date) = datetime.date {
        let _ = write!(output, "{:04}-{:02}-{:02}", date.year, date.month, date.day);
    }
    if let Some(time) = datetime.time {
        if !output.is_empty() {
            output.push(' ');
        }
        let _ = write!(
            output,
            "{:02}:{:02}:{:02}",
            time.hour,
            time.minute,
            time.second.unwrap_or_default()
        );
        let micros = time.nanosecond.unwrap_or_default() / 1_000;
        if micros != 0 {
            let _ = write!(output, ".{micros:06}");
        }
    }
    match datetime.offset {
        Some(toml::value::Offset::Z) => output.push_str("+00:00"),
        Some(toml::value::Offset::Custom { minutes }) => {
            let sign = if minutes < 0 { '-' } else { '+' };
            let minutes = minutes.unsigned_abs();
            let _ = write!(output, "{sign}{:02}:{:02}", minutes / 60, minutes % 60);
        }
        None => {}
    }
    output
}

#[cfg(test)]
mod store_tests;
