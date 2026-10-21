//! The unified session store: one directory per session under
//! `<session root>/unified/`.
//!
//! The layout is the reference's (`harness/runtimes/python/python/mistralai_vibe_local_harness/vibe/_storage.py`
//! and the Host beside it): `CURRENT` names the newest generation, each
//! generation holds a manifest over its checkpoint, runtime state and
//! projection state, larger parts live in content-addressed chunks, a journal
//! segment records the steps since the generation, and `meta.json` with
//! `scheduled-loops.json` describe the session to listings. What a client can
//! observe of it is the layout, the descriptors and the projected snapshot;
//! the checkpoint, runtime state and journal records are this port's own.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use vibe_core::atomic_file::{FileMode, write_atomically};
use vibe_core::clock::now_millis;
use vibe_core::pycompat::{Dumps, dumps};
use vibe_core::text::sha256_hex;
use vibe_core::time::format_iso_millis_z;

use crate::engine::{ConversationMessage, SessionCore};
use crate::state::{HarnessState, Projector};

pub const STORE_FORMAT: &str = "mistral.vibe.unified-session-store/v1";
pub const STORE_FORMAT_MINOR: u64 = 8;
const LOOPS_FORMAT: &str = "vibe.scheduled-loops/v1";

/// The directory every unified session lives under.
#[must_use]
pub fn unified_root(session_root: &Path) -> PathBuf {
    session_root.join("unified")
}

/// One session's directory.
#[derive(Debug, Clone)]
pub struct SessionStore {
    pub dir: PathBuf,
    generation: u64,
    journal: u64,
}

fn compact(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}

/// Writes through a sibling temporary file, so a reader never sees half.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_atomically(path, "unified", bytes, FileMode::Private).map_err(|error| error.source)
}

fn generation_name(generation: u64) -> String {
    format!("{generation:016}")
}

impl SessionStore {
    /// The store a session promoted to disk: its directory, metadata and
    /// first generation.
    ///
    /// # Errors
    ///
    /// The directory or a file in it cannot be written.
    pub fn create(
        dir: PathBuf,
        meta: &Map<String, Value>,
        core: &SessionCore,
    ) -> std::io::Result<Self> {
        let mut store = Self::begin(dir, meta)?;
        // Reference `_persist_scheduled_loops_after_promotion`: what the
        // session scheduled before it was saved is saved with it.
        core.loops
            .attach(store.dir.join("scheduled-loops.json"))
            .map_err(|error| std::io::Error::other(error.0))?;
        store.commit(core, false)?;
        Ok(store)
    }

    /// Reference `_write_initial_store` of an import: the metadata and first
    /// generation, with no schedule written until one is made.
    ///
    /// # Errors
    ///
    /// The directory or a file in it cannot be written.
    pub fn create_imported(
        dir: PathBuf,
        meta: &Map<String, Value>,
        core: &SessionCore,
    ) -> std::io::Result<Self> {
        let mut store = Self::begin(dir, meta)?;
        store.commit(core, false)?;
        Ok(store)
    }

