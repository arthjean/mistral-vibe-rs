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

pub mod parser;
pub mod schema;

#[cfg(test)]
mod search_tests;
#[cfg(test)]
mod skills_parity_tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use crate::extensions::SkillDefinition;
use crate::matching::NameFilter;

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
