//! The skills subsystem, measured before it is built.
//!
//! `skills_parity_tests` replays the corpus captured by
//! `scripts/parity/skills.py` against whatever the port answers today. The
//! implementation is moving here epic by epic as the skills-parity PRD lands:
//! [`parser`] and [`schema`] own the frontmatter contract, this root owns the
//! source and scope vocabularies, the search roots, the filter and the wire
//! projection. The walk itself still lives in `extensions.rs`, which reads
//! [`SkillDiscovery`] for both, and each landing shrinks the divergence ledger
//! the replay enforces.

pub mod builtins;
pub mod parser;
pub mod schema;

#[cfg(test)]
mod builtins_tests;
#[cfg(test)]
mod search_tests;
#[cfg(test)]
mod skills_parity_tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use crate::events::{ModelMessage, ModelToolCall};
use crate::extensions::SkillDefinition;
use crate::matching::NameFilter;
use crate::tools::ToolExecutionOutput;

/// Where a skill came from, in the three-value vocabulary the wire's
/// `SkillSummary` declares: shipped with the binary, found on disk, or
/// materialized from the remote registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    Builtin,
    Local,
    Registry,
}

/// How widely a skill applies. At the pinned reference commit every disk
/// skill is published as `global`, project roots included; the vocabulary is
/// carried whole so the model matches the reference's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    Builtin,
    Global,
    Project,
}

/// Where discovery looks for skills and what it publishes once it has looked.
///
/// Reference `SkillManager` reads the three keys separately, from
/// `_compute_search_paths` and `_apply_filters`; they travel together here
/// because `discover_extensions` is the single place that answers with a
/// catalog, and a filter applied anywhere else would let `skills/list` and the
/// `skill` tool disagree about what exists.
#[derive(Debug, Clone, Default)]
pub struct SkillDiscovery {
    /// The skill directories to walk, in precedence order and already resolved
    /// and deduplicated by [`search_paths`]. The first root holding a name
    /// wins.
    pub roots: Vec<PathBuf>,
    /// `enabled_skills`, verbatim from the merged document.
    pub enabled: Vec<String>,
    /// `disabled_skills`, verbatim from the merged document.
    pub disabled: Vec<String>,
}

/// The inputs [`search_paths`] resolves the roots from.
///
/// Every one of them is passed in rather than read from the environment, so a
/// test drives the same code the session does over a scratch tree.
#[derive(Debug, Clone, Copy)]
pub struct SearchInputs<'a> {
    /// `skill_paths` entries, verbatim: `~` expansion and anchoring happen
    /// here, where the home and the working directory are known.
    pub configured: &'a [String],
    /// The project directories a workspace contributes. An untrusted workspace
    /// contributes none: only the caller knows the trust verdict, and the trust
    /// gate lives with it.
    pub projects: &'a [PathBuf],
    /// The Vibe home, which `skills` and the legacy `extensions/skills` hang
    /// off.
    pub vibe_home: &'a Path,
    /// The operator's home, which `.agents/skills` hangs off. Reference
    /// `AGENTS_HOME` reads `Path.home()` and honors no override.
    pub user_home: Option<&'a Path>,
    /// What a relative `skill_paths` entry is anchored on.
    pub working_directory: &'a Path,
}

