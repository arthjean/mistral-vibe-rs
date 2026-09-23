//! Word splitting with the quoting rules of Python's POSIX `shlex`.
//!
//! The shell policy tokenizes each extracted command part the way the reference
//! does before it reads options and operands: `shlex.split` on a POSIX host,
//! and on Windows a POSIX-mode lexer that splits on whitespace, keeps comments
//! and has no escape character (`_split_command_tokens` in
//! `vibe/core/tools/builtins/bash.py` and
//! `vibe/core/tools/builtins/experimental_bash.py`). A split that does not close
//! answers [`None`], which the callers read the way the reference reads its
//! `ValueError`: as a plain whitespace split.

/// Which of the two lexer configurations the reference builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WordSplit {
    /// `shlex.split`: a backslash escapes, and `#` is an ordinary character.
    Posix,
    /// The Windows lexer: a backslash is literal, so `C:\Users` keeps its
    /// separators, and `#` starts a comment that runs to the end of the line.
    LiteralBackslash,
}

const WHITESPACE: [char; 4] = [' ', '\t', '\r', '\n'];

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Between,
    Word,
    Quote(char),
    Escape(Escaped),
}

/// Where an escape returns to once it has read its character.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Escaped {
    Word,
    DoubleQuote,
}

/// The words of `text`, or [`None`] when a quote or an escape does not close.
pub(crate) fn split_words(text: &str, mode: WordSplit) -> Option<Vec<String>> {
    let escapes = mode == WordSplit::Posix;
    let comments = mode == WordSplit::LiteralBackslash;
    let mut words = Vec::new();
    let mut chars = text.chars();
    loop {
        let mut token = String::new();
        let mut quoted = false;
        let mut state = State::Between;
        let mut ended = false;
        loop {
            let next = chars.next();
            match state {
                State::Between => match next {
                    None => {
                        ended = true;
                        break;
                    }
                    Some(character) if WHITESPACE.contains(&character) => {}
                    Some('#') if comments => skip_line(&mut chars),
                    Some('\\') if escapes => state = State::Escape(Escaped::Word),
                    Some(quote @ ('\'' | '"')) => state = State::Quote(quote),
                    Some(character) => {
                        token.push(character);
                        state = State::Word;
                    }
                },
                State::Word => match next {
                    None => {
                        ended = true;
                        break;
                    }
                    Some(character) if WHITESPACE.contains(&character) => break,
                    Some('#') if comments => {
                        skip_line(&mut chars);
                        if !token.is_empty() || quoted {
                            break;
                        }
                        state = State::Between;
                    }
                    Some(quote @ ('\'' | '"')) => state = State::Quote(quote),
                    Some('\\') if escapes => state = State::Escape(Escaped::Word),
                    Some(character) => token.push(character),
                },
                State::Quote(quote) => {
                    quoted = true;
                    match next {
                        None => return None,
                        Some(character) if character == quote => state = State::Word,
                        Some('\\') if escapes && quote == '"' => {
                            state = State::Escape(Escaped::DoubleQuote);
                        }
                        Some(character) => token.push(character),
                    }
                }
                State::Escape(escaped) => {
                    let character = next?;
                    // Inside double quotes only a quote or a backslash is
                    // escaped; any other character keeps the backslash.
                    if escaped == Escaped::DoubleQuote && character != '\\' && character != '"' {
                        token.push('\\');
                    }
                    token.push(character);
                    state = match escaped {
                        Escaped::Word => State::Word,
                        Escaped::DoubleQuote => State::Quote('"'),
                    };
                }
            }
        }
        if !token.is_empty() || quoted {
            words.push(token);
        }
        if ended {
            return Some(words);
        }
    }
}

/// Consumes the rest of the line a comment started on.
fn skip_line(chars: &mut std::str::Chars<'_>) {
    for character in chars.by_ref() {
        if character == '\n' {
            break;
        }
    }
}

/// The words of `text`, falling back to a whitespace split as the reference
/// does when the quoting does not close.
pub(crate) fn split_tokens(text: &str, mode: WordSplit) -> Vec<String> {
    split_words(text, mode).unwrap_or_else(|| whitespace_words(text))
}

/// Python's `str.split()` with no separator.
pub(crate) fn whitespace_words(text: &str) -> Vec<String> {
    text.split_whitespace().map(ToOwned::to_owned).collect()
}

#[cfg(test)]
mod lexer_tests;
