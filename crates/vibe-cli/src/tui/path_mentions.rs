use std::fs;
use std::path::{Component, Path, PathBuf};

use vibe_core::images::ImageFormat;

#[must_use]
pub fn normalize_pasted_text(pasted: &str) -> String {
    let trimmed = pasted.trim();
    if !trimmed.is_empty() && !trimmed.contains(['\n', '\r']) && !trimmed.starts_with('@') {
        let candidate = unescaped_path_candidate(trimmed);
        if candidate.starts_with(['/', '~']) && is_image_file(&candidate) {
            return format!("@{}", quote_path_if_needed(&candidate));
        }
    }

    rewrite_bare_image_paths(pasted)
}

pub(super) fn mention_values(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut cursor = 0usize;
    while cursor < text.len() {
        let Some(relative_start) = text[cursor..].find('@') else {
            break;
        };
        let start = cursor.saturating_add(relative_start);
        let boundary = text[..start].chars().next_back();
        if boundary.is_some_and(|character| character.is_alphanumeric() || character == '_') {
            cursor = start.saturating_add(1);
            continue;
        }
        let value_start = start.saturating_add(1);
        let Some((value, end)) = scan_path_value(text, value_start, PathSyntax::Mention) else {
            cursor = start.saturating_add(1);
            continue;
        };
        values.push(value);
        cursor = end;
    }
    values
}

pub(super) fn resolve_candidate(workspace: &Path, candidate: &str) -> Option<PathBuf> {
    fs::canonicalize(candidate_path(workspace, candidate)).ok()
}

pub(super) fn resolve_owned_candidate(
    workspace: &Path,
    candidate: &str,
    is_tracked: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let path = candidate_path(workspace, candidate);
    if is_tracked(&path) {
        return Some(path);
    }
    fs::canonicalize(path).ok().filter(|path| is_tracked(path))
}

fn candidate_path(workspace: &Path, candidate: &str) -> PathBuf {
    let candidate = expand_tilde_path(candidate);
    if candidate.is_absolute() {
        candidate
    } else {
        workspace.join(candidate)
    }
}

#[derive(Debug, Clone, Copy)]
enum PathSyntax {
    Mention,
    Pasted,
}

fn scan_path_value(text: &str, start: usize, syntax: PathSyntax) -> Option<(String, usize)> {
    let head = text.get(start..)?.chars().next()?;
    if matches!(head, '\'' | '"') {
        let content_start = start.saturating_add(head.len_utf8());
        let relative_end = text.get(content_start..)?.find(head)?;
        let content_end = content_start.saturating_add(relative_end);
        return Some((
            text[content_start..content_end].to_owned(),
            content_end.saturating_add(head.len_utf8()),
        ));
    }
    if matches!(syntax, PathSyntax::Pasted) && !matches!(head, '/' | '~') {
        return None;
    }

    let mut value = String::new();
    let mut end = start;
    while end < text.len() {
        let character = text.get(end..)?.chars().next()?;
        if matches!(syntax, PathSyntax::Pasted)
            && character == '\\'
            && text[end + character.len_utf8()..].starts_with(' ')
        {
            value.push(' ');
            end = end.saturating_add(character.len_utf8() + 1);
            continue;
        }
        let accepted = match syntax {
            PathSyntax::Mention => is_mention_path_character(character),
            PathSyntax::Pasted => !character.is_whitespace(),
        };
        if !accepted {
            break;
        }
        value.push(character);
        end = end.saturating_add(character.len_utf8());
    }
    (!value.is_empty()).then_some((value, end))
}

fn unescaped_path_candidate(value: &str) -> String {
    let unquoted = ['\'', '"']
        .into_iter()
        .find_map(|quote| {
            value
                .strip_prefix(quote)
                .and_then(|inner| inner.strip_suffix(quote))
        })
        .unwrap_or(value);
    unquoted.replace("\\ ", " ")
}

fn is_image_file(candidate: &str) -> bool {
    let path = expand_tilde_path(candidate);
    path.is_absolute() && ImageFormat::from_path(&path).is_some() && path.is_file()
}

fn quote_path_if_needed(path: &str) -> String {
    if path.contains(' ') {
        format!("'{path}'")
    } else {
        path.to_owned()
    }
}

fn rewrite_bare_image_paths(text: &str) -> String {
    if !text.contains(['/', '~', '\'', '"']) {
        return text.to_owned();
    }
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while cursor < text.len() {
        if is_path_token_boundary(text, cursor)
            && let Some((candidate, end)) = scan_path_value(text, cursor, PathSyntax::Pasted)
            && is_image_file(&candidate)
        {
            output.push('@');
            output.push_str(&quote_path_if_needed(&candidate));
            cursor = end;
            continue;
        }
        let Some(character) = text[cursor..].chars().next() else {
            break;
        };
        output.push(character);
        cursor = cursor.saturating_add(character.len_utf8());
    }
    output
}

fn is_path_token_boundary(text: &str, byte: usize) -> bool {
    if byte == 0 {
        return true;
    }
    let previous = text[..byte].chars().next_back();
    previous != Some('@')
        && previous.is_some_and(|character| character.is_whitespace() || "(<[".contains(character))
}

fn is_mention_path_character(character: char) -> bool {
    character.is_alphanumeric() || "._/\\-()[]{}~".contains(character)
}

fn expand_tilde_path(value: &str) -> PathBuf {
    let path = Path::new(value);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(first)) if first == "~") {
        return path.to_path_buf();
    }
    let Some(mut home) = user_home_directory() else {
        return path.to_path_buf();
    };
    home.extend(components);
    home
}

fn user_home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .or_else(|| {
                let mut home = PathBuf::from(std::env::var_os("HOMEDRIVE")?);
                home.push(std::env::var_os("HOMEPATH")?);
                Some(home)
            })
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}
