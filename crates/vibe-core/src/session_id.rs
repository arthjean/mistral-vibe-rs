//! The session identity, whose trailing segment survives a rotation.
//!
//! An identifier is UUID-shaped, `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, and
//! splits into two parts that are minted differently: the first four segments
//! are always fresh, and the last one is carried over from the identifier being
//! replaced. That last segment is what makes the sessions a compaction leaves
//! on disk recognizable as one another's continuation, since nothing renames
//! the directories they were written under.
//!
//! Every other identifier this workspace mints, a UUID or a random hexadecimal
//! token, draws on [`random_bytes`] here, which owns the one policy for a
//! random source that refuses.
//!
//! Reference: `vibe/core/session/session_id.py` at the pinned commit.

use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest as _, Sha256};

use crate::text::hex_encode;

#[cfg(test)]
mod session_id_tests;

/// The separator the shape is built from, and the one a suffix is read back
/// across.
const SEGMENT_SEPARATOR: char = '-';

/// Mints a UUID-shaped identifier.
///
/// `suffix` becomes the trailing segment verbatim, which is how a rotation
/// keeps the stable part of the identity it replaces; without one, or with an
/// empty one, the whole identifier is fresh, which is what the reference's
/// `suffix or token_hex(6)` resolves an empty string to. The head is 20
/// hexadecimal characters either way.
#[must_use]
pub fn generate_session_id(suffix: Option<&str>) -> String {
    let head = random_hex::<10>();
    let tail = match suffix.filter(|suffix| !suffix.is_empty()) {
        Some(suffix) => suffix.to_owned(),
        None => random_hex::<6>(),
    };
    format!(
        "{}-{}-{}-{}-{tail}",
        &head[..8],
        &head[8..12],
        &head[12..16],
        &head[16..20]
    )
}

/// Reads the stable trailing segment out of an identifier.
///
/// An identifier with no separator is its own suffix, which is what the
/// reference's right split returns and what keeps an identifier this port did
/// not mint, such as one a client named, from losing its identity on a
/// rotation.
#[must_use]
pub fn extract_suffix(session_id: &str) -> &str {
    session_id
        .rsplit_once(SEGMENT_SEPARATOR)
        .map_or(session_id, |(_, suffix)| suffix)
}

/// Mints the identifier a session continues under once its transcript was
/// replaced, keeping `previous`'s trailing segment.
#[must_use]
pub fn rotate_session_id(previous: &str) -> String {
    generate_session_id(Some(extract_suffix(previous)))
}

/// A fresh random version-4 UUID in its canonical form, which is how the
/// reference mints message identifiers (`uuid4()` in `vibe/core/types.py`).
#[must_use]
pub fn uuid_v4() -> String {
    let mut bytes = random_bytes::<16>();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex_encode(&bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// `N` bytes an identifier is minted from.
///
/// The operating system's random source supplies them. When it refuses, which
/// no supported platform does in practice, they are derived from the clock,
/// the process id and a process-wide counter instead: an identifier unique in
/// this process and unlikely to repeat in another is still better than an
/// operation that cannot mint one, and none is ever left zeroed. What must stay
/// unpredictable does not come from here: an OAuth state, a PKCE verifier and
/// a configuration transaction's sidecar name read the random source directly
/// and fail when it refuses.
#[must_use]
pub fn random_bytes<const N: usize>() -> [u8; N] {
    static FALLBACK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let mut bytes = [0_u8; N];
    if getrandom::fill(&mut bytes).is_err() {
        let seed = [
            crate::clock::now_nanos().to_le_bytes().as_slice(),
            std::process::id().to_le_bytes().as_slice(),
            FALLBACK_SEQUENCE
                .fetch_add(1, Ordering::Relaxed)
                .to_le_bytes()
                .as_slice(),
        ]
        .concat();
        for (block, chunk) in (0_u64..).zip(bytes.chunks_mut(32)) {
            let digest = Sha256::new()
                .chain_update(&seed)
                .chain_update(block.to_le_bytes())
                .finalize();
            chunk.copy_from_slice(&digest[..chunk.len()]);
        }
    }
    bytes
}

/// `N` [`random_bytes`] as lowercase hexadecimal.
#[must_use]
pub fn random_hex<const N: usize>() -> String {
    hex_encode(&random_bytes::<N>())
}
