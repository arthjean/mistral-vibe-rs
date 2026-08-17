#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_in_result,
        clippy::unwrap_used
    )
)]

mod atomic_file;
pub mod auth;
pub mod bootstrap;
pub mod checkpoints;
mod child;
pub mod compaction;
pub mod config;
pub mod continuity;
pub mod engine;
pub mod events;
pub mod experiments;
#[cfg(test)]
mod experiments_parity_tests;
pub mod extensions;
pub mod identity;
pub mod images;
pub mod integrations;
pub mod matching;
pub mod mcp;
pub mod middleware;
pub mod observability;
pub mod parity;
pub mod platform;
pub mod policy;
pub mod process;
pub mod prompt;
pub mod provider;
mod pty;
mod remote_tools;
pub mod schema;
pub mod scratchpad;
pub mod session_id;
#[cfg(test)]
mod setup_auth_parity_tests;
pub mod shell;
pub mod skills;
pub mod storage;
pub mod telemetry;
pub mod text;
pub mod tools;
pub mod tracing;
pub mod updates;
pub mod workspace;
