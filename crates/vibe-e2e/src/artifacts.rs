//! What a failed trial leaves under `$CARGO_TARGET_TMPDIR/e2e-artifacts/`:
//! enough to diagnose it without rerunning, within a fixed size.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::world::World;

/// The most a failed trial may leave behind.
pub(crate) const BUDGET: usize = 20 * 1024 * 1024;

/// The directory name a trial's artifacts use.
pub(crate) fn directory_name(trial: &str) -> String {
    trial
        .replace("::", "__")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Whether a variable's value is a credential and must not be written.
pub(crate) fn is_secret(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET"]
        .iter()
        .any(|word| name.contains(word))
}

struct Writer {
    left: usize,
}

impl Writer {
    fn put(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let kept = bytes.len().min(self.left);
        self.left -= kept;
        fs::write(path, &bytes[..kept])
    }

    fn copy_tree(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        let Ok(entries) = fs::read_dir(from) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let kind = entry.file_type()?;
            let target = to.join(entry.file_name());
            if kind.is_dir() {
                self.copy_tree(&entry.path(), &target)?;
            } else if kind.is_file() {
                let bytes = fs::read(entry.path()).unwrap_or_default();
                self.put(&target, &bytes)?;
            }
        }
        Ok(())
    }
}

/// A sized listing of the world, one `mode size path` line per entry.
fn listing(root: &Path) -> String {
    let mut lines = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                format!("{:o}", metadata.permissions().mode() & 0o7777)
            };
            #[cfg(not(unix))]
            let mode = if metadata.permissions().readonly() {
                "r".to_owned()
            } else {
                "rw".to_owned()
            };
            let relative = path.strip_prefix(root).unwrap_or(&path);
            let kind = if metadata.is_dir() { "/" } else { "" };
            lines.push(format!(
                "{mode:>5} {:>10} {}{kind}",
                metadata.len(),
                relative.display()
            ));
            if metadata.is_dir() {
                pending.push(path);
            }
        }
    }
    lines.join("\n") + "\n"
}

fn world_artifacts(writer: &mut Writer, directory: &Path, world: &World) -> io::Result<()> {
    let journal: Vec<Value> = world
        .platform()
        .journal()
        .iter()
        .map(|e| e.to_json())
        .collect();
    writer.put(
        &directory.join("journal.json"),
        serde_json::to_string_pretty(&journal)
            .unwrap_or_default()
            .as_bytes(),
    )?;
    let egress = world.egress().join("\n");
    writer.put(&directory.join("sentinel.log"), egress.as_bytes())?;
    for (index, record) in world.records().iter().enumerate() {
        let stem = format!("child-{index}-{}", record.label);
        let stdout = record.stdout.lock().map(|b| b.clone()).unwrap_or_default();
        let stderr = record.stderr.lock().map(|b| b.clone()).unwrap_or_default();
        writer.put(&directory.join(format!("{stem}.stdout")), &stdout)?;
        writer.put(&directory.join(format!("{stem}.stderr")), &stderr)?;
        let env: String = record
            .env
            .iter()
            .map(|(name, value)| {
                let value = if is_secret(name) { "<redacted>" } else { value };
                format!("{name}={value}\n")
            })
            .collect();
        writer.put(
            &directory.join(format!("{stem}.env")),
            format!("pid {}\n{env}", record.pid).as_bytes(),
        )?;
    }
    let config = fs::read(world.vibe_home().join("config.toml")).unwrap_or_default();
    writer.put(&directory.join("config.toml"), &config)?;
    writer.put(
        &directory.join("tree.txt"),
        listing(world.root()).as_bytes(),
    )?;
    writer.copy_tree(&world.vibe_home().join("logs"), &directory.join("logs"))
}

/// Writes the artifacts of a failed trial into `directory`, replacing what a
/// previous run left there.
pub(crate) fn write(directory: &Path, worlds: &[World], failures: &[String]) -> io::Result<()> {
    if directory.exists() {
        fs::remove_dir_all(directory)?;
    }
    fs::create_dir_all(directory)?;
    let mut writer = Writer { left: BUDGET };
    writer.put(
        &directory.join("failure.txt"),
        failures.join("\n\n").as_bytes(),
    )?;
    match worlds {
        [world] => world_artifacts(&mut writer, directory, world)?,
        worlds => {
            for (index, world) in worlds.iter().enumerate() {
                world_artifacts(
                    &mut writer,
                    &directory.join(format!("world-{index}")),
                    world,
                )?;
            }
        }
    }
    Ok(())
}
