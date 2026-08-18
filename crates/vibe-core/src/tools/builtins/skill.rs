//! The `skill` tool: loading a named skill's instructions into the turn.
//!
//! A skill reaches the model two ways, and both land here: the model calls the
//! tool, or the operator types `/name` and the engine appends the same call and
//! result pair. [`SkillInvocationResolver`] is the second path, and it records
//! what it loaded so a repeat is acknowledged rather than rendered again.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::{MAX_LISTED_SKILL_FILES, MAX_LISTED_SKILLS, declared_document};
use crate::extensions::{DiscoveryRoots, SkillDefinition, discover_extensions};
use crate::schema::{ObjectSchema, Property};
use crate::tools::{
    ToolAvailability, ToolError, ToolExecutionOutput, ToolPresentationKind, ToolSource, ToolSpec,
    reference_text,
};

/// Directive coverage for `skill`.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The name comes from the advertised skill list | "named in available_skills" |
/// | Loading a skill injects its instructions into the conversation | "loads its instructions into this conversation" |
/// | The loaded instructions are followed for the rest of the task | "Follow them for the rest of the task" |
pub(super) fn skill_spec() -> ToolSpec {
    ToolSpec {
        name: "skill".to_owned(),
        description: "Load a skill named in available_skills, which loads its instructions into \
                      this conversation. Follow them for the rest of the task."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "name",
                Property::string()
                    .described("The name of the skill, as advertised in available_skills"),
            )
            .build(),
        output_schema: None,
        config: declared_document("skill"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

pub(super) fn run_skill(
    roots: &DiscoveryRoots,
    loaded: &Mutex<BTreeMap<String, BTreeSet<String>>>,
    session_id: &str,
    name: &str,
) -> Result<ToolExecutionOutput, ToolError> {
    let catalog = discover_extensions(
        roots,
        BTreeMap::new(),
        crate::skills::builtins::builtin_skills(),
        BTreeMap::new(),
    );
    let Some(skill) = catalog.skills.get(name) else {
        // An unknown name is answered with what does exist: a model that
        // guessed the name can correct itself without another round trip.
        let available = catalog
            .skills
            .keys()
            .take(MAX_LISTED_SKILLS)
            .cloned()
            .collect::<Vec<_>>();
        let listed = if available.is_empty() {
            "none".to_owned()
        } else {
            available.join(", ")
        };
        // The catalog's issue list is answered with rather than discarded: a
        // skill that is missing because its own file would not parse is
        // otherwise indistinguishable from one that was never written, and this
        // error is the only surface a mid-turn tool call reads.
        let unloadable = catalog
            .issues
            .iter()
            .filter(|issue| issue.mechanism == "skills")
            .take(MAX_LISTED_SKILLS)
            .map(|issue| format!("{}: {}", issue.path.display(), issue.message))
            .collect::<Vec<_>>();
        let unloadable = if unloadable.is_empty() {
            String::new()
        } else {
            format!(
                "; {} skill file(s) could not be loaded: {}",
                unloadable.len(),
                unloadable.join("; ")
            )
        };
        return Err(ToolError::Execution(format!(
            "skill `{name}` was not found; available skills: {listed}{unloadable}"
        )));
    };
    let directory = skill_directory(skill);
    // A skill already loaded in this conversation is acknowledged rather than
    // rendered again: the instructions are still in the transcript, and paying
    // for them twice buys nothing.
    let already_loaded = {
        let mut loaded = loaded
            .lock()
            .map_err(|_| ToolError::Execution("the skill ledger lock is poisoned".to_owned()))?;
        !loaded
            .entry(session_id.to_owned())
            .or_default()
            .insert(skill.name.clone())
    };
    Ok(skill_output(skill, directory.as_deref(), already_loaded))
}

/// The output a skill load answers with, shared by the `skill` tool and the
/// synthetic pair a slash invocation appends, so both paths deliver the same
/// bytes for the same skill.
pub(super) fn skill_output(
    skill: &SkillDefinition,
    directory: Option<&Path>,
    already_loaded: bool,
) -> ToolExecutionOutput {
    let content = if already_loaded {
        format!(
            "The skill `{}` was already loaded earlier in this conversation; reuse those \
             instructions.",
            skill.name
        )
    } else {
        render_skill(skill, directory)
    };
    let directory_field = directory.map(|path| path.to_string_lossy().replace('\\', "/"));
    ToolExecutionOutput::new(reference_text::joined(&[
        ("name", skill.name.clone()),
        ("content", content.clone()),
        (
            "skill_dir",
            reference_text::optional(directory_field.clone()),
        ),
    ]))
    .displayed_as(json!({"kind": "skill", "name": skill.name}))
    .typed(json!({
        "name": skill.name,
        "content": content,
        "skill_dir": directory_field,
    }))
}

/// Resolves a slash invocation against the same catalog and loaded ledger the
/// `skill` tool answers from.
///
/// Reference `parse_skill_command`: the trimmed prompt's first word past the
/// `/` names the skill case-insensitively, and a name that is unknown or not
/// user invocable resolves to nothing. A resolved skill is recorded in the
/// ledger, so a later `skill` tool call is acknowledged instead of rendered
/// again.
pub(super) struct SkillInvocationResolver {
    pub(super) roots: DiscoveryRoots,
    pub(super) loaded: Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>,
    pub(super) session_id: String,
}

impl crate::skills::InvokedSkillResolver for SkillInvocationResolver {
    fn resolve(&self, prompt: &str) -> Option<crate::skills::InvokedSkill> {
        // The engine asks this question of every user message, and discovery
        // walks five roots parsing every `SKILL.md` it finds. A prompt that
        // cannot name a skill is answered before that walk is paid for, which
        // is also how the reference behaves: its catalog is built once with the
        // session, not once per turn.
        if !prompt.trim_start().starts_with('/') {
            return None;
        }
        let catalog = discover_extensions(
            &self.roots,
            BTreeMap::new(),
            crate::skills::builtins::builtin_skills(),
            BTreeMap::new(),
        );
        let parsed = crate::skills::parse_skill_command(&catalog.skills, prompt)?;
        let skill = catalog.skills.get(&parsed.name)?;
        if let Ok(mut loaded) = self.loaded.lock() {
            loaded
                .entry(self.session_id.clone())
                .or_default()
                .insert(skill.name.clone());
        }
        let directory = skill_directory(skill);
        Some(crate::skills::InvokedSkill {
            name: skill.name.clone(),
            loaded: skill_output(skill, directory.as_deref(), false),
            already_loaded: skill_output(skill, directory.as_deref(), true),
        })
    }
}

/// The directory a skill's files sit in, or [`None`] when it has none on disk.
///
/// A skill declared without a file on disk has no base directory, and the
/// reference then omits the two lines that would otherwise name an empty path.
pub(super) fn skill_directory(skill: &SkillDefinition) -> Option<PathBuf> {
    let base = skill.path.as_deref()?.parent()?;
    base.is_dir().then(|| base.to_path_buf())
}

/// The block the model reads back, carrying the skill body, its base directory
/// and a sample of the files that sit next to it.
///
/// The walk is recursive and the names are relative to the base, which is what
/// makes a skill shipping `references/api.md` advertise that path rather than
/// only its top-level directory.
pub(super) fn render_skill(skill: &SkillDefinition, base: Option<&Path>) -> String {
    let file_lines = base
        .map(skill_files)
        .unwrap_or_default()
        .iter()
        .map(|file| format!("<file>{file}</file>"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut lines = vec![
        crate::skills::skill_content_marker(&skill.name),
        format!("# Skill: {}", skill.name),
        String::new(),
        skill.body.trim().to_owned(),
        String::new(),
    ];
    if let Some(base) = base {
        lines.push(format!("Base directory for this skill: {}", base.display()));
        lines.push("Relative paths in this skill resolve against that base directory.".to_owned());
    }
    lines.extend([
        "Note: the file list below is a sample.".to_owned(),
        String::new(),
        "<skill_files>".to_owned(),
        file_lines,
        "</skill_files>".to_owned(),
        "</skill_content>".to_owned(),
    ]);
    lines.join("\n")
}

/// The files that ship with a skill, sorted, without its own `SKILL.md`, and
/// capped so a large bundle cannot flood the conversation.
pub(super) fn skill_files(base: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let mut pending = vec![base.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if entry.file_name() != "SKILL.md"
                && let Ok(relative) = path.strip_prefix(base)
            {
                names.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    names.sort();
    names.truncate(MAX_LISTED_SKILL_FILES);
    names
}
