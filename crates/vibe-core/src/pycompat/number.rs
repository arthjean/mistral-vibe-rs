//! Numbers as Python spells and reads them.

use std::fmt::Write as _;

/// `repr(float)`, which `str(float)` and `json.dumps` share: the shortest
/// digits that read back as the same value, positional while the decimal
/// exponent lies in `-4..16` and scientific with a signed exponent of at least
/// two digits outside it.
#[must_use]
pub fn repr_float(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if value == 0.0 {
        return format!("{sign}0.0");
    }
    // `{:e}` spells the shortest round-trip digits as `d.ddde<exponent>`.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent = exponent.parse::<i32>().unwrap_or_default();
    let digits = mantissa.replace('.', "");
    let mut rendered = sign.to_owned();
    if !(-4..16).contains(&exponent) {
        let (first, rest) = digits.split_at(1.min(digits.len()));
        rendered.push_str(first);
        if !rest.is_empty() {
            rendered.push('.');
            rendered.push_str(rest);
        }
        let _ = write!(
            rendered,
            "e{}{:02}",
            if exponent < 0 { '-' } else { '+' },
            exponent.unsigned_abs()
        );
    } else if exponent < 0 {
        rendered.push_str("0.");
        for _ in 1..exponent.unsigned_abs() {
            rendered.push('0');
        }
        rendered.push_str(&digits);
    } else {
        let point = usize::try_from(exponent).unwrap_or_default() + 1;
        if digits.len() <= point {
            let _ = write!(rendered, "{digits:0<point$}.0");
        } else {
            let (whole, fraction) = digits.split_at(point);
            let _ = write!(rendered, "{whole}.{fraction}");
        }
    }
    rendered
}

/// `format(value, ",")`: the digits grouped by three with commas. Any
/// integer type groups as is; a `u128` past the `i128` range saturates at its
/// maximum.
#[must_use]
pub fn thousands(value: impl TryInto<i128>) -> String {
    let value = value.try_into().unwrap_or(i128::MAX);
    let digits = value.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if value < 0 {
        grouped.push('-');
    }
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// `int(text)`: surrounding `White_Space`, a sign, and ASCII digits that a
/// single underscore may separate. Python's integers are unbounded; this one
/// saturates at the `i128` range, and the caller decides what a value past
/// its own range means.
#[must_use]
pub fn parse_int(text: &str) -> Option<i128> {
    let trimmed = text.trim();
    let (negative, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'_')
    {
        return None;
    }
    let magnitude = digits
        .bytes()
        .filter(u8::is_ascii_digit)
        .fold(0_i128, |total, digit| {
            total
                .saturating_mul(10)
                .saturating_add(i128::from(digit - b'0'))
        });
    Some(if negative { -magnitude } else { magnitude })
}

/// `float(text)`: surrounding `White_Space`, a sign, digits with an optional
/// fraction and exponent where a single underscore may separate two ASCII
/// digits, and `inf`, `infinity` and `nan` in any case.
#[must_use]
pub fn parse_float(text: &str) -> Option<f64> {
    let characters = text.trim().chars().collect::<Vec<_>>();
    let mut cleaned = String::with_capacity(characters.len());
    for (index, character) in characters.iter().enumerate() {
        if *character != '_' {
            cleaned.push(*character);
            continue;
        }
        let digit_before = index
            .checked_sub(1)
            .and_then(|before| characters.get(before))
            .is_some_and(char::is_ascii_digit);
        let digit_after = characters.get(index + 1).is_some_and(char::is_ascii_digit);
        if !(digit_before && digit_after) {
            return None;
        }
    }
    cleaned.parse::<f64>().ok()
}
