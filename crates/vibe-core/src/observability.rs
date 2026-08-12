//! The log file this build writes and the reader that pages it back.
//!
//! Reference `vibe/observability/logging.py` writes one line per record to
//! `$VIBE_HOME/logs/vibe.log` and `vibe/core/log_reader.py` reads it back from
//! the end, newest first. Before this module the product wrote nothing to disk:
//! a failure before the app server attached left no trace, and
//! `diagnostics/logs/read` answered from a process-local ring buffer, so `ppid`
//! and `pid` were zeros and nothing another process wrote was readable.
//!
//! Three rules run through it. A line is single: a message carrying a newline
//! is encoded rather than wrapped, and [`decode_log_message`] restores it. A
//! record never fails the caller: an unwritable file is dropped, which is why
//! [`log`] returns nothing. And a line that does not parse is skipped rather
//! than raised on, so one hand-edited line cannot empty a page.
//!
//! The pattern below is written for this port rather than copied from the
//! reference, which `NOTICE` requires. It accepts the same language, which is
//! what the `logParse` corpus family measures line for line; the digest the
//! corpus records for the reference's own expression is the one comparison in
//! the family that cannot conform, and the replay's ledger names it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::auth::UtcTimestamp;

#[cfg(test)]
mod observability_tests;

// --------------------------------------------------------------------------
// The constants the reference declares
// --------------------------------------------------------------------------

/// Reference `logging.getLogger("vibe")`: the one logger the product writes
/// through.
pub const LOG_LOGGER_NAME: &str = "vibe";
/// Reference `LOG_FILE`, relative to the vibe home.
pub const LOG_RELATIVE_FILE: &str = "logs/vibe.log";
/// The directory of [`LOG_RELATIVE_FILE`], joined component by component so a
/// Windows path never carries the forward slash the corpus records.
const LOG_DIRECTORY: &str = "logs";
/// The file name of [`LOG_RELATIVE_FILE`].
const LOG_FILE_NAME: &str = "vibe.log";
/// Reference `max_bytes` default, 10 MiB.
pub const LOG_DEFAULT_MAX_BYTES: i64 = 10 * 1024 * 1024;
/// Reference `backupCount=0`: a rotation keeps nothing behind.
pub const LOG_BACKUP_COUNT: u64 = 0;
/// The fields of a line, in the order the formatter writes them.
pub const LOG_FIELD_ORDER: [&str; 5] = ["timestamp", "ppid", "pid", "level", "message"];
/// What separates two fields of a line.
pub const LOG_FIELD_SEPARATOR: &str = " ";
/// Reference `LOG_POLL_INTERVAL`, the interval a console re-reads at.
pub const LOG_POLL_INTERVAL_SECONDS: f64 = 0.5;
/// Reference `chunk_size`: how much of the tail is read at a time.
pub const LOG_READ_CHUNK_SIZE: usize = 8_192;
/// Reference `get_logs(limit=100)`.
pub const LOG_DEFAULT_PAGE_LIMIT: usize = 100;
/// The environment variable naming the level, read at initialization.
pub const LOG_LEVEL_VARIABLE: &str = "LOG_LEVEL";
/// The environment variable that forces `DEBUG`, and the only value that does.
pub const DEBUG_MODE_VARIABLE: &str = "DEBUG_MODE";
/// The value [`DEBUG_MODE_VARIABLE`] is compared against, exactly.
pub const DEBUG_MODE_ENABLED: &str = "true";
/// The environment variable naming the rotation ceiling.
pub const LOG_MAX_BYTES_VARIABLE: &str = "LOG_MAX_BYTES";

/// The line shape [`format_log_line`] writes, read back by [`parse_log_line`].
///
/// Reference `DEFAULT_LOG_PATTERN` accepts the same language: an ISO 8601
/// timestamp with fractional seconds and an optional offset, two process
/// identifiers, one of the five level names, and a non-empty message. The five
/// captures are what a parsed entry is built from.
pub const LOG_LINE_PATTERN: &str = concat!(
    r"^(?<timestamp>\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+(?:[+-]\d{2}:\d{2})?)\s+",
    r"(?<ppid>\d+)\s+(?<pid>\d+)\s+",
    r"(?<level>DEBUG|INFO|WARNING|ERROR|CRITICAL)\s+",
    r"(?<message>.+)$",
);

