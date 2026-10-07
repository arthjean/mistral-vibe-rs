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
pub mod registry;
pub mod schema;

#[cfg(test)]
mod builtins_tests;
#[cfg(test)]
mod search_tests;
#[cfg(test)]
mod skills_parity_tests;
#[cfg(test)]
mod skills_registry_parity_tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use crate::events::{ModelMessage, ModelToolCall};
use crate::extensions::{DiscoveryIssue, SkillDefinition};
use crate::matching::NameFilter;
use crate::tools::ToolExecutionOutput;

/// Where a skill came from, in the four-value vocabulary the wire's
/// `SkillSummary` declares: shipped with the binary, found on disk,
/// materialized from the remote registry, or contributed by a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    Builtin,
    Local,
    Registry,
    Plugin,
}

/// How widely a skill applies. Reference `_compute_search_paths` pairs every
/// root with one: a project harness root publishes `project`, a configured or
/// user root `global`, and a builtin keeps the model default, `global`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    Builtin,
    Global,
    Project,
}

/// The registry version a skill was loaded from (reference `RegistryRef`):
/// the concrete version on disk and, for an alias pin, the alias it follows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryRef {
    pub skill_id: String,
    pub version: i64,
    pub alias: Option<String>,
}

/// Where the registry's pins and materialized versions are read from: the
/// Vibe home holding the global manifest and the store, and the open project
/// roots whose `.vibe/skills.toml` pins win over the global ones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistrySources {
    pub vibe_home: PathBuf,
    pub project_roots: Vec<PathBuf>,
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
    /// The skill directories to walk with the scope each one publishes, in
    /// precedence order and already resolved and deduplicated by
    /// [`search_paths`]. The first root holding a name wins.
    pub roots: Vec<(PathBuf, SkillScope)>,
    /// `enabled_skills`, verbatim from the merged document.
    pub enabled: Vec<String>,
    /// `disabled_skills`, verbatim from the merged document.
    pub disabled: Vec<String>,
    /// Set while `experimental_enable_registry_skills` is on: the pinned
    /// registry skills are loaded after the walk, where a builtin or a disk
    /// skill of the same name wins (reference `_discover_registry_skills`).
    pub registry: Option<RegistrySources>,
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

/// The skill directories to walk, in reference order, resolved and
/// deduplicated, each with the scope it publishes.
///
/// Reference `_compute_search_paths` walks `config.skill_paths` first, then
/// every project root's `.vibe/skills` and `.agents/skills`, then
/// `~/.vibe/skills` and `~/.agents/skills`, keeping only directories and
/// deduplicating on the resolved path so a symlinked spelling of a root already
/// walked is walked once. A configured or user root publishes `global` and a
/// project root `project`, unless it resolves to a global root: a session
/// opened in the home directory reads `~/.vibe/skills` as a project root, and
/// it stays global there.
///
/// One root is this port's own and has no reference counterpart:
/// `{vibe_home}/extensions/skills` is where releases before this one read user
/// skills from, and it stays readable so an existing installation does not stop
/// loading on the day of the change. It ranks last, after both documented user
/// roots, so a name published in either of them wins, and it is global.
#[must_use]
pub fn search_paths(inputs: &SearchInputs<'_>) -> Vec<(PathBuf, SkillScope)> {
    let mut candidates = Vec::new();
    for entry in inputs.configured {
        candidates.push((
            anchor(entry, inputs.user_home, inputs.working_directory),
            SkillScope::Global,
        ));
    }
    for project in inputs.projects {
        candidates.push((project.join(".vibe").join("skills"), SkillScope::Project));
        candidates.push((project.join(".agents").join("skills"), SkillScope::Project));
    }
    candidates.push((inputs.vibe_home.join("skills"), SkillScope::Global));
    if let Some(home) = inputs.user_home {
        candidates.push((home.join(".agents").join("skills"), SkillScope::Global));
    }
    candidates.push((
        inputs.vibe_home.join("extensions").join("skills"),
        SkillScope::Global,
    ));

    let existing = candidates
        .into_iter()
        .filter(|(candidate, _)| candidate.is_dir())
        .map(|(candidate, scope)| {
            (
                std::fs::canonicalize(&candidate).unwrap_or(candidate),
                scope,
            )
        })
        .collect::<Vec<_>>();
    let global = existing
        .iter()
        .filter(|(_, scope)| *scope == SkillScope::Global)
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    let mut unique: Vec<(PathBuf, SkillScope)> = Vec::new();
    for (resolved, scope) in existing {
        if unique.iter().any(|(seen, _)| *seen == resolved) {
            continue;
        }
        let scope = if scope == SkillScope::Project && global.contains(&resolved) {
            SkillScope::Global
        } else {
            scope
        };
        unique.push((resolved, scope));
    }
    unique
}