/// The skill directories to walk, in reference order, resolved and deduplicated.
///
/// Reference `_compute_search_paths` walks `config.skill_paths` first, then
/// every project root's `.vibe/skills` and `.agents/skills`, then
/// `~/.vibe/skills` and `~/.agents/skills`, keeping only directories and
/// deduplicating on the resolved path so a symlinked spelling of a root already
/// walked is walked once.
///
/// One root is this port's own and has no reference counterpart:
/// `{vibe_home}/extensions/skills` is where releases before this one read user
/// skills from, and it stays readable so an existing installation does not stop
/// loading on the day of the change. It ranks last, after both documented user
/// roots, so a name published in either of them wins.
#[must_use]
pub fn search_paths(inputs: &SearchInputs<'_>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for entry in inputs.configured {
        candidates.push(anchor(entry, inputs.user_home, inputs.working_directory));
    }
    for project in inputs.projects {
        candidates.push(project.join(".vibe").join("skills"));
        candidates.push(project.join(".agents").join("skills"));
    }
    candidates.push(inputs.vibe_home.join("skills"));
    if let Some(home) = inputs.user_home {
        candidates.push(home.join(".agents").join("skills"));
    }
    candidates.push(inputs.vibe_home.join("extensions").join("skills"));

    let mut unique: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        if !candidate.is_dir() {
            continue;
        }
        let resolved = std::fs::canonicalize(&candidate).unwrap_or(candidate);
        if !unique.contains(&resolved) {
            unique.push(resolved);
        }
    }
    unique
}

/// Reference `_expand_paths`: a leading `~` becomes the home directory and a
/// relative entry is anchored, so both spellings name one directory rather than
/// one directory per process that reads them.
fn anchor(entry: &str, home: Option<&Path>, working_directory: &Path) -> PathBuf {
    let path = Path::new(entry);
    let expanded = path.strip_prefix("~").map_or_else(
        |_| path.to_path_buf(),
        |rest| home.map_or_else(|| path.to_path_buf(), |home| home.join(rest)),
    );
    if expanded.is_absolute() {
        expanded
    } else {
        working_directory.join(expanded)
    }
}

/// Narrows a discovered catalog to what the configuration publishes.
///
/// Reference `_apply_filters`: `enabled_skills` decides alone when it carries
/// an entry and `disabled_skills` is not consulted at all, even when it names a
/// skill the allowlist matched. The emptiness test reads the configured list
/// rather than the compiled filter, so an `enabled_skills` holding only an
/// uncompilable `re:` entry publishes nothing instead of publishing everything.
pub fn apply_filters(skills: &mut BTreeMap<String, SkillDefinition>, discovery: &SkillDiscovery) {
    if !discovery.enabled.is_empty() {
        let filter = NameFilter::new(&discovery.enabled);
        skills.retain(|name, _| filter.matches(name));
        return;
    }
    if !discovery.disabled.is_empty() {
        let filter = NameFilter::new(&discovery.disabled);
        skills.retain(|name, _| !filter.matches(name));
    }
}

/// One skill as the wire's `SkillSummary` declares it: the body travels as
/// `prompt`, and the richer model fields stay off the summary, which carries
/// exactly these five.
#[must_use]
pub fn skill_summary(skill: &SkillDefinition) -> Value {
    json!({
        "name": skill.name,
        "description": skill.description,
        "prompt": skill.body,
        "userInvocable": skill.user_invocable,
        "source": skill.source,
    })
}

/// A slash prompt resolved to the skill it invokes.
///
/// Reference `ParsedSkillCommand`: the resolved name, the skill's body, and
/// whatever text followed the first word, which stays part of the operator's
/// message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSkillCommand {
    pub name: String,
    pub content: String,
    pub extra_instructions: Option<String>,
}

/// Resolves a prompt against a published catalog.
///
/// Reference `parse_skill_command`: the trimmed prompt must start with `/`,
/// its first word names the skill lowercased, and a name that is unknown or
/// not user invocable resolves to nothing, leaving the prompt an ordinary
/// message. The text past the first word keeps its internal spacing and loses
/// the leading run, which is what the reference's two-way split does.
#[must_use]
pub fn parse_skill_command(
    skills: &BTreeMap<String, SkillDefinition>,
    prompt: &str,
) -> Option<ParsedSkillCommand> {
    let rest = prompt.trim().strip_prefix('/')?.trim_start();
    let first = rest.split_whitespace().next()?;
    let name = first.to_lowercase();
    let skill = skills.get(&name)?;
    if !skill.user_invocable {
        return None;
    }
    let extra = rest[first.len()..].trim_start();
    Some(ParsedSkillCommand {
        name,
        content: skill.body.clone(),
        extra_instructions: (!extra.is_empty()).then(|| extra.to_owned()),
    })
}

