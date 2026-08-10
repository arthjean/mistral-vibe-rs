//! The remote skill registry, ported dormant.
//!
//! The reference implements a paginated catalog client, an atomically staged
//! version store and two manifest scopes under `vibe/core/skills/registry/`,
//! and then never imports any of it: at the pinned commit the subtree is
//! reachable only from its own tests, and `experimental_enable_registry_skills`
//! has exactly one occurrence in the whole tree, its own declaration. The port
//! reproduces that state. The four modules carry the reference's safety rules,
//! the `store` and `manifest` corpus families measure them on every `cargo
//! test`, and [`published_skills`] is the one production door, opened only when
//! the experiment key is true and answering nothing until upstream publishes a
//! load lifecycle to reproduce.

pub mod client;
pub mod manifest;
pub mod models;
pub mod store;

#[cfg(test)]
mod client_tests;
#[cfg(test)]
mod models_tests;

use std::collections::BTreeMap;

use crate::extensions::SkillDefinition;

/// The skills an enabled registry experiment publishes into a catalog.
///
/// At the pinned commit the answer is none: no upstream code path loads
/// registry skills into a session, and open question 3 of the skills-parity
/// PRD records that the load lifecycle is unpublished rather than guessable.
/// The function exists so the app-server gate reaches the subtree through one
/// door, and a future lifecycle lands here rather than beside the key read.
/// Nothing here touches the store, so an enabled experiment with no registry
/// configured changes no catalog, creates no cache directory and constructs
/// no transport.
#[must_use]
pub fn published_skills() -> BTreeMap<String, SkillDefinition> {
    BTreeMap::new()
}
