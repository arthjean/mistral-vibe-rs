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
pub mod bootstrap;
pub mod checkpoints;
mod child;
pub mod compaction;
pub mod config;
pub mod continuity;
pub mod engine;
pub mod events;
pub mod extensions;
pub mod images;
pub mod integrations;
pub mod matching;
pub mod mcp;
pub mod middleware;
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
pub mod shell;
pub mod storage;
pub mod telemetry;
pub mod text;
pub mod tools;
pub mod updates;
pub mod workspace;
