//! Reference `datetime.fromisoformat`, read the way the pinned interpreter's C
//! implementation (`Modules/_datetimemodule.c` in CPython 3.12) reads a stamp.
//!
//! That parser is laxer than ISO 8601 and laxer than its own documentation: the
//! separator after the date may be any single character, a fraction may follow
//! the hour or the minute, and one stray character may stand between the time
//! and its offset. It walks a NUL-terminated buffer, so a read past the end of
//! the stamp sees NUL here too ([`byte_at`]).

use super::{
    Clock, FRACTION_SCALE, UtcTimestamp, byte_at, civil_from_days, civil_instant, days_from_civil,
    digits, is_leap_year,
};

pub(super) fn parse(value: &str) -> Option<UtcTimestamp> {
    if value.chars().count() < 7 {
        return None;
    }
    let bytes = value.as_bytes();
    let separator = date_end(bytes)?;
    let (year, month, day) = date(bytes, separator)?;
    let (clock, offset_micros) = if bytes.len() > separator {
        time(bytes, separator + 1)?
    } else {
        (Clock::default(), 0)
    };
    civil_instant(year, month, day, clock, offset_micros)
}

/// Where the date ends and the separator stands, told apart by the fifth and
/// sixth characters: `YYYY-MM-DD`, `YYYYMMDD`, and the ISO week forms
/// `YYYY-Www[-D]` and `YYYYWww[D]`.
fn date_end(bytes: &[u8]) -> Option<usize> {
    let length = bytes.len();
    if length == 7 {
        return Some(7);
    }
    if byte_at(bytes, 4) == b'-' {
        if byte_at(bytes, 5) != b'W' {
            return Some(10);
        }
        if length > 8 && byte_at(bytes, 8) == b'-' {
            if length == 9 {
                return None;
            }
            // `YYYY-Www-` then a digit at 10 is read as a week date followed
            // by a time, not as a weekday.
            return Some(if length > 10 && byte_at(bytes, 10).is_ascii_digit() {
                8
            } else {
                10
            });
        }
        return Some(8);
    }
    if byte_at(bytes, 4) != b'W' {
        return Some(8);
    }
    let digits_end = (7..length)
        .find(|&index| !byte_at(bytes, index).is_ascii_digit())
        .unwrap_or(length);
    Some(match digits_end {
        _ if digits_end < 9 => digits_end,
        _ if digits_end % 2 == 0 => 7,
        _ => 8,
    })
}

/// The calendar date the first `length` bytes spell, a week date converted.
fn date(bytes: &[u8], length: usize) -> Option<(i64, i64, i64)> {
    let year = digits(bytes, 0, 4)?;
    let dashed = byte_at(bytes, 4) == b'-';
    let mut at = 4 + usize::from(dashed);
    let separator = |at: &mut usize| {
        let ok = !dashed || byte_at(bytes, *at) == b'-';
        *at += usize::from(dashed);
        ok.then_some(())
    };
    if byte_at(bytes, at) == b'W' {
        at += 1;
        let week = digits(bytes, at, 2)?;
        at += 2;
        let weekday = if at < length {
            separator(&mut at)?;
            digits(bytes, at, 1)?
        } else {
            1
        };
        return week_date(year, week, weekday);
    }
    let month = digits(bytes, at, 2)?;
    at += 2;
    separator(&mut at)?;
    let day = digits(bytes, at, 2)?;
    Some((year, month, day))
}

/// The calendar date of ISO week `week`, day `weekday` (Monday is 1) of
/// `year`. Week 53 exists only in a year starting on a Thursday, or a leap
/// year starting on a Wednesday.
fn week_date(year: i64, week: i64, weekday: i64) -> Option<(i64, i64, i64)> {
    let january_first = days_from_civil(year, 1, 1);
    // 1970-01-01, day zero, was a Thursday: Monday is 0 here.
    let first_weekday = (january_first + 3).rem_euclid(7);
    let long_year = first_weekday == 3 || (first_weekday == 2 && is_leap_year(year));
    if !(1..53).contains(&week) && !(week == 53 && long_year) {
        return None;
    }
    if !(1..=7).contains(&weekday) {
        return None;
    }
    let mut week_one = january_first - first_weekday;
    if first_weekday > 3 {
        week_one += 7;
    }
    Some(civil_from_days(week_one + (week - 1) * 7 + weekday - 1))
}

/// The time from `start` to the end, and its offset east of UTC in
/// microseconds: the first `Z`, `+` or `-` opens the offset.
fn time(bytes: &[u8], start: usize) -> Option<(Clock, i64)> {
    let end = bytes.len();
    let mut offset_at = start;
    while !matches!(byte_at(bytes, offset_at), b'Z' | b'+' | b'-') {
        offset_at += 1;
        if offset_at >= end {
            break;
        }
    }
    let (clock, trailing) = clock(bytes, start, offset_at)?;
    if offset_at == end {
        return (!trailing).then_some((clock, 0));
    }
    // One character the time left unread before the offset is ignored,
    // which is how `10:30:15 +02:00` parses.
    let sign = match byte_at(bytes, offset_at) {
        b'Z' => return (byte_at(bytes, offset_at + 1) == 0).then_some((clock, 0)),
        b'-' => -1,
        _ => 1,
    };
    let (offset, trailing) = self::clock(bytes, offset_at + 1, end)?;
    if trailing {
        return None;
    }
    let seconds = offset.hour * 3_600 + offset.minute * 60 + offset.second;
    // A whole-second offset of zero is UTC whatever its fraction says.
    let offset_micros = if seconds == 0 {
        0
    } else {
        sign * (seconds * 1_000_000 + offset.micros)
    };
    (offset_micros.abs() < 86_400 * 1_000_000).then_some((clock, offset_micros))
}

/// `HH[:?MM[:?SS]][(.|,)f+]` from `start`, bounded by `end`, and whether a
/// character is left unread before `end` or the stamp's own end.
///
/// The separator after the hour decides for the whole clock: a colon there
/// requires one between every component, its absence forbids one. A fraction
/// may follow any component; digits past the sixth are read and ignored.
fn clock(bytes: &[u8], start: usize, end: usize) -> Option<(Clock, bool)> {
    let mut components = [0; 3];
    let mut at = start;
    let mut separated = true;
    for (index, component) in components.iter_mut().enumerate() {
        *component = digits(bytes, at, 2)?;
        let next = byte_at(bytes, at + 2);
        at += 3;
        if index == 0 {
            separated = next == b':';
        }
        if at >= end {
            let [hour, minute, second] = components;
            let clock = Clock {
                hour,
                minute,
                second,
                micros: 0,
            };
            return Some((clock, next != 0));
        }
        match next {
            b':' if separated => {}
            b'.' | b',' => break,
            _ if !separated => at -= 1,
            _ => return None,
        }
    }
    let taken = (end - at).min(6);
    let micros = digits(bytes, at, taken)? * FRACTION_SCALE[taken];
    at += taken;
    while byte_at(bytes, at).is_ascii_digit() {
        at += 1;
    }
    let [hour, minute, second] = components;
    let clock = Clock {
        hour,
        minute,
        second,
        micros,
    };
    Some((clock, byte_at(bytes, at) != 0))
}
