//! The text a configuration file is written back as.
//!
//! The reference persists a layer through `tomli_w.dump` with its default
//! options (`vibe/core/config/layers/_base.py`, `_write_toml_snapshot`), so a
//! file this port rewrites reads the same as one the reference rewrote: keys
//! in document order, scalars before sub-tables, every non-empty array spread
//! one item per line, an array of tables kept inline while each entry fits on
//! one line, and a table holding nothing but sub-tables left without a header
//! of its own. `toml::to_string_pretty` lays the same document out otherwise,
//! which is why this encoder exists.

use std::fmt::Write as _;

use toml::{Table, Value};

/// How many columns an inline table may take, indentation and trailing comma
/// included, before an array of tables is written as `[[name]]` sections.
const MAX_LINE_LENGTH: usize = 100;
/// The indentation one array nesting level adds.
const INDENT: &str = "    ";

/// The document as the reference writes it.
#[must_use]
pub(super) fn encode_document(table: &Table) -> String {
    let mut out = String::new();
    write_table(&mut out, table, "", false);
    out
}

fn write_table(out: &mut String, table: &Table, name: &str, inside_array: bool) {
    let mut literals = Vec::new();
    let mut tables: Vec<(&str, &Table, bool)> = Vec::new();
    for (key, value) in table {
        match value {
            Value::Table(nested) => tables.push((key, nested, false)),
            Value::Array(items) if is_array_of_tables(items) && !all_fit_inline(items) => {
                tables.extend(
                    items
                        .iter()
                        .filter_map(Value::as_table)
                        .map(|nested| (key.as_str(), nested, true)),
                );
            }
            value => literals.push((key, value)),
        }
    }

    let mut written = false;
    if inside_array || (!name.is_empty() && (!literals.is_empty() || tables.is_empty())) {
        written = true;
        if inside_array {
            let _ = writeln!(out, "[[{name}]]");
        } else {
            let _ = writeln!(out, "[{name}]");
        }
    }
    for (key, value) in &literals {
        written = true;
        let _ = writeln!(out, "{} = {}", key_part(key), literal(value, 0));
    }
    for (key, nested, in_array) in tables {
        if written {
            out.push('\n');
        } else {
            written = true;
        }
        let key = key_part(key);
        let display = if name.is_empty() {
            key
        } else {
            format!("{name}.{key}")
        };
        write_table(out, nested, &display, in_array);
    }
}

/// A non-empty array whose every item is a table.
fn is_array_of_tables(items: &[Value]) -> bool {
    !items.is_empty() && items.iter().all(Value::is_table)
}

fn all_fit_inline(items: &[Value]) -> bool {
    items.iter().filter_map(Value::as_table).all(|table| {
        let rendered = format!("{INDENT}{},", inline_table(table));
        rendered.chars().count() <= MAX_LINE_LENGTH && !rendered.contains('\n')
    })
}

fn literal(value: &Value, nesting: usize) -> String {
    match value {
        Value::String(text) => basic_string(text),
        Value::Integer(number) => number.to_string(),
        Value::Float(number) => python_float(*number),
        Value::Boolean(flag) => flag.to_string(),
        Value::Datetime(datetime) => python_datetime(datetime),
        Value::Array(items) => inline_array(items, nesting),
        Value::Table(table) => inline_table(table),
    }
}

fn inline_array(items: &[Value], nesting: usize) -> String {
    if items.is_empty() {
        return "[]".to_owned();
    }
    let indent = INDENT.repeat(nesting + 1);
    let closing = INDENT.repeat(nesting);
    let lines = items
        .iter()
        .map(|item| format!("{indent}{}", literal(item, nesting + 1)))
        .collect::<Vec<_>>();
    format!("[\n{},\n{closing}]", lines.join(",\n"))
}

/// An inline table restarts the array indentation, as `tomli_w` renders each
/// of its values at the outermost level.
fn inline_table(table: &Table) -> String {
    if table.is_empty() {
        return "{}".to_owned();
    }
    let pairs = table
        .iter()
        .map(|(key, value)| format!("{} = {}", key_part(key), literal(value, 0)))
        .collect::<Vec<_>>();
    format!("{{ {} }}", pairs.join(", "))
}

fn key_part(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    if bare {
        key.to_owned()
    } else {
        basic_string(key)
    }
}

/// A basic string: a quote, a backslash and every control character but the
/// tab escaped, the short escapes where TOML has one.
fn basic_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '\u{8}' => out.push_str("\\b"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push('\t'),
            control if control.is_ascii_control() => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// A float as Python's `str` spells it: the shortest digits that round-trip,
/// positional between `1e-4` and `1e16` with at least one fractional digit,
/// scientific with a signed two-digit exponent outside that range.
fn python_float(number: f64) -> String {
    if number.is_nan() {
        return "nan".to_owned();
    }
    if number.is_infinite() {
        return if number < 0.0 { "-inf" } else { "inf" }.to_owned();
    }
    // `{:e}` gives the shortest round-trip digits as `d.ddde<exp>`.
    let scientific = format!("{number:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if negative { "-" } else { "" };
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        let (integral, fractional) = if point <= 0 {
            let zeros = "0".repeat(usize::try_from(-point).unwrap_or(0));
            ("0".to_owned(), format!("{zeros}{digits}"))
        } else {
            let point = usize::try_from(point).unwrap_or(0);
            if digits.len() <= point {
                (
                    format!("{digits}{}", "0".repeat(point - digits.len())),
                    String::new(),
                )
            } else {
                (digits[..point].to_owned(), digits[point..].to_owned())
            }
        };
        let fractional = if fractional.is_empty() {
            "0".to_owned()
        } else {
            fractional
        };
        return format!("{sign}{integral}.{fractional}");
    }
    let (head, tail) = digits.split_at(1);
    let mantissa = if tail.is_empty() {
        head.to_owned()
    } else {
        format!("{head}.{tail}")
    };
    let exponent_sign = if exponent < 0 { '-' } else { '+' };
    format!("{sign}{mantissa}e{exponent_sign}{:02}", exponent.abs())
}

/// A date-time as Python's `str` spells the value `tomllib` reads it as: a
/// space between date and time, microseconds only when non-zero, and an
/// offset as `+HH:MM`, UTC included.
fn python_datetime(datetime: &toml::value::Datetime) -> String {
    let mut out = String::new();
    if let Some(date) = datetime.date {
        let _ = write!(out, "{:04}-{:02}-{:02}", date.year, date.month, date.day);
    }
    if let Some(time) = datetime.time {
        if !out.is_empty() {
            out.push(' ');
        }
        let _ = write!(
            out,
            "{:02}:{:02}:{:02}",
            time.hour,
            time.minute,
            time.second.unwrap_or(0)
        );
        let micros = time.nanosecond.unwrap_or(0) / 1_000;
        if micros != 0 {
            let _ = write!(out, ".{micros:06}");
        }
    }
    match datetime.offset {
        Some(toml::value::Offset::Z) => out.push_str("+00:00"),
        Some(toml::value::Offset::Custom { minutes }) => {
            let sign = if minutes < 0 { '-' } else { '+' };
            let minutes = minutes.unsigned_abs();
            let _ = write!(out, "{sign}{:02}:{:02}", minutes / 60, minutes % 60);
        }
        None => {}
    }
    out
}