    fn begin(dir: PathBuf, meta: &Map<String, Value>) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let store = Self {
            dir,
            generation: 0,
            journal: 0,
        };
        store.write_meta(meta)?;
        Ok(store)
    }

    /// The store of a subagent's session: its first generation, without the
    /// listing metadata a root carries.
    ///
    /// # Errors
    ///
    /// The directory or a file in it cannot be written.
    pub fn create_child(dir: PathBuf, core: &SessionCore) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let mut store = Self {
            dir,
            generation: 0,
            journal: 0,
        };
        store.commit(core, false)?;
        Ok(store)
    }

    /// The directory the store lives in.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Opens an existing store.
    #[must_use]
    pub fn open(dir: PathBuf) -> Option<Self> {
        let current: Value = serde_json::from_slice(&fs::read(dir.join("CURRENT")).ok()?).ok()?;
        let generation = current["generation"].as_str()?.parse().ok()?;
        let journal = current["snapshot_sequence"].as_u64().unwrap_or(0);
        Some(Self {
            dir,
            generation,
            journal,
        })
    }

    /// Rewrites `meta.json` as the reference does, indented by two.
    ///
    /// # Errors
    ///
    /// The file cannot be written.
    pub fn write_meta(&self, meta: &Map<String, Value>) -> std::io::Result<()> {
        let mut fields: Vec<(&String, &Value)> = meta.iter().collect();
        fields.sort_by_key(|(key, _)| {
            META_ORDER
                .iter()
                .position(|known| known == key)
                .unwrap_or(usize::MAX)
        });
        let ordered = fields
            .into_iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let text = dumps(
            &Value::Object(ordered),
            Dumps {
                ensure_ascii: false,
                indent: Some(2),
                ..Dumps::DEFAULT
            },
        );
        write_atomic(&self.dir.join("meta.json"), text.as_bytes())
    }

    /// Rewrites `scheduled-loops.json`.
    ///
    /// # Errors
    ///
    /// The file cannot be written.
    pub fn write_loops(&self, loops: &[Value]) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(&json!({"format": LOOPS_FORMAT, "loops": loops}))
            .unwrap_or_default();
        write_atomic(&self.dir.join("scheduled-loops.json"), text.as_bytes())
    }

    /// Reads `meta.json`.
    #[must_use]
    pub fn read_meta(&self) -> Option<Map<String, Value>> {
        read_meta(&self.dir)
    }

    /// Records one journaled step in the current segment.
    ///
    /// # Errors
    ///
    /// The segment cannot be appended to.
    pub fn journal(&mut self, kind: &str, payload: Value) -> std::io::Result<()> {
        self.journal += 1;
        let path = self
            .dir
            .join("journal")
            .join(format!("{}.jsonl", generation_name(self.generation + 1)));
        fs::create_dir_all(path.parent().unwrap_or(&self.dir))?;
        let record = json!({"sequence": self.journal, "type": kind, "payload": payload});
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let mut line = compact(&record);
        line.push(b'\n');
        file.write_all(&line)
    }

    fn chunk(&self, value: &Value) -> std::io::Result<String> {
        let bytes = compact(value);
        let digest = sha256_hex(&bytes);
        let path = self.dir.join("chunks").join(format!("{digest}.json"));
        if !path.exists() {
            write_atomic(&path, &bytes)?;
        }
        Ok(digest)
    }

    fn part(
        &self,
        directory: &Path,
        name: &str,
        value: &Value,
        chunks: Vec<String>,
    ) -> std::io::Result<Value> {
        let bytes = compact(value);
        write_atomic(&directory.join(name), &bytes)?;
        Ok(json!({"chunks": chunks, "path": name, "sha256": sha256_hex(&bytes)}))
    }

    /// Writes a generation over the session as it stands and points
    /// `CURRENT` at it.
    ///
    /// # Errors
    ///
    /// A file of the generation cannot be written.
    pub fn commit(&mut self, core: &SessionCore, running: bool) -> std::io::Result<()> {
        let sequence = core.projector.watermark;
        let generation = sequence.max(self.generation + 1);
        let name = generation_name(generation);
        let directory = self.dir.join("generations").join(&name);
        fs::create_dir_all(&directory)?;
        let durable = core.projector.durable();
        let session_id = durable.session.id.clone();
        let conversation: Vec<Value> = core
            .conversation
            .iter()
            .map(ConversationMessage::to_json)
            .collect();
        let conversation_chunk = self.chunk(&Value::Array(conversation))?;
        let mut checkpoint = self.part(
            &directory,
            "checkpoint.json",
            &json!({
                "checkpoint_version": 1,
                "conversation": conversation_chunk,
                "last_input_id": core.inputs,
            }),
            vec![conversation_chunk.clone()],
        )?;
        checkpoint["checkpoint_version"] = json!(1);
        let todos_chunk = self.chunk(&json!({"todos": core.vibe.todos}))?;
        let mut runtime_state = json!({
            "runtime_state_version": 1,
            "tools": todos_chunk,
            "pins": core.pins,
            "processes": core.processes.public(),
        });
        if core.without_plugins {
            runtime_state["without_plugins"] = json!(true);
        }
        let runtime = self.part(
            &directory,
            "runtime-state.json",
            &runtime_state,
            vec![todos_chunk.clone()],
        )?;
        let entries_chunk = self.chunk(&Value::Array(durable.entries.clone()))?;
        let projection = self.part(
            &directory,
            "projection-state.json",
            &json!({
                "projection_state_version": 1,
                "session_id": session_id,
                "snapshot": durable.persisted_json(),
                "snapshot_sequence": sequence,
                "watermark": sequence,
            }),
            vec![entries_chunk.clone()],
        )?;
        let manifest = json!({
            "checkpoint": checkpoint,
            "created_at": format_iso_millis_z(now_millis()),
            "execution_state": if running { "running" } else { "quiescent" },
            "generation": name,
            "interop_export": null,
            "manifest_version": 1,
            "projection_state": projection,
            "recovery_journal_segment": {
                "first_sequence": sequence + 1,
                "path": format!("journal/{}.jsonl", generation_name(sequence + 1)),
            },
            "runtime_state": runtime,
            "session_id": session_id,
            "snapshot_sequence": sequence,
        });
        let manifest_bytes = compact(&manifest);
        write_atomic(&directory.join("manifest.json"), &manifest_bytes)?;
        let segment = self
            .dir
            .join("journal")
            .join(format!("{}.jsonl", generation_name(sequence + 1)));
        if !segment.exists() {
            write_atomic(&segment, b"")?;
        }
        write_atomic(
            &self.dir.join("CURRENT"),
            &compact(&json!({
                "generation": name,
                "manifest_sha256": sha256_hex(&manifest_bytes),
                "session_id": session_id,
                "snapshot_sequence": sequence,
                "store_format": STORE_FORMAT,
                "store_format_minor": STORE_FORMAT_MINOR,
            })),
        )?;
        self.generation = generation;
        self.journal = sequence;
        Ok(())
    }

    /// The session as its newest generation recorded it.
    #[must_use]
    pub fn load(&self, scratchpad: PathBuf) -> Option<SessionCore> {
        let manifest = self.manifest()?;
        let directory = self
            .dir
            .join("generations")
            .join(manifest["generation"].as_str()?);
        let read = |name: &str| -> Option<Value> {
            serde_json::from_slice(&fs::read(directory.join(name)).ok()?).ok()
        };
        let chunk = |digest: &str| -> Option<Value> {
            serde_json::from_slice(
                &fs::read(self.dir.join("chunks").join(format!("{digest}.json"))).ok()?,
            )
            .ok()
        };
        let checkpoint = read("checkpoint.json")?;
        let projection = read("projection-state.json")?;
        let runtime = read("runtime-state.json").unwrap_or(Value::Null);
        let entries = manifest["projection_state"]["chunks"][0]
            .as_str()
            .and_then(chunk)
            .and_then(|entries| entries.as_array().cloned())
            .unwrap_or_default();
        let mut state = HarnessState::from_persisted(&projection["snapshot"], entries);
        self.replay_journal(&mut state.session);
        let conversation = checkpoint["conversation"]
            .as_str()
            .and_then(chunk)
            .and_then(|messages| messages.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(ConversationMessage::from_json)
            .collect();
        let mut core = SessionCore::new(state, scratchpad);
        core.projector = Projector::new(
            core.projector.state,
            projection["snapshot_sequence"].as_u64().unwrap_or(0),
        );
        core.conversation = conversation;
        core.inputs = checkpoint["last_input_id"].as_u64().unwrap_or(0);
        core.without_plugins = runtime["without_plugins"].as_bool().unwrap_or(false);
        if let Some(pins) = runtime["pins"].as_object() {
            core.pins = pins
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect();
        }
        if let Some(todos) = runtime["tools"]
            .as_str()
            .and_then(chunk)
            .and_then(|tools| tools["todos"].as_array().cloned())
        {
            core.vibe.todos = todos;
        }
        // Reference `restore_scheduled_loops`; a schedule that cannot be
        // read starts empty.
        let _ = core.loops.restore(self.dir.join("scheduled-loops.json"));
        // A process the store left running belongs to no process now.
        if let Some(processes) = runtime["processes"].as_array() {
            core.processes.restore(processes);
            core.refresh_processes();
        }
        Some(core)
    }

    /// Applies what the journal recorded since the newest generation: the
    /// last `state` step names the session's latest title and update time,
    /// which a step taken after the session settled (a title landing late,
    /// a rename) carries only there.
    pub fn replay_journal(&self, session: &mut crate::state::HarnessSession) {
        let path = self
            .dir
            .join("journal")
            .join(format!("{}.jsonl", generation_name(self.generation + 1)));
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        let Some(last) = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .rfind(|record| record["type"] == "state")
        else {
            return;
        };
        let payload = &last["payload"];
        if let Some(updated_at) = payload["updatedAt"].as_u64() {
            session.updated_at = session.updated_at.max(updated_at);
        }
        if let Some(source) = payload["titleSource"].as_str() {
            session.title = payload["title"].as_str().map(ToOwned::to_owned);
            source.clone_into(&mut session.title_source);
        }
    }

    fn manifest(&self) -> Option<Value> {
        let name = generation_name(self.generation);
        serde_json::from_slice(
            &fs::read(
                self.dir
                    .join("generations")
                    .join(name)
                    .join("manifest.json"),
            )
            .ok()?,
        )
        .ok()
    }

    /// The projected snapshot of the newest generation, entries left out.
    #[must_use]
    pub fn snapshot(&self) -> Option<Value> {
        let manifest = self.manifest()?;
        let path = self
            .dir
            .join("generations")
            .join(manifest["generation"].as_str()?)
            .join("projection-state.json");
        let projection: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        Some(projection["snapshot"].clone())
    }
}