/// The compiled [`LOG_LINE_PATTERN`], built once for the process.
fn log_line_pattern() -> Option<&'static Regex> {
    static PATTERN: OnceLock<Option<Regex>> = OnceLock::new();
    PATTERN
        .get_or_init(|| Regex::new(LOG_LINE_PATTERN).ok())
        .as_ref()
}

/// How many captures [`LOG_LINE_PATTERN`] declares, which is what a reader
/// builds an entry from. Zero when the expression does not compile, which no
/// build reaches and no caller has to handle twice.
#[must_use]
pub fn log_pattern_groups() -> usize {
    log_line_pattern().map_or(0, |pattern| pattern.captures_len().saturating_sub(1))
}

// --------------------------------------------------------------------------
// Levels
// --------------------------------------------------------------------------

/// The five levels the reference publishes, ordered by severity so a sink can
/// compare against its own threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
    Critical,
}

impl LogLevel {
    /// Every level, in severity order.
    pub const ALL: [Self; 5] = [
        Self::Debug,
        Self::Info,
        Self::Warning,
        Self::Error,
        Self::Critical,
    ];
    /// What an unset or unrecognized `LOG_LEVEL` resolves to.
    pub const DEFAULT: Self = Self::Warning;

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warning => "WARNING",
            Self::Error => "ERROR",
            Self::Critical => "CRITICAL",
        }
    }

    /// The level a name selects, case-insensitively as the reference upper-cases
    /// before looking one up. An unknown name selects nothing, which is what
    /// makes [`LogLevel::DEFAULT`] the fallback rather than an error.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let upper = value.to_ascii_uppercase();
        Self::ALL.into_iter().find(|level| level.as_str() == upper)
    }
}

impl Default for LogLevel {
    fn default() -> Self {
        Self::DEFAULT
    }
}

// --------------------------------------------------------------------------
// The line
// --------------------------------------------------------------------------

/// Reference `encode_log_message`: a backslash doubles and a newline becomes
/// `\n`, so one record is always one line.
#[must_use]
pub fn encode_log_message(message: &str) -> String {
    message.replace('\\', "\\\\").replace('\n', "\\n")
}

/// Reference `decode_log_message`: `\n` restores a newline and any other
/// escaped character restores itself, which is what makes the round trip exact
/// for a message that already carried backslashes.
#[must_use]
pub fn decode_log_message(encoded: &str) -> String {
    let mut decoded = String::with_capacity(encoded.len());
    let mut characters = encoded.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => decoded.push('\n'),
            Some(escaped) => decoded.push(escaped),
            // A trailing backslash escapes nothing and stays itself.
            None => decoded.push('\\'),
        }
    }
    decoded
}

/// Reference `StructuredLogFormatter.format`: the five fields in order, then
/// the exception text after one more separator when a record carries one.
#[must_use]
pub fn format_log_line(
    timestamp: UtcTimestamp,
    ppid: u32,
    pid: u32,
    level: LogLevel,
    message: &str,
    exception: Option<&str>,
) -> String {
    let mut line = format!(
        "{}{LOG_FIELD_SEPARATOR}{ppid}{LOG_FIELD_SEPARATOR}{pid}{LOG_FIELD_SEPARATOR}{}{LOG_FIELD_SEPARATOR}{}",
        timestamp.to_iso8601(),
        level.as_str(),
        encode_log_message(message)
    );
    if let Some(exception) = exception {
        line.push_str(LOG_FIELD_SEPARATOR);
        line.push_str(&encode_log_message(exception));
    }
    line
}

/// This process and the one that started it, the pair every line carries.
///
/// Windows publishes no parent identifier through the standard library and this
/// workspace forbids `unsafe_code`, so the parent reads as zero there.
#[must_use]
pub fn process_identifiers() -> (u32, u32) {
    #[cfg(unix)]
    let parent = std::os::unix::process::parent_id();
    #[cfg(not(unix))]
    let parent = 0;
    (parent, std::process::id())
}

