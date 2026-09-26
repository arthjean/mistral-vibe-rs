//! Timestamps, spelled the way the session directory and its metadata do.
//!
//! Two spellings are needed: the ISO instant metadata records, and the compact
//! stamp a session directory is named by, which is what makes a listing sort by
//! age without reading any metadata. Both are derived from milliseconds since
//! the epoch with Howard Hinnant's civil-from-days algorithm rather than a date
//! dependency, because the whole need is these two formats.

/// Reference `utc_now().isoformat()`: microseconds and an explicit offset.
pub(crate) fn format_iso_timestamp(milliseconds: u64) -> String {
    let (year, month, day, hour, minute, second, millis) = timestamp_parts(milliseconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}000+00:00")
}

/// Reference `datetime.now(UTC).isoformat(timespec="milliseconds")` with the
/// offset spelled `Z`, which is how a lease diagnostic stamps its holder.
pub(crate) fn format_lease_timestamp(milliseconds: u64) -> String {
    let (year, month, day, hour, minute, second, millis) = timestamp_parts(milliseconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

pub(super) fn format_compact_timestamp(milliseconds: u64) -> String {
    let (year, month, day, hour, minute, second, _) = timestamp_parts(milliseconds);
    format!("{year:04}{month:02}{day:02}_{hour:02}{minute:02}{second:02}")
}

/// Milliseconds since the epoch of an ISO 8601 instant, as reference
/// `datetime.fromisoformat` reads one: a date, a `T` or a space, a time with an
/// optional fraction, and an optional `Z` or `+HH:MM` offset. A reading with no
/// offset is taken as UTC; every timestamp either implementation writes
/// carries one.
#[must_use]
pub fn parse_iso_millis(value: &str) -> Option<u64> {
    let value = value.trim();
    let (date, rest) = value.split_at_checked(10)?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u64 = date_parts.next()?.parse().ok()?;
    let day: u64 = date_parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut seconds_of_day = 0_i64;
    let mut fraction_millis = 0_i64;
    let mut offset_seconds = 0_i64;
    if !rest.is_empty() {
        let rest = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' '))?;
        let offset_start = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
        let (clock, offset) = rest.split_at(offset_start);
        let (clock, fraction) = clock.split_once('.').unwrap_or((clock, ""));
        let mut clock_parts = clock.split(':');
        let hour: i64 = clock_parts.next()?.parse().ok()?;
        let minute: i64 = clock_parts.next().unwrap_or("0").parse().ok()?;
        let second: i64 = clock_parts.next().unwrap_or("0").parse().ok()?;
        seconds_of_day = hour * 3_600 + minute * 60 + second;
        if !fraction.is_empty() {
            let digits: String = fraction.chars().chain("000".chars()).take(3).collect();
            fraction_millis = digits.parse().ok()?;
        }
        if let Some(sign) = offset
            .chars()
            .next()
            .filter(|sign| matches!(sign, '+' | '-'))
        {
            let offset = offset.get(1..)?.replace(':', "");
            let hours: i64 = offset.get(..2)?.parse().ok()?;
            let minutes: i64 = offset.get(2..4).unwrap_or("0").parse().ok()?;
            offset_seconds = (hours * 3_600 + minutes * 60) * if sign == '-' { -1 } else { 1 };
        }
    }
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + seconds_of_day - offset_seconds;
    u64::try_from(seconds * 1_000 + fraction_millis).ok()
}

/// The ISO instant [`parse_iso_millis`] reads, rewritten in UTC the way the
/// reference's listing index normalizes every timestamp it keeps.
#[must_use]
pub fn normalize_iso_utc(value: &str) -> Option<String> {
    parse_iso_micros(value).map(format_iso_micros)
}

/// Reference `datetime.isoformat()` of a UTC instant: the fraction appears
/// only when it is not zero.
pub(super) fn format_iso_micros(micros: u64) -> String {
    let (year, month, day, hour, minute, second, _) = timestamp_parts(micros / 1_000);
    let fraction = micros % 1_000_000;
    if fraction == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
    } else {
        format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{fraction:06}+00:00"
        )
    }
}

/// [`parse_iso_millis`] at microsecond precision, which is what the listing
/// index keeps when it rewrites a timestamp in UTC.
fn parse_iso_micros(value: &str) -> Option<u64> {
    let value = value.trim();
    let millis = parse_iso_millis(value)?;
    let fraction = value
        .split_once('.')
        .map(|(_, rest)| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .chain("000000".chars())
                .take(6)
                .collect::<String>()
        })
        .and_then(|digits| digits.parse::<u64>().ok())
        .unwrap_or(0);
    Some(millis / 1_000 * 1_000_000 + fraction)
}

fn days_from_civil(year: i64, month: u64, day: u64) -> i64 {
    let month = i64::try_from(month).unwrap_or(1);
    let day = i64::try_from(day).unwrap_or(1);
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn timestamp_parts(milliseconds: u64) -> (i64, u64, u64, u64, u64, u64, u64) {
    let seconds = milliseconds / 1_000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60,
        milliseconds % 1_000,
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u64, u64) {
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
    (
        year,
        u64::try_from(month).unwrap_or_default(),
        u64::try_from(day).unwrap_or_default(),
    )
}
