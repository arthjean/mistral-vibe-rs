//! The session identity, whose trailing segment survives a rotation.
//!
//! An identifier is UUID-shaped, `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, and
//! splits into two parts that are minted differently: the first four segments
//! are always fresh, and the last one is carried over from the identifier being
//! replaced. That last segment is what makes the sessions a compaction leaves
//! on disk recognizable as one another's continuation, since nothing renames
//! the directories they were written under.
//!
//! Reference: `vibe/core/session/session_id.py` at the pinned commit.

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
    let head = random_hexadecimal::<10>();
    let tail = match suffix.filter(|suffix| !suffix.is_empty()) {
        Some(suffix) => suffix.to_owned(),
        None => random_hexadecimal::<6>(),
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

/// `N` random bytes as lowercase hexadecimal.
///
/// A random source that refuses falls back to the clock, following
/// `new_compaction_id`: an identifier that is merely unlikely to repeat is
/// still better than a turn that cannot rotate its session at all.
fn random_hexadecimal<const N: usize>() -> String {
    let mut bytes = [0_u8; N];
    if getrandom::fill(&mut bytes).is_err() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default()
            .to_le_bytes();
        for (slot, byte) in bytes.iter_mut().zip(stamp.iter().cycle()) {
            *slot = *byte;
        }
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
