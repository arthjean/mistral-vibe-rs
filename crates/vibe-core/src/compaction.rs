//! Turning a conversation that no longer fits into one that does.
//!
//! The reference splits compaction the way it split checkpoints: a pure half
//! that is total functions over strings and message lists, and an orchestrating
//! half that drives injected callables and never touches disk itself. This port
//! keeps the split, which is what lets the calculation ship and be measured
//! against a committed corpus before any provider work exists.
//!
//! [`tokens`] holds the arithmetic every budget decision rests on and
//! [`context`] the selection, the envelope and its parser.
//!
//! Reference: `vibe/core/compaction/` and `vibe/core/utils/tokens.py` at the
//! pinned commit.

pub mod context;
pub mod tokens;

#[cfg(test)]
mod compaction_parity_tests;
#[cfg(test)]
mod context_tests;
#[cfg(test)]
mod tokens_tests;
