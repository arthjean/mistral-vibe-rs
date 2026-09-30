//! Python's `string.Template.safe_substitute`, which every prompt placeholder
//! upstream goes through.
//!
//! The rules are the standard library's: `$$` becomes `$`, `$name` and
//! `${name}` become the named value when one is given and stay as written
//! otherwise, and a `$` that starts neither is left alone. A name is an ASCII
//! letter or underscore followed by ASCII letters, digits or underscores, and
//! the match is greedy, so `$current_dates` names `current_dates`, not
//! `current_date`.

/// Substitutes `values` into `template` the way `safe_substitute` does.
#[must_use]
pub fn safe_substitute(template: &str, values: &[(&str, &str)]) -> String {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(index) = rest.find('$') {
        output.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            output.push('$');
            rest = tail;
            continue;
        }
        if let Some(braced) = after.strip_prefix('{') {
            let length = identifier_length(braced);
            if length > 0 && braced[length..].starts_with('}') {
                let name = &braced[..length];
                match lookup(values, name) {
                    Some(value) => output.push_str(value),
                    None => {
                        output.push_str("${");
                        output.push_str(name);
                        output.push('}');
                    }
                }
                rest = &braced[length + 1..];
                continue;
            }
            output.push('$');
            rest = after;
            continue;
        }
        let length = identifier_length(after);
        if length > 0 {
            let name = &after[..length];
            match lookup(values, name) {
                Some(value) => output.push_str(value),
                None => {
                    output.push('$');
                    output.push_str(name);
                }
            }
            rest = &after[length..];
            continue;
        }
        output.push('$');
        rest = after;
    }
    output.push_str(rest);
    output
}

fn lookup<'a>(values: &[(&str, &'a str)], name: &str) -> Option<&'a str> {
    values
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| *value)
}

/// The byte length of the identifier `text` opens with, zero when it opens
/// with none.
fn identifier_length(text: &str) -> usize {
    let bytes = text.as_bytes();
    match bytes.first() {
        Some(first) if first.is_ascii_alphabetic() || *first == b'_' => {}
        _ => return 0,
    }
    bytes
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
        .count()
}
