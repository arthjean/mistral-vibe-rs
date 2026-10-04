use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::images::ImageFormat;

/// Normalizes a bracketed paste before it is inserted.
///
/// Reference `maybe_prepend_at_for_path`
/// (`vibe/cli/textual_ui/widgets/chat_input/paste_path.py`): a paste whose
/// non-blank lines are all existing absolute paths, of any type, becomes one
/// mention per line joined by spaces; anything else keeps the bare image
/// rewrite the text-changed hook applies.
#[must_use]
pub fn normalize_pasted_text(pasted: &str) -> String {
    let lines = pasted
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if !lines.is_empty()
        && let Some(mentions) = lines
            .iter()
            .map(|line| pasted_path_mention(line))
            .collect::<Option<Vec<_>>>()
    {
        return mentions.join(" ");
    }

    rewrite_bare_image_paths(pasted)
}

/// Normalizes the composer text after an edit: reference
/// `rewrite_bare_image_paths_in_text`, the text-changed hook that recovers
/// drag-and-drop in terminals without bracketed paste.
#[must_use]
pub fn normalize_typed_text(text: &str) -> String {
    rewrite_bare_image_paths(text)
}

fn pasted_path_mention(line: &str) -> Option<String> {
    if line.starts_with('@') {
        return None;
    }
    let candidate = unescaped_path_candidate(line);
    if candidate.is_empty() {
        return None;
    }
    let path = expand_tilde_path(&candidate);
    (path.is_absolute() && path.exists()).then(|| format!("@{}", quote_path_if_needed(&candidate)))
}

/// Reference `build_path_prompt_payload`'s scan
/// (`vibe/core/autocompletion/path_prompt.py`): every `@` that does not follow
/// a letter, a digit or `_` opens a candidate, quoted or bare, and a candidate
/// that `resolve` accepts is kept and skipped over. One it refuses, an empty
/// one included, moves the scan a single character on, so an anchor inside a
/// refused quoted candidate is still read.
pub fn resolved_mentions<T>(
    text: &str,
    mut resolve: impl FnMut(&str) -> Option<T>,
) -> Vec<(String, T)> {
    let characters = text.chars().collect::<Vec<_>>();
    let mut mentions = Vec::new();
    let mut position = 0usize;
    while position < characters.len() {
        if is_path_anchor(&characters, position)
            && let Some((candidate, next)) = extract_candidate(&characters, position + 1)
            && !candidate.is_empty()
            && let Some(value) = resolve(&candidate)
        {
            mentions.push((candidate, value));
            position = next;
            continue;
        }
        position += 1;
    }
    mentions
}

/// Reference `_is_path_anchor`.
fn is_path_anchor(characters: &[char], position: usize) -> bool {
    if characters.get(position) != Some(&'@') {
        return false;
    }
    position == 0
        || !characters
            .get(position - 1)
            .is_some_and(|previous| previous.is_alphanumeric() || *previous == '_')
}

/// Reference `_extract_candidate`: a quoted candidate, or the run of path
/// characters, and the position after it.
fn extract_candidate(characters: &[char], start: usize) -> Option<(String, usize)> {
    let head = *characters.get(start)?;
    if matches!(head, '\'' | '"') {
        return extract_quoted_candidate(characters, start + 1, head);
    }
    let end = characters[start..]
        .iter()
        .position(|character| !is_mention_path_character(*character))
        .map_or(characters.len(), |offset| start + offset);
    (end > start).then(|| (characters[start..end].iter().collect(), end))
}

/// Reference `_extract_quoted_candidate`: everything up to the closing quote,
/// with a backslash before the quote character standing for the quote itself.
/// An unterminated quote is no candidate.
fn extract_quoted_candidate(
    characters: &[char],
    start: usize,
    quote: char,
) -> Option<(String, usize)> {
    let mut candidate = String::new();
    let mut position = start;
    while let Some(&character) = characters.get(position) {
        if character == '\\' && characters.get(position + 1) == Some(&quote) {
            candidate.push(quote);
            position += 2;
            continue;
        }
        if character == quote {
            return Some((candidate, position + 1));
        }
        candidate.push(character);
        position += 1;
    }
    None
}

pub fn resolve_candidate(workspace: &Path, candidate: &str) -> Option<PathBuf> {
    fs::canonicalize(candidate_path(workspace, candidate)).ok()
}

pub fn resolve_owned_candidate(
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

/// A path token in pasted or typed text: a quoted run, or an absolute or
/// home-relative path whose escaped spaces are kept.
fn scan_pasted_path(text: &str, start: usize) -> Option<(String, usize)> {
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
    if !matches!(head, '/' | '~') {
        return None;
    }

    let mut value = String::new();
    let mut end = start;
    while end < text.len() {
        let character = text.get(end..)?.chars().next()?;
        if character == '\\' && text[end + character.len_utf8()..].starts_with(' ') {
            value.push(' ');
            end = end.saturating_add(character.len_utf8() + 1);
            continue;
        }
        if character.is_whitespace() {
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
    // Reference `_unescape_spaces` leaves Windows paths as typed.
    if cfg!(windows) {
        unquoted.to_owned()
    } else {
        unquoted.replace("\\ ", " ")
    }
}

fn is_image_file(candidate: &str) -> bool {
    let path = expand_tilde_path(candidate);
    path.is_absolute() && ImageFormat::from_path(&path).is_some() && path.is_file()
}

/// Reference `_quote_if_needed`: a path made only of alphanumerics and the
/// unquoted mention characters stays bare; otherwise it takes the first quote
/// it does not contain, escaping single quotes when it contains both.
fn quote_path_if_needed(path: &str) -> String {
    if path.chars().all(is_mention_path_character) {
        return path.to_owned();
    }
    if !path.contains('\'') {
        format!("'{path}'")
    } else if !path.contains('"') {
        format!("\"{path}\"")
    } else {
        format!("'{}'", path.replace('\'', "\\'"))
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
            && let Some((candidate, end)) = scan_pasted_path(text, cursor)
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
