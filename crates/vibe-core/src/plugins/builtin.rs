//! The `vibe` plugin the binary ships.
//!
//! Reference `vibe/plugins/builtins/vibe/`, a native package installed beside
//! the reference's sources. The manifest is reproduced, since its digest is
//! published; the four skills are this repository's own prose (`NOTICE`), the
//! `vibe` and `skill-creator` bodies shared with the legacy builtins in
//! [`crate::skills::builtins`]. A compiled binary has no package directory, so
//! the tree is written under the vibe home and rewritten only when it differs.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::extensions::SkillDefinition;
use crate::hooks::python_json_dumps;

/// The directory under the vibe home the built-in plugins are written to.
pub const BUILTIN_PLUGINS_DIRECTORY: &str = "builtin-plugins";

const MANIFEST: &str = include_str!("builtin/vibe/plugin.json");
const CREATE_PLUGIN: &str = include_str!("builtin/vibe/skills/create-plugin/SKILL.md");
const WORKTREE: &str = include_str!("builtin/vibe/skills/worktree/SKILL.md");

/// Writes the built-in plugins under `vibe_home` and answers the root that
/// holds them, the one a resolver scans as its builtin root.
///
/// # Errors
///
/// A file that could not be written.
pub fn materialize_builtin_plugins(vibe_home: &Path) -> io::Result<PathBuf> {
    let root = vibe_home.join(BUILTIN_PLUGINS_DIRECTORY);
    let plugin = root.join("vibe");
    let legacy = crate::skills::builtins::builtin_skills();
    let mut files = vec![
        (PathBuf::from("plugin.json"), MANIFEST.to_owned()),
        (skill_file("create-plugin"), CREATE_PLUGIN.to_owned()),
        (skill_file("worktree"), WORKTREE.to_owned()),
    ];
    for name in ["skill-creator", "vibe"] {
        if let Some(skill) = legacy.get(name) {
            files.push((skill_file(name), render_skill(skill)));
        }
    }
    for (relative, content) in files {
        write_if_changed(&plugin.join(relative), &content)?;
    }
    Ok(root)
}

fn skill_file(name: &str) -> PathBuf {
    Path::new("skills").join(name).join("SKILL.md")
}

/// A legacy builtin as a `SKILL.md` the plugin ships.
fn render_skill(skill: &SkillDefinition) -> String {
    let quoted = |text: &str| python_json_dumps(&Value::String(text.to_owned()));
    let mut lines = vec![
        "---".to_owned(),
        format!("name: {}", quoted(&skill.name)),
        format!("description: {}", quoted(&skill.description)),
    ];
    if !skill.user_invocable {
        lines.push("user-invocable: false".to_owned());
    }
    lines.extend(["---".to_owned(), String::new(), skill.body.clone()]);
    let mut text = lines.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn write_if_changed(path: &Path, content: &str) -> io::Result<()> {
    if fs::read_to_string(path).is_ok_and(|current| current == content) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)
}

#[cfg(test)]
mod builtin_tests;
