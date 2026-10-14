//! Wall-clock readings in the units the harness publishes.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the epoch, the unit every snapshot time is in.
#[must_use]
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Microseconds since the epoch.
#[must_use]
pub fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
        })
}

/// Python's `datetime.isoformat()` of a UTC instant in microseconds:
/// `2026-10-13T14:58:17.313000+00:00`.
#[must_use]
pub fn iso_micros(micros: u64) -> String {
    let Ok(timestamp) = jiff::Timestamp::from_microsecond(i64::try_from(micros).unwrap_or(0))
    else {
        return String::new();
    };
    timestamp
        .strftime("%Y-%m-%dT%H:%M:%S%.6f+00:00")
        .to_string()
}

/// The same instant in milliseconds.
#[must_use]
pub fn iso_millis(millis: u64) -> String {
    iso_micros(millis.saturating_mul(1000))
}

/// An instant as `isoformat(timespec="milliseconds")` with a `Z`.
#[must_use]
pub fn iso_millis_z(millis: u64) -> String {
    let Ok(timestamp) = jiff::Timestamp::from_millisecond(i64::try_from(millis).unwrap_or(0))
    else {
        return String::new();
    };
    timestamp.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Milliseconds of an ISO instant either spelling writes.
#[must_use]
pub fn parse_iso_millis(value: &str) -> Option<u64> {
    let timestamp: jiff::Timestamp = value.parse().ok()?;
    u64::try_from(timestamp.as_millisecond()).ok()
}

/// A moment in local time as Python's `%Y-%m-%d %H:%M:%S %Z` renders it.
#[must_use]
pub fn local_timestamp(epoch_seconds: f64) -> Option<String> {
    if !epoch_seconds.is_finite() {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let whole = epoch_seconds.floor() as i64;
    let timestamp = jiff::Timestamp::from_second(whole).ok()?;
    let zoned = timestamp.to_zoned(jiff::tz::TimeZone::system());
    Some(zoned.strftime("%Y-%m-%d %H:%M:%S %Z").to_string())
}
