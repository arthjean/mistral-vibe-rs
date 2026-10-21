//! Civil time: the UTC calendar arithmetic and the ISO 8601 spellings every
//! layer reads and writes timestamps in.
//!
//! Dates are derived from counts since the epoch with Howard Hinnant's
//! civil-from-days algorithm rather than a date dependency, because the whole
//! need is a handful of fixed spellings; only [`format_local_timestamp`],
//! which needs the system time zone, reads it through `jiff`. [`UtcTimestamp`]
//! is the instant a caller carries; the free functions serve callers that hold
//! milliseconds or microseconds since the epoch. The wall clock itself is read
//! through [`crate::clock`].
//!
//! A stamp is read with the grammar of the reference call that reads it:
//! `datetime.fromisoformat` ([`UtcTimestamp::parse_iso8601`], which every
//! free reader derives from) or a pydantic `datetime` field
//! ([`UtcTimestamp::parse_pydantic`]).

/// A UTC instant with microsecond precision, the resolution the reference's
/// `datetime` carries: parsed from an ISO 8601 stamp, compared, offset and
/// written back out the way `isoformat` writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UtcTimestamp {
    micros_since_epoch: i64,
}

impl UtcTimestamp {
    pub fn from_micros_since_epoch(micros_since_epoch: i64) -> Self {
        Self { micros_since_epoch }
    }

    /// The instant as microseconds since the epoch, which a caller rendering a
    /// coarser stamp divides down.
    #[must_use]
    pub const fn micros_since_epoch(self) -> i64 {
        self.micros_since_epoch
    }

    pub fn now() -> Self {
        Self {
            micros_since_epoch: i64::try_from(crate::clock::now_micros()).unwrap_or(i64::MAX),
        }
    }

    /// Reference `datetime.fromisoformat`, as the pinned interpreter's C
    /// parser reads a stamp, normalized to UTC. A stamp with no offset is read
    /// as UTC, where the reference's callers read it in local time; every
    /// stamp either implementation writes carries one.
    pub fn parse_iso8601(value: &str) -> Option<Self> {
        fromisoformat::parse(value)
    }

    /// Pydantic's `datetime` validation of a string, normalized to UTC: a
    /// `YYYY-MM-DD` date, alone or followed by `T`, `t`, a space or `_` and
    /// `HH:MM[:SS[(.|,)f+]]`, then `Z`, `z` or `±HH[:]MM`. A naive stamp is
    /// read as UTC. A string pydantic reads as a Unix timestamp is refused.
    pub fn parse_pydantic(value: &str) -> Option<Self> {
        pydantic::parse(value)
    }

    /// The instant `millis` milliseconds after the epoch.
    #[must_use]
    pub(crate) fn from_millis_since_epoch(millis: u64) -> Self {
        Self {
            micros_since_epoch: i64::try_from(millis)
                .map_or(i64::MAX, |millis| millis.saturating_mul(1_000)),
        }
    }

    /// The UTC calendar fields of the instant.
    #[must_use]
    pub(crate) fn civil(self) -> CivilTime {
        let seconds = self.micros_since_epoch.div_euclid(1_000_000);
        let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
        let of_day = seconds.rem_euclid(86_400);
        CivilTime {
            year,
            month,
            day,
            hour: of_day / 3_600,
            minute: (of_day % 3_600) / 60,
            second: of_day % 60,
            micros: self.micros_since_epoch.rem_euclid(1_000_000),
        }
    }

    /// The Python `isoformat` shape the corpus and the editor protocol carry:
    /// `+00:00` suffix, fractional seconds only when they are nonzero.
    pub fn to_iso8601(self) -> String {
        let CivilTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
            micros,
        } = self.civil();
        if micros == 0 {
            format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
        } else {
            format!(
                "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}+00:00"
            )
        }
    }

    pub fn plus_seconds(self, seconds: f64) -> Self {
        Self {
            micros_since_epoch: self.micros_since_epoch + (seconds * 1_000_000.0) as i64,
        }
    }

    /// Seconds from `earlier` to `self`, negative when `self` precedes it.
    pub fn seconds_since(self, earlier: Self) -> f64 {
        (self.micros_since_epoch - earlier.micros_since_epoch) as f64 / 1_000_000.0
    }
}

/// The UTC calendar fields of an instant: the proleptic Gregorian date, the
/// time of day, and the microseconds within the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CivilTime {
    pub(crate) year: i64,
    pub(crate) month: i64,
    pub(crate) day: i64,
    pub(crate) hour: i64,
    pub(crate) minute: i64,
    pub(crate) second: i64,
    pub(crate) micros: i64,
}

