//! The skills subsystem, measured before it is built.
//!
//! This module currently holds only the differential oracle for the skills
//! surface: `skills_parity_tests` replays the committed corpus captured by
//! `scripts/parity/skills.py` against whatever the port answers today. The
//! implementation itself still lives across `extensions.rs` and the tool
//! handlers; it moves here as the skills-parity PRD lands, epic by epic, and
//! each landing shrinks the divergence ledger the replay enforces.

#[cfg(test)]
mod skills_parity_tests;
