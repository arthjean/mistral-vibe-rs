//! An edit: the operations a tool asks for, applied to the text they were
//! written against, and the replacements they performed.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::WorkspaceError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditOperation {
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: bool,
}

/// One replacement an edit performed, expanded to the whole lines it sat on.
///
/// Reference `EditResult._ui_occurrences`: a client renders the replaced text
/// in its own line context rather than the bare anchor, which is what makes a
/// one-word change readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditOccurrence {
    /// The one-based line the replacement starts on, counted in the file as it
    /// stood before the edit.
    pub start_line: usize,
    pub old_text: String,
    pub new_text: String,
}

/// Applies one edit's operations to the text it was written against.
///
/// The rules are the file's, not the storage's, which is what lets an edit whose
/// content came from a client-owned buffer refuse a stale or ambiguous needle on
/// the same terms as one read from disk.
pub(crate) fn apply_edit_operations(
    path: &Path,
    original: &str,
    operations: &[EditOperation],
) -> Result<Edited, WorkspaceError> {
    let mut updated = original.to_owned();
    let mut occurrences = Vec::new();
    for operation in operations {
        let matches = updated.matches(&operation.old_text).count();
        if matches == 0 {
            return Err(WorkspaceError::StaleEdit {
                path: path.to_path_buf(),
                needle: operation.old_text.clone(),
            });
        }
        if matches > 1 && !operation.replace_all {
            return Err(WorkspaceError::AmbiguousEdit {
                path: path.to_path_buf(),
                matches,
            });
        }
        // The contexts are read off the text as it stands before this
        // operation, so a start line names the file the operator was looking
        // at rather than the one the replacement produced.
        let contexts = line_contexts(&updated, &operation.old_text);
        let kept = if operation.replace_all {
            contexts.as_slice()
        } else {
            &contexts[..contexts.len().min(1)]
        };
        occurrences.extend(
            kept.iter()
                .map(|(start_line, prefix, suffix)| EditOccurrence {
                    start_line: *start_line,
                    old_text: format!("{prefix}{}{suffix}", operation.old_text),
                    new_text: format!("{prefix}{}{suffix}", operation.new_text),
                }),
        );
        updated = if operation.replace_all {
            updated.replace(&operation.old_text, &operation.new_text)
        } else {
            updated.replacen(&operation.old_text, &operation.new_text, 1)
        };
    }
    Ok(Edited {
        text: updated,
        occurrences,
    })
}

/// The text one edit produced and the replacements it performed.
pub(crate) struct Edited {
    pub(crate) text: String,
    pub(crate) occurrences: Vec<EditOccurrence>,
}

/// Every occurrence of `snippet` in `content`, as the whole lines it sits on.
///
/// Reference `line_contexts`: each hit is reported as its one-based start line
/// and the two halves of the lines around it, so a caller can widen the anchor
/// to whole lines without searching the file again. A snippet made of newlines
/// alone anchors nothing and reports nothing.
fn line_contexts(content: &str, snippet: &str) -> Vec<(usize, String, String)> {
    if snippet.trim_matches('\n').is_empty() {
        return Vec::new();
    }
    let mut contexts = Vec::new();
    let mut from = 0;
    while let Some(offset) = content[from..].find(snippet) {
        let start = from + offset;
        let end = start + snippet.len();
        let line_start = content[..start].rfind('\n').map_or(0, |index| index + 1);
        let suffix = if content[..end].ends_with('\n') {
            ""
        } else {
            let line_end = content[end..]
                .find('\n')
                .map_or(content.len(), |index| end + index);
            &content[end..line_end]
        };
        contexts.push((
            content[..start].matches('\n').count() + 1,
            content[line_start..start].to_owned(),
            suffix.to_owned(),
        ));
        from = end;
    }
    contexts
}
