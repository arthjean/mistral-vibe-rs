//! Who owns a managed worktree, and who is standing in it.
//!
//! Reference `vibe/core/git/worktree/record.py`. A managed worktree is the
//! directory `<vibe home>/worktrees/<bucket>/<name>`, and everything known
//! about it lives beside the buckets rather than inside them, under
//! `.claims/<bucket>/<name>/`:
//!
//! - `record.json`, the ownership record written when the name is reserved and
//!   completed with the base commit once `git worktree add` returned;
//! - `recovery.json`, written when retention removed the worktree after saving
//!   its state to a snapshot ref;
//! - `holders/`, one empty file per session working in the worktree, each held
//!   under an exclusive advisory lock for as long as the session lives, plus a
//!   `.starting` marker a preparation holds while it is in flight.
//!
//! The layout, the file names and the JSON shapes are the reference's, and the
//! locks are the same `flock` the reference takes, so a worktree prepared by
//! either implementation is managed by the other: a session of one holds off a
//! retention sweep of the other.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};

use crate::auth::UtcTimestamp;

use super::{ManagedRoot, WorktreeError, resolve_lenient};

/// The directory claims live in, beside the buckets. A bucket is always
/// `<name>-<12 hex>`, so this leading-dot name cannot collide with one
/// (`vibe/core/git/worktree/record.py:25-31`).
pub const CLAIMS_DIR_NAME: &str = ".claims";
const RECORD_FILENAME: &str = "record.json";
const RECOVERY_FILENAME: &str = "recovery.json";
const HOLDERS_DIR_NAME: &str = "holders";
const STARTING_HOLDER: &str = ".starting";
const PRUNE_LOCK_FILENAME: &str = ".prune";
/// The registry lock every holder change and holder scan serializes on, which
/// is the session lease's directory lock (`vibe/core/session/session_lease.py:124-137`).
const REGISTRY_LOCK_FILENAME: &str = ".registry";

/// The holder files this process keeps locked, with how many times each was
/// taken. A second hold of the same file from this process must not open it
/// again: `flock` locks belong to the open file description, so a second
/// descriptor would contend with the first (`vibe/core/git/worktree/record.py:36-37`).
static HELD_FILES: LazyLock<Mutex<HashMap<PathBuf, (File, usize)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The ownership record of one managed worktree.
///
/// Field order and names are the reference's `WorktreeRecord`, because the file
/// is shared with it. `base_commit` is [`None`] between the directory claim
/// and a completed `git worktree add`, when the record describes a
/// reservation rather than a worktree (`vibe/core/git/worktree/record.py:45-70`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    #[serde(default = "record_version")]
    pub version: u32,
    pub name: String,
    pub branch: String,
    pub repo_root: PathBuf,
    #[serde(default)]
    pub base_commit: Option<String>,
    pub branch_created: bool,
    #[serde(with = "timestamp")]
    pub claimed_at: UtcTimestamp,
}

const fn record_version() -> u32 {
    1
}

impl WorktreeRecord {
    #[must_use]
    pub fn new(name: &str, branch: &str, repo_root: &Path, branch_created: bool) -> Self {
        Self {
            version: 1,
            name: name.to_owned(),
            branch: branch.to_owned(),
            repo_root: repo_root.to_path_buf(),
            base_commit: None,
            branch_created,
            claimed_at: UtcTimestamp::now(),
        }
    }
}

/// What retention left behind when it removed a worktree with work in it:
/// where the state went, and enough to put the worktree back
/// (`vibe/core/git/worktree/record.py:73-100`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecoveryRecord {
    #[serde(default = "record_version")]
    pub version: u32,
    pub name: String,
    pub branch: String,
    pub repo_root: PathBuf,
    pub base_commit: String,
    pub snapshot_ref: String,
    #[serde(with = "timestamp")]
    pub removed_at: UtcTimestamp,
}

impl WorktreeRecoveryRecord {
    /// The recovery record for a removal of `record`'s worktree.
    ///
    /// A reservation that never became a worktree has no base to recover to,
    /// so it has no recovery record either.
    pub fn new(record: &WorktreeRecord, snapshot_ref: &str) -> Result<Self, WorktreeError> {
        let base_commit = record.base_commit.clone().ok_or_else(|| {
            WorktreeError::record("an incomplete worktree claim cannot be recovered")
        })?;
        Ok(Self {
            version: 1,
            name: record.name.clone(),
            branch: record.branch.clone(),
            repo_root: record.repo_root.clone(),
            base_commit,
            snapshot_ref: snapshot_ref.to_owned(),
            removed_at: UtcTimestamp::now(),
        })
    }
}

/// The instant a record carries, spelled as pydantic spells an aware UTC
/// `datetime`: ISO 8601 with a `Z`, fractional seconds only when nonzero.
mod timestamp {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::auth::UtcTimestamp;

