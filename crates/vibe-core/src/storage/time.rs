//! The compact stamp a session directory is named by, which is what makes a
//! listing sort by age without reading any metadata. The ISO instants metadata
//! and leases record, and the calendar arithmetic, are [`crate::time`]'s.

use crate::time::utc_fields;

pub(super) fn format_compact_timestamp(milliseconds: u64) -> String {
    let (year, month, day, hour, minute, second, _) = utc_fields(milliseconds);
    format!("{year:04}{month:02}{day:02}_{hour:02}{minute:02}{second:02}")
}
