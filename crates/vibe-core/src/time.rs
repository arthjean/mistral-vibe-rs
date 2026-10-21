//! Civil time: the UTC calendar arithmetic and the ISO 8601 spellings every
//! layer reads and writes timestamps in.
//!
//! Dates are derived from counts since the epoch with Howard Hinnant's
//! civil-from-days algorithm rather than a date dependency, because the whole
//! need is a handful of fixed spellings. [`UtcTimestamp`] is the instant a
//! caller carries; the free functions serve callers that hold milliseconds or
//! microseconds since the epoch. The wall clock itself is read through
//! [`crate::clock`].

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

    /// Parses `YYYY-MM-DD[T ]HH:MM[:SS[.ffffff]]` with an optional `Z` or
    /// `±HH[:]MM` offset, normalizing to UTC. This covers what the reference
    /// accepts through `value.replace("Z", "+00:00")` plus `fromisoformat`
    /// over the server's expiry field.
    pub fn parse_iso8601(value: &str) -> Option<Self> {
        let (head, offset_seconds) = split_utc_offset(value)?;
        let year: i64 = parse_digits(head, 0..4)?;
        if head.as_bytes().get(4) != Some(&b'-') || head.as_bytes().get(7) != Some(&b'-') {
            return None;
        }
        let month: i64 = parse_digits(head, 5..7)?;
        let day: i64 = parse_digits(head, 8..10)?;
        if !matches!(head.as_bytes().get(10), Some(b'T' | b't' | b' ')) {
            return None;
        }
        let hour: i64 = parse_digits(head, 11..13)?;
        if head.as_bytes().get(13) != Some(&b':') {
            return None;
        }
        let minute: i64 = parse_digits(head, 14..16)?;
        let (second, fraction_micros) = match head.as_bytes().get(16) {
            None => (0, 0),
            Some(b':') => {
                let second: i64 = parse_digits(head, 17..19)?;
                let fraction = match head.as_bytes().get(19) {
                    None => 0,
                    Some(b'.' | b',') => parse_fraction_micros(head.get(20..)?)?,
                    Some(_) => return None,
                };
                (second, fraction)
            }
            Some(_) => return None,
        };
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || !(0..24).contains(&hour)
            || !(0..60).contains(&minute)
            || !(0..60).contains(&second)
        {
            return None;
        }
        let seconds =
            days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
                - offset_seconds;
        Some(Self {
            micros_since_epoch: seconds * 1_000_000 + fraction_micros,
        })
    }

    /// The Python `isoformat` shape the corpus and the editor protocol carry:
    /// `+00:00` suffix, fractional seconds only when they are nonzero.
    pub fn to_iso8601(self) -> String {
        let seconds = self.micros_since_epoch.div_euclid(1_000_000);
        let micros = self.micros_since_epoch.rem_euclid(1_000_000);
        let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
        let of_day = seconds.rem_euclid(86_400);
        let (hour, minute, second) = (of_day / 3_600, (of_day % 3_600) / 60, of_day % 60);
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

fn split_utc_offset(value: &str) -> Option<(&str, i64)> {
    if let Some(head) = value.strip_suffix(['Z', 'z']) {
        return Some((head, 0));
    }
    if value.len() > 10
        && let Some(position) = value[10..].rfind(['+', '-'])
    {
        let (head, offset) = value.split_at(10 + position);
        return Some((head, parse_utc_offset(offset)?));
    }
    // A naive timestamp is read as UTC; the server contract always sends an
    // offset, so nothing measured reaches this line.
    Some((value, 0))
}

fn parse_utc_offset(offset: &str) -> Option<i64> {
    let sign = match offset.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = offset[1..].chars().filter(|c| *c != ':').collect();
    let (hours, minutes): (i64, i64) = match digits.len() {
        2 => (digits.parse().ok()?, 0),
        4 => (
            digits.get(0..2)?.parse().ok()?,
            digits.get(2..4)?.parse().ok()?,
        ),
        _ => return None,
    };
    Some(sign * (hours * 3_600 + minutes * 60))
}

fn parse_digits(value: &str, range: std::ops::Range<usize>) -> Option<i64> {
    let slice = value.get(range)?;
    if !slice.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    slice.parse().ok()
}

fn parse_fraction_micros(digits: &str) -> Option<i64> {
    if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let padded = format!("{digits:0<6}");
    padded.get(0..6)?.parse().ok()
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
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
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

/// Reference `datetime.isoformat()` of a UTC instant in microseconds since
/// the epoch: the fraction appears only when it is not zero.
#[must_use]
pub fn format_iso_micros(micros: u64) -> String {
    UtcTimestamp::from_micros_since_epoch(i64::try_from(micros).unwrap_or(i64::MAX)).to_iso8601()
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

/// The UTC calendar fields of an instant in milliseconds since the epoch:
/// year, month, day, hour, minute, second and millisecond.
pub(crate) fn utc_fields(milliseconds: u64) -> (i64, u64, u64, u64, u64, u64, u64) {
    let seconds = milliseconds / 1_000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    (
        year,
        u64::try_from(month).unwrap_or_default(),
        u64::try_from(day).unwrap_or_default(),
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60,
        milliseconds % 1_000,
    )
}