/// The identity of one entry, which is what a console dedupes a re-read page by.
///
/// Reference digests the raw line with SHA-1. Nothing reads the value as a
/// SHA-1, and this workspace already carries SHA-256 for the parity corpora, so
/// the identity is the same function of the same input under a stronger digest.
#[must_use]
pub fn entry_identity(raw_line: &str) -> String {
    let digest = Sha256::digest(raw_line.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

// --------------------------------------------------------------------------
// The resolved settings
// --------------------------------------------------------------------------

/// What the environment decides about a log file: how much it says, and how
/// large it grows before it rotates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogSettings {
    pub level: LogLevel,
    /// Reference `maxBytes`. Zero or less disables rotation, as the handler it
    /// configures does.
    pub max_bytes: i64,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            level: LogLevel::DEFAULT,
            max_bytes: LOG_DEFAULT_MAX_BYTES,
        }
    }
}

/// The one way resolving settings fails: a rotation ceiling that is not a
/// number. Reference lets `int` raise, which fails initialization rather than
/// falling back on the default.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{LOG_MAX_BYTES_VARIABLE} is not a whole number of bytes: `{value}`")]
pub struct LogSettingsError {
    pub value: String,
}

impl LogSettings {
    /// Reference `init_file_logging`'s resolution: `DEBUG_MODE` set to exactly
    /// `true` forces `DEBUG` and nothing else does, an unknown `LOG_LEVEL` falls
    /// back on `WARNING`, and `LOG_MAX_BYTES` is read as a whole number or not
    /// at all.
    pub fn resolve(read: &dyn Fn(&str) -> Option<String>) -> Result<Self, LogSettingsError> {
        let max_bytes = match read(LOG_MAX_BYTES_VARIABLE) {
            None => LOG_DEFAULT_MAX_BYTES,
            Some(value) => value
                .trim()
                .parse::<i64>()
                .map_err(|_| LogSettingsError { value })?,
        };
        let level = if read(DEBUG_MODE_VARIABLE).as_deref() == Some(DEBUG_MODE_ENABLED) {
            LogLevel::Debug
        } else {
            read(LOG_LEVEL_VARIABLE)
                .as_deref()
                .and_then(LogLevel::parse)
                .unwrap_or_default()
        };
        Ok(Self { level, max_bytes })
    }

    /// The settings this process's environment asks for.
    pub fn from_environment() -> Result<Self, LogSettingsError> {
        Self::resolve(&|name| std::env::var(name).ok())
    }
}

// --------------------------------------------------------------------------
// The file
// --------------------------------------------------------------------------

/// One log file and what the environment decided about it.
///
/// The handle is opened per record rather than held, so two processes writing
/// the same file append rather than overwrite each other, which is the whole
/// point of a shared file: the app server and the terminal client are two
/// processes and an operator reads one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLog {
    path: PathBuf,
    settings: LogSettings,
}

impl FileLog {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, settings: LogSettings) -> Self {
        Self {
            path: path.into(),
            settings,
        }
    }

    /// Reference `LOG_FILE`: `logs/vibe.log` under the vibe home.
    #[must_use]
    pub fn in_home(vibe_home: &Path, settings: LogSettings) -> Self {
        Self::new(vibe_home.join(LOG_DIRECTORY).join(LOG_FILE_NAME), settings)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn settings(&self) -> LogSettings {
        self.settings
    }

    /// A reader over this file.
    #[must_use]
    pub fn reader(&self) -> LogReader {
        LogReader::new(self.path.clone())
    }

    /// Appends one record, rotating first when the line would carry the file
    /// past its ceiling.
    ///
    /// Reference reopens the same file in append mode when it rotates with no
    /// backup, so upstream a log grows without bound once it passes the
    /// ceiling. This port keeps the ceiling: nothing is kept behind, which is
    /// what `backupCount=0` says, and an operator's home does not fill up.
    pub fn write(
        &self,
        level: LogLevel,
        message: &str,
        exception: Option<&str>,
    ) -> Result<(), io::Error> {
        if level < self.settings.level {
            return Ok(());
        }
        let (ppid, pid) = process_identifiers();
        let line = format_log_line(UtcTimestamp::now(), ppid, pid, level, message, exception);
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = if self.rotates_before(&line) {
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)?
        } else {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?
        };
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")
    }

    /// Reference `shouldRollover`: the record that would reach the ceiling
    /// rotates before it is written, and a ceiling of zero or less never does.
    fn rotates_before(&self, line: &str) -> bool {
        let Ok(ceiling) = u64::try_from(self.settings.max_bytes) else {
            return false;
        };
        if ceiling == 0 {
            return false;
        }
        let size = fs::metadata(&self.path).map_or(0, |metadata| metadata.len());
        let written = u64::try_from(line.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        size.saturating_add(written) >= ceiling
    }
}

