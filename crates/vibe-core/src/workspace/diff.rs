//! The unified diff a file mutation reports.

const DIFF_CONTEXT_LINES: usize = 3;

/// Renders a single unified-diff hunk covering the changed span.
///
/// Only the lines that actually differ, plus [`DIFF_CONTEXT_LINES`] of context,
/// reach the model. Emitting whole files would make an one-line edit cost as
/// much as the file itself.
pub(crate) fn unified_diff(before: &str, after: &str) -> String {
    if before == after {
        return String::new();
    }
    let before_lines = before.lines().collect::<Vec<_>>();
    let after_lines = after.lines().collect::<Vec<_>>();
    let shortest = before_lines.len().min(after_lines.len());
    let prefix = before_lines
        .iter()
        .zip(&after_lines)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = before_lines
        .iter()
        .rev()
        .zip(after_lines.iter().rev())
        .take_while(|(left, right)| left == right)
        .count()
        .min(shortest.saturating_sub(prefix));
    let context_start = prefix.saturating_sub(DIFF_CONTEXT_LINES);
    let before_span = context_start
        ..before_lines
            .len()
            .saturating_sub(suffix)
            .saturating_add(DIFF_CONTEXT_LINES)
            .min(before_lines.len());
    let after_span = context_start
        ..after_lines
            .len()
            .saturating_sub(suffix)
            .saturating_add(DIFF_CONTEXT_LINES)
            .min(after_lines.len());

    let mut lines = vec![
        "--- before".to_owned(),
        "+++ after".to_owned(),
        format!(
            "@@ -{},{} +{},{} @@",
            hunk_start(before_span.start, before_span.len()),
            before_span.len(),
            hunk_start(after_span.start, after_span.len()),
            after_span.len(),
        ),
    ];
    lines.extend(
        before_lines
            .get(context_start..prefix)
            .unwrap_or_default()
            .iter()
            .map(|line| format!(" {line}")),
    );
    lines.extend(
        before_lines
            .get(prefix..before_lines.len().saturating_sub(suffix))
            .unwrap_or_default()
            .iter()
            .map(|line| format!("-{line}")),
    );
    lines.extend(
        after_lines
            .get(prefix..after_lines.len().saturating_sub(suffix))
            .unwrap_or_default()
            .iter()
            .map(|line| format!("+{line}")),
    );
    lines.extend(
        after_lines
            .get(after_lines.len().saturating_sub(suffix)..after_span.end)
            .unwrap_or_default()
            .iter()
            .map(|line| format!(" {line}")),
    );
    lines.join("\n")
}

const fn hunk_start(start: usize, length: usize) -> usize {
    if length == 0 {
        start
    } else {
        start.saturating_add(1)
    }
}
