//! The two skills the binary ships before any disk walk.
//!
//! Reference `BUILTIN_SKILLS` seeds `vibe` and `skill-creator` into every
//! catalog and reserves their names against disk skills. The set is seeded
//! here, in `vibe-core`, so the `skill` tool handler and the app-server
//! catalog read the same two entries.
//!
//! The bodies and descriptions are this repository's own prose: `NOTICE`
//! forbids shipping the reference's 40 589 bytes of builtin text, so both are
//! written originally against the same directive coverage, and
//! `skills_parity_tests` holds the divergence in place by failing the moment
//! either matches the reference digest recorded in the corpus.

use std::collections::BTreeMap;

use crate::extensions::SkillDefinition;
use crate::skills::{SkillScope, SkillSource};

/// The `vibe` body, with `__VERSION__` standing for the running version and
/// `__REPOSITORY__` for the published repository, so the documentation link
/// never names a release that is not running nor a location the manifest does
/// not declare. Both come from `[workspace.package]`, which stays the one
/// place either fact is written.
const VIBE_BODY: &str = include_str!("assets/vibe.md");
const SKILL_CREATOR_BODY: &str = include_str!("assets/skill_creator.md");

/// The builtin catalog, keyed by name, in the shape `discover_extensions`
/// seeds ahead of the disk walk. Both entries carry `source: builtin`, the
/// `global` scope the reference publishes for them, and no path.
#[must_use]
pub fn builtin_skills() -> BTreeMap<String, SkillDefinition> {
    [vibe(), skill_creator()]
        .into_iter()
        .map(|skill| (skill.name.clone(), skill))
        .collect()
}

fn vibe() -> SkillDefinition {
    SkillDefinition {
        name: "vibe".to_owned(),
        description: "The definitive self-reference for Mistral Vibe, the CLI agent currently \
                      running. Load it whenever the user asks about the tool itself, under any \
                      name (this CLI, this assistant, you), wants to inspect or change their \
                      setup, or asks why the agent behaved the way it did. It covers the Vibe \
                      home and `VIBE_HOME`, configuration files and their precedence, `VIBE_*` \
                      and `LOG_*` environment variables, models and providers, agents and \
                      subagents, skills, tools and the permission model, every slash command \
                      and CLI flag, hooks, MCP servers, connectors, trusted folders, `@` file \
                      mentions, logs, themes and voice. When unsure whether a command, flag or \
                      setting exists, load this skill instead of guessing."
            .to_owned(),
        license: None,
        compatibility: None,
        metadata: BTreeMap::new(),
        allowed_tools: Vec::new(),
        user_invocable: false,
        body: VIBE_BODY
            .replace("__REPOSITORY__", env!("CARGO_PKG_REPOSITORY"))
            .replace("__VERSION__", env!("CARGO_PKG_VERSION")),
        source: SkillSource::Builtin,
        scope: SkillScope::Global,
        path: None,
    }
}

fn skill_creator() -> SkillDefinition {
    SkillDefinition {
        name: "skill-creator".to_owned(),
        description: "Load before creating, editing or removing a Vibe skill, or before \
                      touching any `SKILL.md` under a skills directory. It documents the \
                      frontmatter schema, the five discovery locations and their precedence, \
                      the reserved builtin names, support files, and how permission prompts \
                      apply to skill writes."
            .to_owned(),
        license: None,
        compatibility: None,
        metadata: BTreeMap::new(),
        allowed_tools: Vec::new(),
        user_invocable: true,
        body: SKILL_CREATOR_BODY.to_owned(),
        source: SkillSource::Builtin,
        scope: SkillScope::Global,
        path: None,
    }
}