    pub(super) fn serialize<S: Serializer>(
        value: &UtcTimestamp,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let rendered = value.to_iso8601();
        let rendered = rendered
            .strip_suffix("+00:00")
            .map_or_else(|| rendered.clone(), |head| format!("{head}Z"));
        serializer.serialize_str(&rendered)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<UtcTimestamp, D::Error> {
        let text = String::deserialize(deserializer)?;
        UtcTimestamp::parse_iso8601(&text)
            .ok_or_else(|| serde::de::Error::custom(format!("not an ISO 8601 instant: {text}")))
    }
}

/// The directory name a repository's managed worktrees live under: the
/// primary checkout's name and twelve hex digits of the common git directory's
/// digest (`vibe/core/git/worktree/record.py:103-105`).
#[must_use]
pub fn managed_bucket_name(repo_root: &Path, common_git_dir: &Path) -> String {
    use sha2::{Digest as _, Sha256};

    let digest = hex::encode(Sha256::digest(
        super::strip_verbatim_prefix(&common_git_dir.to_string_lossy()).as_bytes(),
    ));
    let repository_name = repo_root
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!(
        "{repository_name}-{}",
        &digest[..super::REPOSITORY_DIGEST_LENGTH]
    )
}

/// An exclusive lock on one file under the claims root, released on drop.
struct DirectoryLock {
    file: File,
}

impl DirectoryLock {
    fn acquire(managed: &ManagedRoot, name: &str) -> Result<Self, WorktreeError> {
        let root = managed.claims();
        fs::create_dir_all(&root).map_err(|source| WorktreeError::io(&root, source))?;
        let path = root.join(name);
        let file = open_lock_file(&path)?;
        fs2::FileExt::lock_exclusive(&file).map_err(|source| WorktreeError::io(&path, source))?;
        Ok(Self { file })
    }
}

impl Drop for DirectoryLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

fn open_lock_file(path: &Path) -> Result<File, WorktreeError> {
    let mut options = OpenOptions::new();
    options.read(true).append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|source| WorktreeError::io(path, source))
}

/// Holds the retention lock for its lifetime.
///
/// Every sweep takes it, and so does every attachment hold, so a worktree
/// cannot be judged abandoned while a session is on its way into it
/// (`vibe/core/git/worktree/record.py:112-121`).
pub struct PruneLock {
    _lock: DirectoryLock,
}

impl PruneLock {
    pub fn acquire(managed: &ManagedRoot) -> Result<Self, WorktreeError> {
        Ok(Self {
            _lock: DirectoryLock::acquire(managed, PRUNE_LOCK_FILENAME)?,
        })
    }
}

/// One managed worktree, addressed by its bucket and name.
///
/// The two travel as one value because they are meaningless apart and
/// indistinguishable as bare strings (`vibe/core/git/worktree/record.py:124-130`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorktreeClaim {
    managed: ManagedRoot,
    pub bucket: String,
    pub name: String,
}

impl WorktreeClaim {
    #[must_use]
    pub fn new(managed: &ManagedRoot, bucket: &str, name: &str) -> Self {
        Self {
            managed: managed.clone(),
            bucket: bucket.to_owned(),
            name: name.to_owned(),
        }
    }