/// The opening tag a rendered skill body starts with, which is also the
/// dedup marker: reference `skill_content_marker` searches the stored tool
/// messages for it to decide whether a skill is already loaded.
#[must_use]
pub fn skill_content_marker(name: &str) -> String {
    format!("<skill_content name=\"{name}\">")
}

/// What a slash invocation delivers, resolved once and carrying both possible
/// answers: the rendered body for a first load and the acknowledgment for a
/// repeat. Which one is appended is decided against the conversation, by
/// [`append_invoked_skill`], because only the caller holds the history.
#[derive(Debug, Clone)]
pub struct InvokedSkill {
    pub name: String,
    /// The full rendering, exactly what the `skill` tool answers on a first
    /// load.
    pub loaded: ToolExecutionOutput,
    /// The already-loaded acknowledgment, answered when the marker is found.
    pub already_loaded: ToolExecutionOutput,
}

/// Answers whether a prompt is a skill invocation.
///
/// Reference `parse_skill_command`: the trimmed prompt must start with `/`,
/// the first word names the skill case-insensitively, and a name that is
/// unknown or not user invocable resolves to nothing, leaving the prompt an
/// ordinary message.
pub trait InvokedSkillResolver: Send + Sync {
    fn resolve(&self, prompt: &str) -> Option<InvokedSkill>;
}

/// Whether the conversation already carries the skill's rendered body.
///
/// Reference `_skill_already_loaded` reads role, tool name and marker; the
/// port's tool message carries no name, so the `skill` calls are joined from
/// the assistant messages through their call ids before the marker is sought.
#[must_use]
pub fn skill_already_loaded(messages: &[ModelMessage], name: &str) -> bool {
    let marker = skill_content_marker(name);
    let skill_calls = messages
        .iter()
        .filter_map(|message| match message {
            ModelMessage::Assistant { tool_calls, .. } => Some(tool_calls.iter()),
            _ => None,
        })
        .flatten()
        .filter(|call| call.name == "skill")
        .map(|call| call.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    messages.iter().any(|message| {
        matches!(
            message,
            ModelMessage::Tool {
                call_id, content, ..
            } if skill_calls.contains(call_id.as_str()) && content.contains(&marker)
        )
    })
}

/// The synthetic call the pair carries, handed back so the caller can emit the
/// matching engine events.
#[derive(Debug, Clone)]
pub struct AppendedSkillCall {
    pub call_id: String,
    /// The encoded arguments, the JSON object `{"name": "<skill>"}`.
    pub arguments: String,
    /// The output the tool message carries: the rendering on a first load and
    /// the acknowledgment on a repeat.
    pub output: ToolExecutionOutput,
}

/// Appends the synthetic `skill` call pair the reference's
/// `_inject_invoked_skill` writes: an assistant message whose only content is
/// one `skill` tool call, then the tool message carrying the selected result.
///
/// The already-loaded decision is made here, against the messages as they
/// stand, so a second invocation is acknowledged rather than rendered again.
pub fn append_invoked_skill(
    messages: &mut Vec<ModelMessage>,
    invoked: &InvokedSkill,
) -> AppendedSkillCall {
    let output = if skill_already_loaded(messages, &invoked.name) {
        invoked.already_loaded.clone()
    } else {
        invoked.loaded.clone()
    };
    let call_id = crate::session_id::generate_session_id(None);
    let arguments = json!({"name": invoked.name}).to_string();
    messages.push(ModelMessage::Assistant {
        content: String::new(),
        reasoning: None,
        reasoning_signature: None,
        reasoning_state: Vec::new(),
        tool_calls: vec![ModelToolCall {
            id: call_id.clone(),
            name: "skill".to_owned(),
            arguments: arguments.clone(),
        }],
    });
    messages.push(ModelMessage::Tool {
        call_id: call_id.clone(),
        content: output.model_text.clone(),
        is_error: false,
    });
    AppendedSkillCall {
        call_id,
        arguments,
        output,
    }
}
