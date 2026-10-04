//! Ghost-text completion for a skill typed mid-prompt.
//!
//! Reference `InlineSkillCompletionController`
//! (`vibe/cli/autocompletion/inline_skill_completion.py`, new at v2.24.1):
//! when the word ending at the caret starts with `/` and is not the first word
//! of a default-mode prompt, the first skill alias in sorted order that
//! extends it, compared without case, is previewed after the caret. Tab and
//! the right arrow accept it; the slash popup keeps the first word.
//!
//! Positions here are byte offsets into the prompt, so a keystroke costs the
//! length of the token under the caret rather than the length of the prompt.

/// A mid-prompt skill token is at least a slash and one more character.
const MIN_SKILL_TOKEN_LEN: usize = 2;

/// The ghost on display: the token it completes and the alias it completes
/// the token to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InlineSkillGhost {
    /// The token's byte range in the prompt.
    pub(crate) start: usize,
    pub(crate) end: usize,
    /// The alias accepting the ghost writes over the token.
    pub(crate) alias: String,
    /// The token's length in characters, which is where the ghost starts.
    token_length: usize,
}

impl InlineSkillGhost {
    /// What is drawn after the caret: the alias past the token's length.
    pub(crate) fn suffix(&self) -> String {
        self.alias.chars().skip(self.token_length).collect()
    }
}

/// Reference `_token_span`: the byte range of the skill token ending at
/// `cursor`, or [`None`] when the prompt is not in its default mode, the caret
/// sits inside a word, the token is the prompt's first word or the token is no
/// skill token.
pub(crate) fn token_span(text: &str, cursor: usize, default_mode: bool) -> Option<(usize, usize)> {
    if cursor == 0 || cursor > text.len() || !text.is_char_boundary(cursor) || !default_mode {
        return None;
    }
    if text[cursor..]
        .chars()
        .next()
        .is_some_and(|following| !is_python_space(following))
    {
        return None;
    }
    let start = text[..cursor]
        .char_indices()
        .rev()
        .find(|(_, character)| is_python_space(*character))
        .map_or(0, |(byte, character)| byte + character.len_utf8());
    if text[..start].chars().rev().all(is_python_space) {
        return None;
    }
    let token = &text[start..cursor];
    let mut characters = token.chars();
    let is_skill_token = token.chars().count() >= MIN_SKILL_TOKEN_LEN
        && characters.next() == Some('/')
        && !characters.any(|character| character == '/')
        && !token.contains('@');
    is_skill_token.then_some((start, cursor))
}

/// Reference `_best_match`: the smallest alias, in code-point order, longer
/// than `token` and starting with it without regard to case.
pub(crate) fn best_match<'a>(
    token: &str,
    aliases: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let needle = token.to_lowercase();
    let length = token.chars().count();
    aliases
        .into_iter()
        .filter(|alias| alias.chars().count() > length && alias.to_lowercase().starts_with(&needle))
        .min()
        .map(str::to_owned)
}

/// Reference `on_text_changed`: the ghost for `text` with the caret at byte
/// `cursor`, or [`None`] when nothing is previewed.
pub(crate) fn ghost_for<'a>(
    text: &str,
    cursor: usize,
    default_mode: bool,
    aliases: impl IntoIterator<Item = &'a str>,
) -> Option<InlineSkillGhost> {
    let (start, end) = token_span(text, cursor, default_mode)?;
    let token = &text[start..end];
    let alias = best_match(token, aliases)?;
    Some(InlineSkillGhost {
        start,
        end,
        alias,
        token_length: token.chars().count(),
    })
}

/// Reference `_accept`: whether `ghost` still applies to `text` with the caret
/// at byte `cursor`. A caret move that never reached the controller can leave
/// a ghost behind; it is accepted only while the caret still ends the same
/// token and the alias still extends it.
pub(crate) fn still_applies(
    ghost: &InlineSkillGhost,
    text: &str,
    cursor: usize,
    default_mode: bool,
) -> bool {
    token_span(text, cursor, default_mode) == Some((ghost.start, ghost.end))
        && ghost
            .alias
            .to_lowercase()
            .starts_with(&text[ghost.start..ghost.end].to_lowercase())
}

/// `str.isspace` for one character: Unicode whitespace plus the four
/// information separators Python also counts.
fn is_python_space(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALIASES: [&str; 3] = ["/implement", "/implement-plan", "/deploy"];

    #[test]
    fn the_ghost_previews_the_rest_of_the_smallest_extending_alias() {
        let ghost = ghost_for("fix /imp", 8, true, ALIASES).expect("a ghost");
        assert_eq!((ghost.start, ghost.end), (4, 8));
        assert_eq!(ghost.suffix(), "lement");
        let deeper = ghost_for("fix /implement", 14, true, ALIASES).expect("a deeper ghost");
        assert_eq!(deeper.suffix(), "-plan");
    }

    #[test]
    fn the_first_word_and_other_modes_get_no_ghost() {
        assert_eq!(ghost_for("/imp", 4, true, ALIASES), None);
        assert_eq!(ghost_for("  /imp", 6, true, ALIASES), None);
        assert_eq!(ghost_for("fix /imp", 8, false, ALIASES), None);
    }
}
