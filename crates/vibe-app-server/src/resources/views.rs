//! The answer shapes a resource dispatch publishes.
//!
//! A method either reads state or moves it, and the two publish different
//! shapes: a read carries the state alone, a mutation also carries what it
//! diagnosed and, for the six that move runtime state, the runtime the server
//! composes afterward. Building them here keeps the service's methods about
//! what they do rather than about how the answer is spelled.

use super::*;

pub(super) fn read_only<const N: usize>(entries: [(&str, Value); N]) -> ResourceDispatch {
    ResourceDispatch {
        result: entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
        signals: ResourceSignals::default(),
    }
}

/// A mutation that moved runtime state, with whatever it could not do cleanly.
///
/// The answer carries only what the method's own response declares. The runtime
/// is filled in by the server, which is the only owner able to compose it, and
/// each diagnostic is published as its own `warning`, which is how the reference
/// splits an answer from what a client must be told about.
pub(super) fn runtime_mutation<const N: usize>(
    entries: [(&str, Value); N],
    diagnostics: Vec<String>,
) -> ResourceDispatch {
    ResourceDispatch {
        result: entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
        signals: ResourceSignals {
            runtime_updated: true,
            warnings: diagnostics,
            auth_required: Vec::new(),
            integrations: None,
        },
    }
}