/// Reference `_expand_paths`: a leading `~` becomes the home directory and a
/// relative entry is anchored, so both spellings name one directory rather than
/// one directory per process that reads them.
pub(crate) fn anchor(entry: &str, home: Option<&Path>, working_directory: &Path) -> PathBuf {
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

/// Reference `SkillManager.installed_skills`: every installed skill, one row
/// per name, scope and source, builtins aside and filters ignored.
///
/// Nothing is collapsed by name: a skill a project root and a user root both
/// hold is two rows, and so is a skill pinned globally and in the project,
/// because a browser manages each one separately. A disabled skill keeps its
/// row so it can be turned back on. The winners of discovery come first, then
/// the rows they shadow in root order, then the registry pins.
#[must_use]
pub fn installed_skills(
    discovery: &SkillDiscovery,
    builtin_names: &BTreeSet<String>,
) -> (Vec<SkillDefinition>, Vec<DiscoveryIssue>) {
    let mut issues = Vec::new();
    let walked = discovery
        .roots
        .iter()
        .map(|(directory, scope)| {
            let (skills, root_issues) =
                crate::extensions::skills_in_directory(directory, *scope, builtin_names);
            issues.extend(root_issues);
            skills
        })
        .collect::<Vec<_>>();
    let mut keys: BTreeSet<(String, SkillScope, SkillSource)> = BTreeSet::new();
    let mut rows = Vec::new();
    let mut winners = BTreeSet::new();
    for skills in &walked {
        for (name, skill) in skills {
            if winners.insert(name.clone()) {
                keys.insert((name.clone(), skill.scope, skill.source));
                rows.push(skill.clone());
            }
        }
    }
    for skills in &walked {
        for (name, skill) in skills {
            if keys.insert((name.clone(), skill.scope, skill.source)) {
                rows.push(skill.clone());
            }
        }
    }
    if let Some(sources) = &discovery.registry {
        let (pinned, pin_issues) = registry::loader::pinned_skills(sources);
        issues.extend(pin_issues);
        for skill in pinned {
            if keys.insert((skill.name.clone(), skill.scope, skill.source)) {
                rows.push(skill);
            }
        }
    }
    (rows, issues)
}

/// One skill as the wire's `SkillSummary` declares it: the body travels as
/// `prompt`, the registry provenance as `registry`, and the richer model
/// fields stay off the summary. A published skill is enabled and unlocked
/// (reference `_skill_summary` defaults,
/// `vibe/app_server/_projection.py:288-301`); [`installed_marks`] answers the
/// two flags a browser row carries instead.
#[must_use]
pub fn skill_summary(skill: &SkillDefinition) -> Value {
    json!({
        "name": skill.name,
        "description": skill.description,
        "prompt": skill.body,
        "userInvocable": skill.user_invocable,
        "source": skill.source,
        "scope": skill.scope,
        "registry": skill.registry,
        "enabled": true,
        "locked": false,
    })
}

/// The `(enabled, locked)` pair a browser row carries for one installed
/// skill (reference `project_installed_skill_summaries`).
///
/// `enabled` is whether the session loads the skill and `locked` whether a
/// toggle could change that: a plugin skill is managed by its plugin, an
/// allowlist decides alone and locks every row, a skill a pattern disables,
/// rather than its own name, stays disabled whatever a toggle writes, and so
/// does one only another layer than the writable one disables, since
/// `disabled_skills` concatenates across layers. `own` is the writable
/// layer's own `disabled_skills`.
#[must_use]
pub fn installed_marks(
    name: &str,
    source: SkillSource,
    allowed: &[String],
    disabled: &[String],
    own: &[String],
) -> (bool, bool) {
    if source == SkillSource::Plugin {
        return (true, true);
    }
    if !allowed.is_empty() {
        return (NameFilter::new(allowed).matches(name), true);
    }
    if disabled.is_empty() {
        return (true, false);
    }
    let denied = NameFilter::new(disabled).matches(name);
    let others = disabled
        .iter()
        .filter(|pattern| pattern.as_str() != name)
        .cloned()
        .collect::<Vec<_>>();
    let locked = NameFilter::new(&others).matches(name)
        || (denied && !own.iter().any(|entry| entry == name));
    (!denied, locked)
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

/// Whether a skill's optional `agents/openai.yaml` lets the model invoke it
/// on its own.
///
/// Reference `SkillManager._openai_allows_implicit_invocation` over
/// `load_openai_skill_metadata` (`vibe/core/skills/parser.py`): no file allows
/// it, `policy.allow_implicit_invocation` decides when it is set, and a file
/// that cannot be read or does not validate disables it, so a typo in the
/// policy never makes an explicit-only skill visible. The policy mapping is
/// strict (a key other than `allow_implicit_invocation` and `products` is
/// invalid, and neither is coerced); the document around it may carry
/// anything.
#[must_use]
pub fn openai_allows_implicit_invocation(skill_path: &Path) -> bool {
    openai_invocation_policy(skill_path).unwrap_or(false)
}

/// Where a skill's optional OpenAI metadata lives: `agents/openai.yaml`
/// beside its `SKILL.md` (reference `openai_skill_metadata_path`).
#[must_use]
pub fn openai_metadata_path(skill_path: &Path) -> Option<PathBuf> {
    Some(skill_path.parent()?.join("agents").join("openai.yaml"))
}

/// [`openai_allows_implicit_invocation`], answering why a file disabled
/// model invocation when it did: the reference records that reason as a
/// configuration issue on the metadata file.
///
/// # Errors
///
/// The reason a present metadata file could not be read or validated, which
/// disables model invocation.
pub fn openai_invocation_policy(skill_path: &Path) -> Result<bool, String> {
    let Some(metadata) = openai_metadata_path(skill_path) else {
        return Ok(true);
    };
    if !metadata.is_file() {
        return Ok(true);
    }
    let bytes = std::fs::read(&metadata)
        .map_err(|error| format!("cannot read agents/openai.yaml: {error}"))?;
    let document = parser::yaml_document(&String::from_utf8_lossy(&bytes))
        .map_err(|error| format!("agents/openai.yaml is not valid YAML: {error}"))?;
    let policy = match document {
        Value::Null => return Ok(true),
        Value::Object(mut mapping) => mapping.remove("policy"),
        _ => return Err("agents/openai.yaml must be a mapping".to_owned()),
    };
    let policy = match policy {
        None | Some(Value::Null) => return Ok(true),
        Some(Value::Object(policy)) => policy,
        Some(_) => return Err("agents/openai.yaml: `policy` must be a mapping".to_owned()),
    };
    if let Some(key) = policy
        .keys()
        .find(|key| *key != "allow_implicit_invocation" && *key != "products")
    {
        return Err(format!("agents/openai.yaml: unknown policy key `{key}`"));
    }
    let products_valid = match policy.get("products") {
        None => true,
        Some(Value::Array(products)) => products.iter().all(Value::is_string),
        Some(_) => false,
    };
    if !products_valid {
        return Err("agents/openai.yaml: `policy.products` must be a list of strings".to_owned());
    }
    match policy.get("allow_implicit_invocation") {
        None | Some(Value::Null) => Ok(true),
        Some(Value::Bool(allowed)) => Ok(*allowed),
        Some(_) => Err(
            "agents/openai.yaml: `policy.allow_implicit_invocation` must be a boolean".to_owned(),
        ),
    }
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
        message_id: None,
        reasoning_message_id: None,
        content: String::new(),
        reasoning: None,
        reasoning_payloads: Vec::new(),
        tool_calls: vec![ModelToolCall {
            id: call_id.clone(),
            name: "skill".to_owned(),
            arguments: arguments.clone(),
            presentation: None,
        }],
        keeps_empty_content: true,
    });
    messages.push(ModelMessage::Tool {
        call_id: call_id.clone(),
        content: output.model_text.clone(),
        is_error: false,
        name: "skill".to_owned(),
        result: None,
    });
    AppendedSkillCall {
        call_id,
        arguments,
        output,
    }
}
