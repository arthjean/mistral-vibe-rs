//! Where Python's `json` module stops reading a document, and why.
//!
//! A hook that answers with something other than JSON is reported with the
//! decoder's own message and position (reference `_parse_structured_response`
//! formats `JSONDecodeError.msg`, `lineno` and `colno`). `serde_json` stops at
//! other places with other words, so this scanner walks the document the way
//! CPython's `json.scanner` does and reports its first complaint. It only
//! locates errors: a document it accepts is then parsed by `serde_json`.

/// One complaint: the decoder's message and the character offset it names.
type Failure = (&'static str, usize);

/// The decoder's message, line and column for a document Python refuses, or
/// `None` for one it reads.
pub(super) fn decode_error(text: &str) -> Option<(&'static str, usize, usize)> {
    let chars = text.chars().collect::<Vec<_>>();
    let failure = scan_document(&chars).err()?;
    let (message, position) = failure;
    let before = chars.get(..position).unwrap_or(&chars);
    let line = before
        .iter()
        .filter(|character| **character == '\n')
        .count()
        + 1;
    let column = match before.iter().rposition(|character| *character == '\n') {
        Some(newline) => position - newline,
        None => position + 1,
    };
    Some((message, line, column))
}

/// Whether the document is one of the three non-finite floats Python's
/// decoder accepts at the top level and `serde_json` does not.
pub(super) fn is_non_finite_literal(text: &str) -> bool {
    matches!(
        text.trim_matches(WHITESPACE),
        "NaN" | "Infinity" | "-Infinity"
    )
}

const WHITESPACE: [char; 4] = [' ', '\t', '\n', '\r'];

fn scan_document(chars: &[char]) -> Result<(), Failure> {
    if chars.first() == Some(&'\u{feff}') {
        return Err(("Unexpected UTF-8 BOM (decode using utf-8-sig)", 0));
    }
    let start = skip_whitespace(chars, 0);
    let end = scan_value(chars, start)?;
    let end = skip_whitespace(chars, end);
    if end == chars.len() {
        Ok(())
    } else {
        Err(("Extra data", end))
    }
}

fn skip_whitespace(chars: &[char], mut index: usize) -> usize {
    while chars
        .get(index)
        .is_some_and(|character| WHITESPACE.contains(character))
    {
        index += 1;
    }
    index
}

fn starts_with(chars: &[char], index: usize, word: &str) -> bool {
    let word = word.chars().collect::<Vec<_>>();
    chars.get(index..index + word.len()) == Some(word.as_slice())
}

fn scan_value(chars: &[char], index: usize) -> Result<usize, Failure> {
    const EXPECTING_VALUE: &str = "Expecting value";
    match chars.get(index) {
        Some('"') => scan_string(chars, index + 1),
        Some('{') => scan_object(chars, index + 1),
        Some('[') => scan_array(chars, index + 1),
        Some('n') if starts_with(chars, index, "null") => Ok(index + 4),
        Some('t') if starts_with(chars, index, "true") => Ok(index + 4),
        Some('f') if starts_with(chars, index, "false") => Ok(index + 5),
        Some('N') if starts_with(chars, index, "NaN") => Ok(index + 3),
        Some('I') if starts_with(chars, index, "Infinity") => Ok(index + 8),
        Some('-') if starts_with(chars, index, "-Infinity") => Ok(index + 9),
        Some(character) if *character == '-' || character.is_ascii_digit() => {
            scan_number(chars, index).ok_or((EXPECTING_VALUE, index))
        }
        _ => Err((EXPECTING_VALUE, index)),
    }
}

/// `-?(?:0|[1-9]\d*)(\.\d+)?([eE][-+]?\d+)?`, the decoder's number pattern.
fn scan_number(chars: &[char], index: usize) -> Option<usize> {
    let digit = |at: usize| chars.get(at).is_some_and(char::is_ascii_digit);
    let mut end = index;
    if chars.get(end) == Some(&'-') {
        end += 1;
    }
    match chars.get(end) {
        Some('0') => end += 1,
        Some(character) if character.is_ascii_digit() => {
            while digit(end) {
                end += 1;
            }
        }
        _ => return None,
    }
    if chars.get(end) == Some(&'.') && digit(end + 1) {
        end += 1;
        while digit(end) {
            end += 1;
        }
    }
    if matches!(chars.get(end), Some('e' | 'E')) {
        let mut exponent = end + 1;
        if matches!(chars.get(exponent), Some('+' | '-')) {
            exponent += 1;
        }
        if digit(exponent) {
            end = exponent;
            while digit(end) {
                end += 1;
            }
        }
    }
    Some(end)
}

/// Scans a string whose opening quote sits just before `index`.
fn scan_string(chars: &[char], index: usize) -> Result<usize, Failure> {
    const UNTERMINATED: &str = "Unterminated string starting at";
    let opening = index - 1;
    let mut cursor = index;
    loop {
        match chars.get(cursor) {
            None => return Err((UNTERMINATED, opening)),
            Some('"') => return Ok(cursor + 1),
            Some(character) if u32::from(*character) < 0x20 => {
                return Err(("Invalid control character at", cursor));
            }
            Some('\\') => match chars.get(cursor + 1) {
                None => return Err((UNTERMINATED, opening)),
                Some('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') => cursor += 2,
                Some('u') => {
                    let hex = chars.get(cursor + 2..cursor + 6);
                    if hex.is_none_or(|hex| !hex.iter().all(char::is_ascii_hexdigit)) {
                        return Err(("Invalid \\uXXXX escape", cursor + 1));
                    }
                    cursor += 6;
                }
                Some(_) => return Err(("Invalid \\escape", cursor)),
            },
            Some(_) => cursor += 1,
        }
    }
}

fn scan_object(chars: &[char], index: usize) -> Result<usize, Failure> {
    const PROPERTY_NAME: &str = "Expecting property name enclosed in double quotes";
    let mut cursor = skip_whitespace(chars, index);
    match chars.get(cursor) {
        Some('}') => return Ok(cursor + 1),
        Some('"') => {}
        _ => return Err((PROPERTY_NAME, cursor)),
    }
    loop {
        cursor = scan_string(chars, cursor + 1)?;
        cursor = skip_whitespace(chars, cursor);
        if chars.get(cursor) != Some(&':') {
            return Err(("Expecting ':' delimiter", cursor));
        }
        cursor = skip_whitespace(chars, cursor + 1);
        cursor = scan_value(chars, cursor)?;
        cursor = skip_whitespace(chars, cursor);
        match chars.get(cursor) {
            Some('}') => return Ok(cursor + 1),
            Some(',') => {
                cursor = skip_whitespace(chars, cursor + 1);
                if chars.get(cursor) != Some(&'"') {
                    return Err((PROPERTY_NAME, cursor));
                }
            }
            _ => return Err(("Expecting ',' delimiter", cursor)),
        }
    }
}

fn scan_array(chars: &[char], index: usize) -> Result<usize, Failure> {
    let mut cursor = skip_whitespace(chars, index);
    if chars.get(cursor) == Some(&']') {
        return Ok(cursor + 1);
    }
    loop {
        cursor = scan_value(chars, cursor)?;
        cursor = skip_whitespace(chars, cursor);
        match chars.get(cursor) {
            Some(']') => return Ok(cursor + 1),
            Some(',') => cursor = skip_whitespace(chars, cursor + 1),
            _ => return Err(("Expecting ',' delimiter", cursor)),
        }
    }
}

/// Python's `json.dumps(value)` with its defaults: `", "` and `": "` as
/// separators and every non-ASCII character escaped. This is how the reference
/// writes a rewritten call's arguments back into the assistant message.
#[must_use]
pub fn python_json_dumps(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

fn write_value(value: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => match (number.as_i64(), number.as_u64(), number.as_f64()) {
            (Some(integer), _, _) => out.push_str(&integer.to_string()),
            (None, Some(integer), _) => out.push_str(&integer.to_string()),
            (None, None, Some(float)) => out.push_str(&python_float_json(float)),
            _ => out.push_str(&number.to_string()),
        },
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            out.push('{');
            for (index, (key, item)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_string(key, out);
                out.push_str(": ");
                write_value(item, out);
            }
            out.push('}');
        }
    }
}

/// `float.__repr__`, which `json` uses, spelling the non-finite values the
/// way `json.dumps` does.
fn python_float_json(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else {
        crate::mcp::render::python_float(value)
    }
}

fn write_string(text: &str, out: &mut String) {
    use std::fmt::Write as _;
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(character),
            _ => {
                let mut units = [0_u16; 2];
                for unit in character.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}