/// A time of day as a stamp spells it, before it is checked against the
/// calendar.
#[derive(Debug, Clone, Copy, Default)]
struct Clock {
    hour: i64,
    minute: i64,
    second: i64,
    micros: i64,
}

/// What `n` fraction digits are multiplied by to count microseconds.
const FRACTION_SCALE: [i64; 7] = [0, 100_000, 10_000, 1_000, 100, 10, 1];

/// The byte at `index`, or NUL past the end, which is what a parser reading
/// a NUL-terminated buffer sees there.
fn byte_at(bytes: &[u8], index: usize) -> u8 {
    bytes.get(index).copied().unwrap_or(0)
}

/// `count` ASCII digits starting at `at`, as one number.
fn digits(bytes: &[u8], at: usize, count: usize) -> Option<i64> {
    (at..at + count).try_fold(0, |value, index| {
        let digit = byte_at(bytes, index);
        digit
            .is_ascii_digit()
            .then(|| value * 10 + i64::from(digit - b'0'))
    })
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// The instant a calendar date, a time of day and an offset east of UTC name,
/// once `datetime` would accept them: a year from 1 to 9999, a day the month
/// holds, and a time of day below 24:00.
fn civil_instant(
    year: i64,
    month: i64,
    day: i64,
    clock: Clock,
    offset_micros: i64,
) -> Option<UtcTimestamp> {
    let month_days = match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1..=12 => 31,
        _ => return None,
    };
    if !(1..=9999).contains(&year)
        || !(1..=month_days).contains(&day)
        || !(0..24).contains(&clock.hour)
        || !(0..60).contains(&clock.minute)
        || !(0..60).contains(&clock.second)
    {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400
        + clock.hour * 3_600
        + clock.minute * 60
        + clock.second;
    Some(UtcTimestamp {
        micros_since_epoch: seconds * 1_000_000 + clock.micros - offset_micros,
    })
}

/// Days between 1970-01-01 and a proleptic Gregorian date (Howard Hinnant's
/// algorithm), the inverse of [`civil_from_days`].
#[must_use]
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The proleptic Gregorian `(year, month, day)` of a count of days since
/// 1970-01-01.
#[must_use]
pub fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_epoch.saturating_add(719_468);
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

/// Reference `_parse_time_ms`: milliseconds since the epoch of the instant
/// [`UtcTimestamp::parse_iso8601`] reads, or nothing for one before the epoch.
#[must_use]
pub fn parse_iso_millis(value: &str) -> Option<u64> {
    let micros = UtcTimestamp::parse_iso8601(value)?.micros_since_epoch();
    u64::try_from(micros.div_euclid(1_000)).ok()
}

/// Reference `_convert_to_utc_iso`: the instant [`UtcTimestamp::parse_iso8601`]
/// reads, rewritten in UTC the way the listing index keeps every timestamp.
#[must_use]
pub fn normalize_iso_utc(value: &str) -> Option<String> {
    UtcTimestamp::parse_iso8601(value).map(UtcTimestamp::to_iso8601)
}

/// Reference `datetime.isoformat()` of a UTC instant in microseconds since
/// the epoch: the fraction appears only when it is not zero.
#[must_use]
pub fn format_iso_micros(micros: u64) -> String {
    UtcTimestamp::from_micros_since_epoch(i64::try_from(micros).unwrap_or(i64::MAX)).to_iso8601()
}

/// [`format_iso_micros`] of an instant in milliseconds since the epoch.
#[must_use]
pub fn format_iso_millis(millis: u64) -> String {
    format_iso_micros(millis.saturating_mul(1_000))
}

/// Reference `isoformat(timespec="milliseconds")` with the UTC offset spelled
/// `Z`: the fraction is always three digits.
#[must_use]
pub fn format_iso_millis_z(millis: u64) -> String {
    let CivilTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
        micros,
    } = UtcTimestamp::from_millis_since_epoch(millis).civil();
    let millis = micros / 1_000;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// A moment in local time as Python's `%Y-%m-%d %H:%M:%S %Z` renders it.
#[must_use]
pub fn format_local_timestamp(epoch_seconds: f64) -> Option<String> {
    if !epoch_seconds.is_finite() {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let whole = epoch_seconds.floor() as i64;
    let timestamp = jiff::Timestamp::from_second(whole).ok()?;
    let zoned = timestamp.to_zoned(jiff::tz::TimeZone::system());
    Some(zoned.strftime("%Y-%m-%d %H:%M:%S %Z").to_string())
}

mod fromisoformat;
mod pydantic;

#[cfg(test)]
mod time_tests;
