//! The text a tool result reaches the model as.
//!
//! The reference tools return a typed model and its agent loop renders it as
//! one `field: value` line per declared field, in declaration order, with
//! Python's own scalar formatting. That rendering is part of the observable
//! contract rather than an implementation detail: it is what the model reads,
//! so a port that renders its own summary answers a different prompt.
//!
//! This module is that rendering. A handler builds its fields in the order the
//! reference declares them and calls [`joined`]; the scalar helpers cover the
//! three renderings that differ from Rust's own (`None`, `True`, `False`) plus
//! the repr Python uses for a list of dictionaries, which is how `todo` reaches
//! the model.

use std::fmt::Display;
use std::fmt::Write as _;

/// One `key: value` line per field, joined by newlines.
#[must_use]
pub fn joined(fields: &[(&str, String)]) -> String {
    fields
        .iter()
        .map(|(key, value)| format!("{key}: {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A boolean the way Python prints one.
#[must_use]
pub const fn boolean(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

/// An optional scalar, absent as Python's `None`.
#[must_use]
pub fn optional<T: Display>(value: Option<T>) -> String {
    value.map_or_else(|| "None".to_owned(), |value| value.to_string())
}

/// A list of dictionaries the way Python's `repr` prints one.
///
/// Each entry keeps the field order it was built in, because a dictionary
/// preserves insertion order and the rendering is compared against a capture
/// that observed exactly that order.
#[must_use]
pub fn dictionary_list(entries: &[Vec<(&str, String)>]) -> String {
    let mut rendered = String::from("[");
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        rendered.push('{');
        for (position, (key, value)) in entry.iter().enumerate() {
            if position > 0 {
                rendered.push_str(", ");
            }
            let _ = write!(rendered, "{}: {value}", crate::pycompat::repr_str(key));
        }
        rendered.push('}');
    }
    rendered.push(']');
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_render_the_way_python_prints_them() {
        assert_eq!(boolean(true), "True");
        assert_eq!(boolean(false), "False");
        assert_eq!(optional::<usize>(None), "None");
        assert_eq!(optional(Some(3_usize)), "3");
    }

    #[test]
    fn a_dictionary_list_renders_entries_in_the_order_they_were_built() {
        let entries = vec![vec![
            ("id", crate::pycompat::repr_str("one")),
            ("content", crate::pycompat::repr_str("first task")),
        ]];
        assert_eq!(
            dictionary_list(&entries),
            "[{'id': 'one', 'content': 'first task'}]"
        );
        assert_eq!(dictionary_list(&[]), "[]");
    }

    #[test]
    fn fields_join_one_per_line() {
        assert_eq!(
            joined(&[("a", "1".to_owned()), ("b", "None".to_owned())]),
            "a: 1\nb: None"
        );
    }
}