/// The order `meta.json` declares its fields in.
const META_ORDER: &[&str] = &[
    "session_id",
    "parent_session_id",
    "start_time",
    "end_time",
    "git_commit",
    "git_branch",
    "environment",
    "origin_directory",
    "username",
    "child_sessions",
    "loops",
    "title",
    "title_source",
    "bumped_at",
    "pinned_at",
    "archived_at",
    "unseen_at",
    "seen_at",
    "experiments",
    "config",
    "import_provenance",
    "created_worktree",
];

/// A session directory's `meta.json`.
#[must_use]
pub fn read_meta(dir: &Path) -> Option<Map<String, Value>> {
    let value: Value = serde_json::from_slice(&fs::read(dir.join("meta.json")).ok()?).ok()?;
    value.as_object().cloned()
}

/// What `meta.json` holds for a session that has just been promoted, in the
/// reference's key order.
#[must_use]
pub fn new_meta(
    session_id: &str,
    parent_session_id: Option<&str>,
    start_time: &str,
    working_directory: &str,
    username: &str,
) -> Map<String, Value> {
    let value = json!({
        "session_id": session_id,
        "parent_session_id": parent_session_id,
        "start_time": start_time,
        "end_time": null,
        "git_commit": null,
        "git_branch": null,
        "environment": {"working_directory": working_directory},
        "origin_directory": working_directory,
        "username": username,
        "child_sessions": [],
        "loops": [],
        "title": null,
        "title_source": "auto",
        "bumped_at": null,
        "pinned_at": null,
        "archived_at": null,
        "unseen_at": null,
        "seen_at": null,
        "experiments": null,
        "config": null,
        "import_provenance": null,
        "created_worktree": null,
    });
    value.as_object().cloned().unwrap_or_default()
}

/// Every session directory under the unified root that holds a generation.
#[must_use]
pub fn list(session_root: &Path) -> Vec<SessionStore> {
    let Ok(entries) = fs::read_dir(unified_root(session_root)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| SessionStore::open(entry.path()))
        .collect()
}

/// Reference `getpass.getuser`.
#[must_use]
pub fn username() -> String {
    ["LOGNAME", "USER", "LNAME", "USERNAME"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "unknown".to_owned())
}
