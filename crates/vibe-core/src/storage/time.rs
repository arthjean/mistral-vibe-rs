//! Timestamps, spelled the way the session directory and its metadata do.
//!
//! Three spellings are storage's own: the ISO instant metadata records, the
//! stamp a lease diagnostic carries, and the compact stamp a session directory
//! is named by, which is what makes a listing sort by age without reading any
//! metadata. The calendar arithmetic is [`crate::time`]'s.

use crate::time::utc_fields;

/// Reference `utc_now().isoformat()`: microseconds and an explicit offset.
pub(crate) fn format_iso_timestamp(milliseconds: u64) -> String {
    let (year, month, day, hour, minute, second, millis) = utc_fields(milliseconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}000+00:00")
}

/// Reference `datetime.now(UTC).isoformat(timespec="milliseconds")` with the
/// offset spelled `Z`, which is how a lease diagnostic stamps its holder.
pub(crate) fn format_lease_timestamp(milliseconds: u64) -> String {
    let (year, month, day, hour, minute, second, millis) = utc_fields(milliseconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

pub(super) fn format_compact_timestamp(milliseconds: u64) -> String {
    let (year, month, day, hour, minute, second, _) = utc_fields(milliseconds);
    format!("{year:04}{month:02}{day:02}_{hour:02}{minute:02}{second:02}")
}
