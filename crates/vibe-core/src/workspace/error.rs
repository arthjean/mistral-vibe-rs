use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace path `{0}` escapes the authorized root")]
    OutsideRoot(PathBuf),
    #[error("filesystem error at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("binary file `{0}` cannot be injected as text")]
    Binary(PathBuf),
    #[error("file `{0}` is not valid UTF-8")]
    InvalidEncoding(PathBuf),
    #[error("file `{0}` changed while it was being read")]
    ChangedDuringRead(PathBuf),
    #[error("discovery exceeded the {0}-entry limit")]
    DiscoveryLimit(usize),
    #[error("write is {actual} bytes, exceeding the {limit}-byte limit")]
    WriteLimit { actual: usize, limit: usize },
    #[error("`{0}` already exists; use edit to modify it")]
    AlreadyExists(PathBuf),
    #[error("invalid search pattern: {0}")]
    InvalidPattern(String),
    #[error("the search pattern is empty")]
    EmptyPattern,
    #[error("the search path `{0}` does not exist")]
    MissingSearchPath(String),
    #[error("the search did not finish inside the {seconds}-second budget")]
    SearchTimeout { seconds: u64 },
    #[error("git inspection failed: {0}")]
    GitInspection(String),
    #[error("edit is stale in `{path}` because `{needle}` was not found")]
    StaleEdit { path: PathBuf, needle: String },
    #[error("edit in `{path}` matches {matches} locations; set replace_all explicitly")]
    AmbiguousEdit { path: PathBuf, matches: usize },
    #[error("a review mutation is unavailable while a turn is active")]
    ReviewBusy,
    #[error("mutation requires an active turn")]
    NoActiveTurn,
    #[error("review snapshots are {actual} bytes, exceeding the {limit}-byte limit")]
    ReviewSnapshotLimit { actual: usize, limit: usize },
    #[error("checkpoint capture failed: {0}")]
    Checkpoint(String),
    #[error("numeric limit cannot be represented on this platform")]
    LimitOverflow,
}