// --------------------------------------------------------------------------
// Initialization
// --------------------------------------------------------------------------

/// The one file this process logs to, installed once.
static INSTALLED: OnceLock<FileLog> = OnceLock::new();

/// What [`init_file_logging`] did, so a second entrypoint in the same process
/// can tell an installation from a duplicate rather than counting handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogInstallation {
    Installed,
    /// Reference guards on a handler already pointing at the resolved path and
    /// returns without attaching a second one.
    AlreadyInstalled,
}

/// Why a log file could not be installed. Every variant leaves the binary
/// running: the caller reports the degradation once and starts anyway.
#[derive(Debug, Error)]
pub enum LogInitError {
    #[error("the log directory `{path}` could not be created: {source}")]
    Directory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the log file `{path}` could not be opened: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Settings(#[from] LogSettingsError),
}

/// Reference `init_file_logging`: the duplicate guard, the directory, the
/// resolved settings and the attached sink, in that order.
///
/// The directory is created before the settings are read, so a ceiling that is
/// not a number leaves the directory behind exactly as upstream does.
pub fn init_file_logging(
    path: &Path,
    read: &dyn Fn(&str) -> Option<String>,
) -> Result<LogInstallation, LogInitError> {
    if INSTALLED.get().is_some() {
        return Ok(LogInstallation::AlreadyInstalled);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| LogInitError::Directory {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let settings = LogSettings::resolve(read)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| LogInitError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    if INSTALLED.set(FileLog::new(path, settings)).is_err() {
        return Ok(LogInstallation::AlreadyInstalled);
    }
    Ok(LogInstallation::Installed)
}

/// The file this process logs to, once one is installed.
#[must_use]
pub fn installed_log() -> Option<&'static FileLog> {
    INSTALLED.get()
}

/// Writes one record to the installed file, or nothing at all when none is
/// installed. A failed write is dropped: logging never fails a caller.
pub fn log(level: LogLevel, message: &str) {
    if let Some(file) = INSTALLED.get() {
        drop(file.write(level, message, None));
    }
}

// --------------------------------------------------------------------------
// Reading
// --------------------------------------------------------------------------

/// One parsed line. Reference `LogEntry`, with the timestamp kept as the text
/// the line carried so a naive stamp is not published with an offset it never
/// had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub timestamp: String,
    pub ppid: i64,
    pub pid: i64,
    pub level: String,
    pub message: String,
    pub raw_line: String,
    /// Where the line sits counting back from the end, ignoring blank lines.
    pub line_number: i64,
}

/// Reference `PaginatedLogs`: one page, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogPage {
    pub entries: Vec<LogEntry>,
    pub has_more: bool,
    pub cursor: Option<i64>,
}

/// Reference `LogReader`: pages a log file backward from its end.
///
/// Reference also carries a polling watcher that pushes new lines to a
/// consumer. It never starts at the pinned commit, and this port's console
/// pulls pages instead, so what is ported is the read path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogReader {
    path: PathBuf,
}

