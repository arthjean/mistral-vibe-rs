use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use tokio::task::JoinHandle;

const MAX_HISTORY_ENTRIES: usize = 100;

#[derive(Debug)]
pub struct HistoryLoad {
    pub entries: Vec<String>,
    pub diagnostic: Option<String>,
}

/// Persistent prompt-history adapter.
///
/// Reads happen during TUI startup. Every write is dispatched to a blocking
/// worker, merged under a cross-process lock, and atomically replaces the
/// JSONL file. Diagnostics deliberately omit entry contents.
#[derive(Debug)]
pub struct PromptHistory {
    path: PathBuf,
    pending: VecDeque<String>,
    write: Option<JoinHandle<Result<(), String>>>,
}

impl PromptHistory {
    #[must_use]
    pub fn open(path: PathBuf) -> (Self, HistoryLoad) {
        let load = match read_entries(&path) {
            Ok(entries) => HistoryLoad {
                entries,
                diagnostic: None,
            },
            Err(error) => HistoryLoad {
                entries: Vec::new(),
                diagnostic: Some(format!(
                    "Prompt history could not be read from `{}`: {error}",
                    path.display()
                )),
            },
        };
        (
            Self {
                path,
                pending: VecDeque::new(),
                write: None,
            },
            load,
        )
    }

    pub fn persist(&mut self, entry: String) {
        self.pending.push_back(entry);
        self.start_next_write();
    }

    pub async fn drain_ready(&mut self) -> Vec<String> {
        let mut diagnostics = Vec::new();
        if self.write.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(write) = self.write.take()
        {
            collect_write_result(write.await, &mut diagnostics);
            self.start_next_write();
        }
        diagnostics
    }

    pub async fn finish(mut self) -> Vec<String> {
        let mut diagnostics = Vec::new();
        while let Some(write) = self.write.take() {
            collect_write_result(write.await, &mut diagnostics);
            self.start_next_write();
        }
        diagnostics
    }

    fn start_next_write(&mut self) {
        if self.write.is_some() {
            return;
        }
        let Some(entry) = self.pending.pop_front() else {
            return;
        };
        let path = self.path.clone();
        self.write = Some(tokio::task::spawn_blocking(move || {
            persist_entry(&path, &entry)
        }));
    }
}

fn collect_write_result(
    result: Result<Result<(), String>, tokio::task::JoinError>,
    output: &mut Vec<String>,
) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => output.push(error),
        Err(error) => output.push(format!("Prompt history worker failed: {error}")),
    }
}

fn read_entries(path: &Path) -> Result<Vec<String>, std::io::Error> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut entries = text
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<String>(line).unwrap_or_else(|_| line.to_owned()))
        .collect::<Vec<_>>();
    if entries.len() > MAX_HISTORY_ENTRIES {
        entries.drain(..entries.len() - MAX_HISTORY_ENTRIES);
    }
    Ok(entries)
}

fn persist_entry(path: &Path, entry: &str) -> Result<(), String> {
    let entry = entry.trim();
    if entry.is_empty() {
        return Ok(());
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| history_error(path, "create directory", error))?;
    let _lock = HistoryLock::acquire(path)?;
    let mut entries = read_entries(path).map_err(|error| history_error(path, "read", error))?;
    if entries.last().is_some_and(|previous| previous == entry) {
        return Ok(());
    }
    entries.push(entry.to_owned());
    if entries.len() > MAX_HISTORY_ENTRIES {
        entries.drain(..entries.len() - MAX_HISTORY_ENTRIES);
    }
    write_entries(path, &entries)
}

fn write_entries(path: &Path, entries: &[String]) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stamp = vibe_core::clock::now_nanos();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("vibehistory");
    let temporary = parent.join(format!(".{name}.{}.{stamp}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| history_error(path, "create replacement", error))?;
    let write_result = (|| {
        for entry in entries {
            serde_json::to_writer(&mut file, entry)
                .map_err(|error| format!("Prompt history serialization failed: {error}"))?;
            file.write_all(b"\n")
                .map_err(|error| history_error(path, "write replacement", error))?;
        }
        file.sync_all()
            .map_err(|error| history_error(path, "sync replacement", error))?;
        drop(file);
        fs::rename(&temporary, path).map_err(|error| history_error(path, "replace", error))?;
        sync_parent(parent);
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn sync_parent(parent: &Path) {
    #[cfg(unix)]
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
}

fn history_error(path: &Path, operation: &str, error: std::io::Error) -> String {
    format!(
        "Prompt history {operation} failed for `{}`: {error}",
        path.display()
    )
}

struct HistoryLock {
    file: File,
}

impl HistoryLock {
    fn acquire(history_path: &Path) -> Result<Self, String> {
        let parent = history_path.parent().unwrap_or_else(|| Path::new("."));
        let name = history_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("vibehistory");
        let path = parent.join(format!(".{name}.lock"));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.mode(0o600);
        }
        let file = options
            .open(path)
            .map_err(|error| history_error(history_path, "open lock", error))?;
        file.lock()
            .map_err(|error| history_error(history_path, "lock", error))?;
        Ok(Self { file })
    }
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn history_loads_json_strings_and_recovers_raw_lines() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("vibehistory");
        fs::write(&path, "\"valid\"\nnot json\n\"newest\"\n").expect("history fixture");
        assert_eq!(
            read_entries(&path).expect("history loads"),
            vec!["valid", "not json", "newest"]
        );
    }

    #[test]
    fn history_is_bounded_and_suppresses_consecutive_duplicates() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("vibehistory");
        for index in 0..105 {
            persist_entry(&path, &format!("entry {index}")).expect("history persists");
        }
        persist_entry(&path, "entry 104").expect("duplicate is harmless");
        let entries = read_entries(&path).expect("history reloads");
        assert_eq!(entries.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(entries.first().map(String::as_str), Some("entry 5"));
        assert_eq!(entries.last().map(String::as_str), Some("entry 104"));
    }

    #[test]
    fn concurrent_writers_merge_under_the_history_lock() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("vibehistory");
        let first_path = path.clone();
        let second_path = path.clone();
        let first = thread::spawn(move || persist_entry(&first_path, "first"));
        let second = thread::spawn(move || persist_entry(&second_path, "second"));
        first.join().expect("first worker").expect("first write");
        second.join().expect("second worker").expect("second write");
        let entries = read_entries(&path).expect("merged history");
        assert_eq!(entries.len(), 2);
        assert!(entries.contains(&"first".to_owned()));
        assert!(entries.contains(&"second".to_owned()));
    }

    #[tokio::test]
    async fn async_adapter_preserves_submission_order() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("vibehistory");
        let (mut history, load) = PromptHistory::open(path.clone());
        assert!(load.entries.is_empty());
        history.persist("first".to_owned());
        history.persist("second".to_owned());
        assert!(history.finish().await.is_empty());
        assert_eq!(
            read_entries(&path).expect("ordered history"),
            vec!["first", "second"]
        );
    }

    #[test]
    fn failed_replacement_preserves_the_existing_file_without_prompt_data_in_diagnostics() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let parent_file = temporary.path().join("not-a-directory");
        fs::write(&parent_file, "occupied").expect("parent fixture");
        let path = parent_file.join("vibehistory");
        let error = persist_entry(&path, "top secret prompt").expect_err("write must fail");
        assert!(!error.contains("top secret prompt"), "{error}");
        assert_eq!(
            fs::read_to_string(&parent_file).expect("fixture survives"),
            "occupied"
        );
    }
}