    /// The claim `path` sits in, or [`None`] outside every managed worktree.
    ///
    /// A path is placed by where it resolves, so a spelling through a link
    /// names the same claim (`vibe/core/git/worktree/record.py:132-142`).
    #[must_use]
    pub fn locate(managed: &ManagedRoot, path: &Path) -> Option<Self> {
        let resolved = resolve_lenient(path);
        let relative = resolved.strip_prefix(managed.path()).ok()?;
        let mut parts = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            });
        let bucket = parts.next()?;
        let name = parts.next()?;
        (bucket != CLAIMS_DIR_NAME).then(|| Self::new(managed, &bucket, &name))
    }

    /// Every claimed name of one bucket.
    ///
    /// Only claims, never a listing of the bucket itself: a directory there with
    /// no claim is a live reservation or something the user made, and neither
    /// belongs to automatic cleanup (`vibe/core/git/worktree/record.py:144-157`).
    #[must_use]
    pub fn in_bucket(managed: &ManagedRoot, bucket: &str) -> Vec<Self> {
        let Ok(entries) = fs::read_dir(managed.claims().join(bucket)) else {
            return Vec::new();
        };
        let mut claims = entries
            .filter_map(Result::ok)
            .map(|entry| Self::new(managed, bucket, &entry.file_name().to_string_lossy()))
            .collect::<Vec<_>>();
        claims.sort_by(|left, right| left.name.cmp(&right.name));
        claims
    }

    /// Every claim of every bucket.
    #[must_use]
    pub fn all(managed: &ManagedRoot) -> Vec<Self> {
        let Ok(entries) = fs::read_dir(managed.claims()) else {
            return Vec::new();
        };
        let mut buckets = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        buckets.sort();
        buckets
            .iter()
            .flat_map(|bucket| Self::in_bucket(managed, bucket))
            .collect()
    }

    #[must_use]
    pub fn managed(&self) -> &ManagedRoot {
        &self.managed
    }

    /// Where this claim's files are.
    #[must_use]
    pub fn directory(&self) -> PathBuf {
        self.managed.claims().join(&self.bucket).join(&self.name)
    }

    pub fn write(&self, record: &WorktreeRecord) -> Result<(), WorktreeError> {
        self.write_json(RECORD_FILENAME, record)
    }

    /// The record, or [`None`] when there is none or it cannot be read.
    ///
    /// Fails closed: an unreadable record means the worktree is not treated as
    /// Vibe's, so nothing is deleted on the strength of one, and the file is
    /// left in place as what may be the only breadcrumb
    /// (`vibe/core/git/worktree/record.py:163-175`).
    #[must_use]
    pub fn read(&self) -> Option<WorktreeRecord> {
        read_json(&self.directory().join(RECORD_FILENAME))
    }

    #[must_use]
    pub fn has_recovery(&self) -> bool {
        self.directory().join(RECOVERY_FILENAME).exists()
    }

    pub fn write_recovery(&self, recovery: &WorktreeRecoveryRecord) -> Result<(), WorktreeError> {
        self.write_json(RECOVERY_FILENAME, recovery)
    }

    #[must_use]
    pub fn read_recovery(&self) -> Option<WorktreeRecoveryRecord> {
        read_json(&self.directory().join(RECOVERY_FILENAME))
    }

    pub fn delete_recovery(&self) {
        let _ = fs::remove_file(self.directory().join(RECOVERY_FILENAME));
        self.discard_empty_directories();
    }

    /// Deletes the record, and the claim directories if nothing else is in
    /// them. A surviving holder keeps them, because losing its marker would
    /// let the next release delete the worktree underneath a live session.
    pub fn delete(&self) {
        self.finish_starting();
        let _ = fs::remove_file(self.directory().join(RECORD_FILENAME));
        self.discard_empty_directories();
    }

    fn discard_empty_directories(&self) {
        let directory = self.directory();
        let bucket = directory.parent().map(Path::to_path_buf);
        let candidates = [
            Some(directory.join(HOLDERS_DIR_NAME)),
            Some(directory),
            bucket,
        ];
        for candidate in candidates.into_iter().flatten() {
            match fs::remove_dir(&candidate) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return,
            }
        }
    }

    /// The holder file for `session_id`, refusing any id that would name a
    /// file outside the holders directory rather than sanitizing it into one
    /// that still unlinks the wrong path (`vibe/core/git/worktree/record.py:217-230`).
    fn holder_path(&self, session_id: &str) -> Result<PathBuf, WorktreeError> {
        let holders = self.directory().join(HOLDERS_DIR_NAME);
        let mut components = Path::new(session_id).components();
        let single = matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(_)), None)
        );
        if session_id.is_empty() || !single || session_id.contains(['/', '\\']) {
            return Err(WorktreeError::record(format!(
                "unusable worktree holder id {session_id:?}"
            )));
        }
        Ok(holders.join(session_id))
    }

    pub fn add_holder(&self, session_id: &str) -> Result<(), WorktreeError> {
        self.acquire_holder(&self.holder_path(session_id)?, true)
    }

    /// Drops a holder. The last holder out of a claim whose record was already
    /// deleted takes the empty directories with it, because nothing else
    /// revisits them (`vibe/core/git/worktree/record.py:235-243`).
    pub fn remove_holder(&self, session_id: &str) -> Result<(), WorktreeError> {
        self.release_holder(&self.holder_path(session_id)?);
        if !self.directory().join(RECORD_FILENAME).exists() {
            self.discard_empty_directories();
        }
        Ok(())
    }

    /// The sessions standing in this worktree, the in-flight preparation
    /// marker excluded.
    #[must_use]
    pub fn holders(&self) -> BTreeSet<String> {
        self.live_holders(|name| name != STARTING_HOLDER)
    }

    /// Marks a preparation in flight. Not reference counted: a second one for
    /// the same claim is refused, which is what serializes two restores.
    pub fn mark_starting(&self) -> Result<(), WorktreeError> {
        self.acquire_holder(&self.holder_path(STARTING_HOLDER)?, false)
    }

    pub fn finish_starting(&self) {
        if let Ok(path) = self.holder_path(STARTING_HOLDER) {
            self.release_holder(&path);
        }
        self.discard_empty_directories();
    }

    #[must_use]
    pub fn is_starting(&self) -> bool {
        !self.live_holders(|name| name == STARTING_HOLDER).is_empty()
    }

    fn acquire_holder(&self, holder: &Path, reference_counted: bool) -> Result<(), WorktreeError> {
        if let Some(parent) = holder.parent() {
            fs::create_dir_all(parent).map_err(|source| WorktreeError::io(parent, source))?;
        }
        let _registry = DirectoryLock::acquire(&self.managed, REGISTRY_LOCK_FILENAME)?;
        let mut held = HELD_FILES
            .lock()
            .map_err(|_| WorktreeError::record("the holder registry is poisoned"))?;
        if let Some(entry) = held.get_mut(holder) {
            if !reference_counted {
                return Err(WorktreeError::record(format!(
                    "worktree holder {:?} is already active",
                    file_name(holder)
                )));
            }
            entry.1 += 1;
            return Ok(());
        }
        let file = open_lock_file(holder)?;
        if fs2::FileExt::try_lock_exclusive(&file).is_err() {
            return Err(WorktreeError::record(format!(
                "worktree holder {:?} is already active",
                file_name(holder)
            )));
        }
        held.insert(holder.to_path_buf(), (file, 1));
        Ok(())
    }

    fn release_holder(&self, holder: &Path) {
        if !holder.exists() && !holder_is_owned(holder) {
            return;
        }
        let Ok(_registry) = DirectoryLock::acquire(&self.managed, REGISTRY_LOCK_FILENAME) else {
            return;
        };
        if let Ok(mut held) = HELD_FILES.lock()
            && let Some(entry) = held.get_mut(holder)
        {
            if entry.1 > 1 {
                entry.1 -= 1;
                return;
            }
            if let Some((file, _)) = held.remove(holder) {
                let _ = fs2::FileExt::unlock(&file);
                drop(file);
                let _ = fs::remove_file(holder);
            }
            return;
        }
        discard_stale_holder(holder);
    }

    fn live_holders(&self, keep: impl Fn(&str) -> bool) -> BTreeSet<String> {
        let directory = self.directory().join(HOLDERS_DIR_NAME);
        let mut live = BTreeSet::new();
        let Ok(_registry) = DirectoryLock::acquire(&self.managed, REGISTRY_LOCK_FILENAME) else {
            return live;
        };
        let Ok(entries) = fs::read_dir(&directory) else {
            return live;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if keep(&name) && holder_is_live(&entry.path()) {
                live.insert(name);
            }
        }
        live
    }

    fn write_json(&self, file: &str, value: &impl Serialize) -> Result<(), WorktreeError> {
        let directory = self.directory();
        fs::create_dir_all(&directory).map_err(|source| WorktreeError::io(&directory, source))?;
        let target = directory.join(file);
        let rendered = serde_json::to_string_pretty(value)
            .map_err(|error| WorktreeError::record(error.to_string()))?;
        let staging = directory.join(format!(
            ".{file}.{}.{}.json.tmp",
            std::process::id(),
            crate::clock::now_nanos()
        ));
        let written = (|| {
            let mut handle = File::create(&staging)?;
            handle.write_all(rendered.as_bytes())?;
            handle.sync_all()?;
            fs::rename(&staging, &target)
        })();
        if let Err(source) = written {
            let _ = fs::remove_file(&staging);
            return Err(WorktreeError::io(&target, source));
        }
        Ok(())
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn holder_is_owned(holder: &Path) -> bool {
    HELD_FILES
        .lock()
        .is_ok_and(|held| held.contains_key(holder))
}

/// Whether a holder file belongs to a live session.
///
/// A marker is live while some process holds its lock. One nobody holds was
/// left by a process that died, and is removed on the way past, which is the
/// only liveness signal available across unrelated processes
/// (`vibe/core/git/worktree/record.py:365-393`).
fn holder_is_live(holder: &Path) -> bool {
    holder_is_owned(holder) || !discard_stale_holder(holder)
}

/// Removes `holder` when no process holds it, answering whether it did.
fn discard_stale_holder(holder: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).append(true).open(holder) else {
        return false;
    };
    if fs2::FileExt::try_lock_exclusive(&file).is_err() {
        return false;
    }
    let _ = fs2::FileExt::unlock(&file);
    drop(file);
    fs::remove_file(holder).is_ok()
}

/// A temporary holder bridging worktree resolution and session attachment.
///
/// Pruning has to see the worktree as occupied before the session has an id it
/// can register, so this holds it in between and is released once the session
/// holder is in place or startup is abandoned
/// (`vibe/core/git/worktree/repository.py:48-62`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSessionHold {
    pub claim: WorktreeClaim,
    pub holder_id: String,
}

impl PendingSessionHold {
    pub fn release(&self) {
        let _ = self.claim.remove_holder(&self.holder_id);
    }
}
