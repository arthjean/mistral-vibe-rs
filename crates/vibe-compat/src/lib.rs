#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod baseline;
pub mod canonical;
pub mod differential;
pub mod matrix;
pub mod model;
pub mod oracle;
mod release4_contracts;
pub mod release5;
mod release5_contracts;
pub mod rust_recorder;
pub mod workspace;

pub const BASELINE_TOML: &str = include_str!("../../../compat/baseline.toml");
pub const MATRIX_TOML: &str = include_str!("../../../compat/capability-matrix.toml");
pub const DISCOVERED_TOML: &str = include_str!("../../../compat/discovered-surfaces.toml");
pub const SCENARIOS_TOML: &str = include_str!("../../../compat/scenarios.toml");
