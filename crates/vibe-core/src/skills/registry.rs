//! The remote skill registry.
//!
//! Reference `vibe/core/skills/registry/`: a paginated catalog client, an
//! atomically staged version store, two manifest scopes, the ledger the shared
//! store is pruned against, and the service a skills browser and a session
//! drive. Everything is gated on `experimental_enable_registry_skills`: while
//! it is off a session loads no pin and runs no sync, which is the
//! reference's behavior. [`loader`] is what a session reads (the pins
//! materialized on disk, a builtin or disk skill of the same name winning),
//! [`sync`] what a session start runs, and [`pins`] and [`service`] what the
//! `skills/*` methods call.

pub mod client;
pub mod ledger;
pub mod loader;
pub mod manifest;
pub mod models;
pub mod pins;
pub mod service;
pub mod store;
pub mod sync;

#[cfg(test)]
mod client_tests;
#[cfg(test)]
mod models_tests;
