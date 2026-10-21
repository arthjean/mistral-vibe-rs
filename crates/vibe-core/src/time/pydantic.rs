//! Pydantic's `datetime` validation of a string (pydantic-core's `speedate`),
//! the grammar a reference model field typed `datetime` reads a stamp with.

use super::{Clock, FRACTION_SCALE, UtcTimestamp, byte_at, civil_instant, digits};

pub(super) fn parse(value: &str) -> Option<UtcTimestamp> {
    let bytes = value.as_bytes();
    let year = digits(bytes, 0, 4)?;
    let month = digits(bytes, 5, 2)?;
    let day = digits(bytes, 8, 2)?;
    if byte_at(bytes, 4) != b'-' || byte_at(bytes, 7) != b'-' {
        return None;
    }
    if bytes.len() == 10 {
        return civil_instant(year, month, day, Clock::default(), 0);
    }
    if !matches!(byte_at(bytes, 10), b'T' | b't' | b' ' | b'_') {
        return None;
    }
    let mut clock = Clock {
        hour: digits(bytes, 11, 2)?,
        minute: digits(bytes, 14, 2)?,
        ..Clock::default()
    };
    if byte_at(bytes, 13) != b':' {
        return None;
    }
    let mut at = 16;
    if byte_at(bytes, at) == b':' {
        clock.second = digits(bytes, 17, 2)?;
        at = 19;
        if matches!(byte_at(bytes, at), b'.' | b',') {
            at += 1;
            let count = bytes[at..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if count == 0 {
                return None;
            }
            let taken = count.min(6);
            clock.micros = digits(bytes, at, taken)? * FRACTION_SCALE[taken];
            at += count;
        }
    }
    let offset_micros = match byte_at(bytes, at) {
        _ if at == bytes.len() => 0,
        b'Z' | b'z' if at + 1 == bytes.len() => 0,
        sign @ (b'+' | b'-') => {
            let hours = digits(bytes, at + 1, 2)?;
            let minutes_at = at + 3 + usize::from(byte_at(bytes, at + 3) == b':');
            let minutes = digits(bytes, minutes_at, 2)?;
            if minutes_at + 2 != bytes.len() || hours >= 24 || minutes >= 60 {
                return None;
            }
            let offset = (hours * 3_600 + minutes * 60) * 1_000_000;
            if sign == b'-' { -offset } else { offset }
        }
        _ => return None,
    };
    civil_instant(year, month, day, clock, offset_micros)
}
