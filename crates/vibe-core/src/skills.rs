//! The skills subsystem, measured before it is built.
//!
//! `skills_parity_tests` replays the corpus captured by
//! `scripts/parity/skills.py` against whatever the port answers today. The
//! implementation is moving here epic by epic as the skills-parity PRD lands:
//! [`parser`] and [`schema`] now own the frontmatter contract, and this root
//! owns the source and scope vocabularies plus the wire projection. Discovery
//! still lives in `extensions.rs`, and each landing shrinks the divergence
//! ledger the replay enforces.

pub mod parser;
pub mod schema;

#[cfg(test)]
mod skills_parity_tests;

use serde::Serialize;
use serde_json::{Value, json};

use crate::extensions::SkillDefinition;

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
