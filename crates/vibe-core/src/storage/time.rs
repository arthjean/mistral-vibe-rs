//! Timestamps, spelled the way the session directory and its metadata do.
//!
//! Two spellings are needed: the ISO instant metadata records, and the compact
//! stamp a session directory is named by, which is what makes a listing sort by
//! age without reading any metadata. Both are derived from milliseconds since
//! the epoch with Howard Hinnant's civil-from-days algorithm rather than a date
//! dependency, because the whole need is these two formats.

pub(super) fn format_iso_timestamp(milliseconds: u64) -> String {
    let (year, month, day, hour, minute, second, millis) = timestamp_parts(milliseconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}000+00:00")
}

pub(super) fn format_compact_timestamp(milliseconds: u64) -> String {
    let (year, month, day, hour, minute, second, _) = timestamp_parts(milliseconds);
    format!("{year:04}{month:02}{day:02}_{hour:02}{minute:02}{second:02}")
}

pub(super) fn timestamp_sort_key(value: &str) -> Option<u64> {
    let digits: String = value
        .chars()
        .filter(char::is_ascii_digit)
        .take(17)
        .collect();
    (digits.len() == 17)
        .then(|| digits.parse::<u64>().ok())
        .flatten()
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