impl LogReader {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reference `get_logs`: `limit` entries starting `offset` non-blank lines
    /// back from the end, newest first.
    ///
    /// `offset` counts lines rather than entries, so a line the pattern refuses
    /// still advances the window; that is what keeps a page stable while a
    /// hand-edited line sits in the middle of the file. A page that filled its
    /// limit reports where the next one starts; the last page reports nothing,
    /// which is how a caller knows it reached the top.
    #[must_use]
    pub fn get_logs(&self, limit: usize, offset: usize) -> LogPage {
        let mut entries: Vec<LogEntry> = Vec::new();
        let mut skipped = 0usize;
        let mut relative = 0i64;
        let filled = |entries: &Vec<LogEntry>| entries.len() >= limit;
        let Ok(mut file) = File::open(&self.path) else {
            return LogPage::default();
        };
        let Ok(mut position) = file.seek(SeekFrom::End(0)) else {
            return LogPage::default();
        };
        let mut remainder: Vec<u8> = Vec::new();
        while position > 0 {
            let read_size = u64::try_from(LOG_READ_CHUNK_SIZE)
                .unwrap_or(u64::MAX)
                .min(position);
            position -= read_size;
            if file.seek(SeekFrom::Start(position)).is_err() {
                break;
            }
            let Ok(size) = usize::try_from(read_size) else {
                break;
            };
            let mut chunk = vec![0u8; size];
            if file.read_exact(&mut chunk).is_err() {
                break;
            }
            chunk.extend_from_slice(&remainder);
            let mut lines = chunk.split(|byte| *byte == b'\n').collect::<Vec<_>>();
            // The first piece of a chunk continues into the one before it, so
            // it is carried over rather than read as a line of its own.
            remainder = lines.first().map(|line| line.to_vec()).unwrap_or_default();
            if !lines.is_empty() {
                lines.remove(0);
            }
            for line in lines.iter().rev() {
                if line.is_empty() {
                    continue;
                }
                if skipped < offset {
                    skipped = skipped.saturating_add(1);
                    continue;
                }
                relative = relative.saturating_add(1);
                if let Some(entry) = parse_log_line(&String::from_utf8_lossy(line), relative) {
                    entries.push(entry);
                    if filled(&entries) {
                        return Self::truncated(entries, offset);
                    }
                }
            }
        }
        if !remainder.is_empty() {
            if skipped < offset {
                return LogPage {
                    entries,
                    has_more: false,
                    cursor: None,
                };
            }
            relative = relative.saturating_add(1);
            if let Some(entry) = parse_log_line(&String::from_utf8_lossy(&remainder), relative) {
                entries.push(entry);
                if filled(&entries) {
                    return Self::truncated(entries, offset);
                }
            }
        }
        LogPage {
            entries,
            has_more: false,
            cursor: None,
        }
    }

    /// A page that stopped at its limit: the caller resumes where it ended.
    fn truncated(entries: Vec<LogEntry>, offset: usize) -> LogPage {
        let cursor = i64::try_from(offset.saturating_add(entries.len())).unwrap_or(i64::MAX);
        LogPage {
            entries,
            has_more: true,
            cursor: Some(cursor),
        }
    }
}

/// Reference `_parse_line`: the five captures, or nothing at all.
///
/// A line the pattern refuses, a timestamp no calendar holds and an identifier
/// too large to carry all answer the same way, so a page is never failed by one
/// line.
#[must_use]
pub fn parse_log_line(line: &str, line_number: i64) -> Option<LogEntry> {
    let captures = log_line_pattern()?.captures(line)?;
    let timestamp = normalized_timestamp(captures.name("timestamp")?.as_str())?;
    Some(LogEntry {
        timestamp,
        ppid: captures.name("ppid")?.as_str().parse().ok()?,
        pid: captures.name("pid")?.as_str().parse().ok()?,
        level: captures.name("level")?.as_str().to_owned(),
        message: captures.name("message")?.as_str().to_owned(),
        raw_line: line.to_owned(),
        line_number,
    })
}

/// The timestamp a parsed entry publishes: the same instant the line carried,
/// with its fractional seconds normalized to six digits and dropped when they
/// are zero, and its offset kept or absent exactly as written.
///
/// A stamp no calendar holds resolves to nothing, which is what makes a line
/// with a valid shape and an impossible date skip rather than parse.
fn normalized_timestamp(raw: &str) -> Option<String> {
    UtcTimestamp::parse_iso8601(raw)?;
    let (body, offset) = split_offset(raw);
    let seconds = body.get(..19)?;
    let micros = fraction_micros(body.get(20..)?)?;
    if micros == 0 {
        Some(format!("{seconds}{offset}"))
    } else {
        Some(format!("{seconds}.{micros:06}{offset}"))
    }
}

/// Splits a `±HH:MM` suffix off a timestamp, leaving the empty string when the
/// stamp carries none.
fn split_offset(raw: &str) -> (&str, &str) {
    let Some(start) = raw.len().checked_sub(6) else {
        return (raw, "");
    };
    match raw.get(start..) {
        Some(suffix) if suffix.starts_with('+') || suffix.starts_with('-') => {
            (raw.get(..start).unwrap_or(raw), suffix)
        }
        _ => (raw, ""),
    }
}

/// Fractional-second digits as microseconds, padded to six and truncated past
/// them.
fn fraction_micros(digits: &str) -> Option<i64> {
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    format!("{digits:0<6}").get(..6)?.parse().ok()
}
