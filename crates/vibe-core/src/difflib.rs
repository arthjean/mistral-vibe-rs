//! The unified diff CPython's `difflib.unified_diff` produces, without its two
//! file header lines.
//!
//! Presentation that reproduces the reference's diffs (the edit result's
//! occurrence diff in the terminal) has to cut hunks where the reference cuts
//! them, so this runs the same matcher, with the same popular-element
//! heuristic, and the same grouping.

use crate::checkpoints::matcher::{SequenceMatcher, Tag};

/// The hunks turning `old` into `new` with `context` lines around each change:
/// an `@@ -a,b +c,d @@` header per hunk, then one line per element prefixed
/// with a space, `-` or `+`. Identical inputs produce no line at all.
#[must_use]
pub fn unified_diff(old: &[&str], new: &[&str], context: usize) -> Vec<String> {
    let matcher = SequenceMatcher::with_autojunk(old, new);
    let mut lines = Vec::new();
    for group in matcher.grouped_opcodes(context) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        lines.push(format!(
            "@@ -{} +{} @@",
            unified_range(first.i1, last.i2),
            unified_range(first.j1, last.j2)
        ));
        for code in &group {
            if code.tag == Tag::Equal {
                lines.extend(old[code.i1..code.i2].iter().map(|line| format!(" {line}")));
                continue;
            }
            if matches!(code.tag, Tag::Replace | Tag::Delete) {
                lines.extend(old[code.i1..code.i2].iter().map(|line| format!("-{line}")));
            }
            if matches!(code.tag, Tag::Replace | Tag::Insert) {
                lines.extend(new[code.j1..code.j2].iter().map(|line| format!("+{line}")));
            }
        }
    }
    lines
}

/// CPython `_format_range_unified`: one-based, the length omitted when it is
/// one, and an empty range named by the line before it.
fn unified_range(start: usize, stop: usize) -> String {
    let length = stop - start;
    match length {
        1 => format!("{}", start + 1),
        0 => format!("{start},0"),
        _ => format!("{},{length}", start + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_keeps_two_lines_of_context_and_splits_distant_hunks() {
        let old = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        let new = ["a", "B", "c", "d", "e", "f", "g", "h", "I", "j"];
        assert_eq!(
            unified_diff(&old, &new, 2),
            [
                "@@ -1,4 +1,4 @@",
                " a",
                "-b",
                "+B",
                " c",
                " d",
                "@@ -7,4 +7,4 @@",
                " g",
                " h",
                "-i",
                "+I",
                " j",
            ]
        );
    }

    #[test]
    fn identical_inputs_have_no_hunk_and_empty_ranges_name_the_line_before() {
        assert!(unified_diff(&["x"], &["x"], 2).is_empty());
        assert_eq!(unified_diff(&[], &["new"], 2), ["@@ -0,0 +1 @@", "+new"]);
    }
}
