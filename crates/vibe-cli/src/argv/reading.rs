//! argparse's reading of an argument vector, replayed ahead of clap.
//!
//! The two parsers disagree on what a token is before they disagree on what it
//! means. CPython's `argparse` classifies every token first
//! (`ArgumentParser._parse_optional`): a long option may be abbreviated to any
//! unambiguous prefix, a token shaped like a negative number or holding a
//! space is a value rather than an option, and a value is then consumed by the
//! option before it whatever it looks like. clap reads `-5` as a short flag,
//! refuses an abbreviation unless told to infer one, and infers over hidden
//! arguments too, which would make `--p` ambiguous with this port's own
//! `--provider-style`.
//!
//! So the vector is read here the way argparse reads it, and rewritten into a
//! spelling clap cannot misread: every option under its full long name, a value
//! that starts with a hyphen joined to its option by `=`, and every positional
//! moved behind one `--` in the order it was given. The declaration stays
//! clap's, so the help, the conversions and the conflicts are still clap's to
//! enforce. The one refusal clap cannot phrase, an ambiguous abbreviation, is
//! reported here, and the vector is cut before it so that anything argparse
//! would have refused first is still refused first.

use std::ffi::OsString;

use clap::{Arg, ArgAction, Command};

/// How many values an option takes, in argparse's three shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arity {
    /// A flag: `store_true`, `help`, `version`.
    Zero,
    /// `nargs=None`: exactly one value.
    One,
    /// `nargs="?"`: the next value when there is one.
    Optional,
}

/// One option as argparse registers it.
#[derive(Debug)]
struct Declared {
    /// Every spelling, shorts first and in declaration order, which is the
    /// order argparse lists them in an ambiguity.
    spellings: Vec<String>,
    /// The spelling handed to clap: the long name where there is one.
    canonical: String,
    arity: Arity,
    /// Whether an abbreviation may resolve to it. The arguments only this port
    /// declares answer to their exact spelling alone, so they never take a
    /// prefix the reference resolves elsewhere.
    abbreviable: bool,
}

/// What argparse makes of one token.
#[derive(Debug)]
enum Token {
    /// A value, for an option or a positional.
    Value,
    /// The first `--`, after which every token is a value.
    Separator,
    /// A declared option, with the value an `=` attached to it.
    Option {
        declared: usize,
        explicit: Option<String>,
    },
    /// A short option with its value or its neighbors in the same token
    /// (`-pvalue`, `-cp`), which clap splits the way argparse does.
    Cluster,
    /// A prefix of more than one option.
    Ambiguous(Vec<String>),
    /// Shaped like an option, and matching none.
    Unknown,
}

/// The vector rewritten for clap, and the refusal argparse would have raised
/// where the rewrite stopped.
#[derive(Debug)]
pub(crate) struct Reading {
    pub(crate) argv: Vec<OsString>,
    /// argparse's message, without the `prog: error:` prefix.
    pub(crate) refusal: Option<String>,
}

/// Reads `argv`, program name first, against `command`'s options.
///
/// `exact_only` names the arguments, by id, that no abbreviation may reach.
pub(crate) fn read(command: &Command, argv: Vec<OsString>, exact_only: &[&str]) -> Reading {
    let declared = declarations(command, exact_only);
    let mut argv = argv.into_iter();
    let mut rewritten: Vec<OsString> = argv.next().into_iter().collect();
    let tokens: Vec<OsString> = argv.collect();
    let kinds = classify_all(&tokens, &declared);
    let mut positionals: Vec<OsString> = Vec::new();
    let mut refusal = None;
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        index += 1;
        match &kinds[index - 1] {
            Token::Value => positionals.push(token.clone()),
            Token::Separator => {}
            Token::Cluster | Token::Unknown => rewritten.push(token.clone()),
            Token::Ambiguous(matches) => {
                refusal = Some(format!(
                    "ambiguous option: {} could match {}",
                    token.to_string_lossy(),
                    matches.join(", ")
                ));
                break;
            }
            Token::Option {
                declared: which,
                explicit,
            } => {
                let option = &declared[*which];
                if let Some(explicit) = explicit {
                    if option.arity == Arity::Zero {
                        // `_parse_known_args.consume_optional`: a flag given
                        // `=value` is refused rather than stripped.
                        refusal = Some(format!(
                            "argument {}: ignored explicit argument {}",
                            option.spellings.join("/"),
                            python_repr(explicit)
                        ));
                        break;
                    }
                    rewritten.push(OsString::from(format!("{}={explicit}", option.canonical)));
                    continue;
                }
                let takes_next =
                    option.arity != Arity::Zero && matches!(kinds.get(index), Some(Token::Value));
                if !takes_next {
                    rewritten.push(OsString::from(&option.canonical));
                    continue;
                }
                let value = &tokens[index];
                index += 1;
                if value.as_encoded_bytes().first() == Some(&b'-') {
                    let mut joined = OsString::from(format!("{}=", option.canonical));
                    joined.push(value);
                    rewritten.push(joined);
                } else {
                    rewritten.push(OsString::from(&option.canonical));
                    rewritten.push(value.clone());
                }
            }
        }
    }
    if !positionals.is_empty() {
        rewritten.push(OsString::from("--"));
        rewritten.extend(positionals);
    }
    Reading {
        argv: rewritten,
        refusal,
    }
}

fn declarations(command: &Command, exact_only: &[&str]) -> Vec<Declared> {
    command
        .get_arguments()
        .filter(|argument| !argument.is_positional())
        .filter_map(|argument| {
            let mut spellings: Vec<String> = Vec::new();
            if let Some(short) = argument.get_short() {
                spellings.push(format!("-{short}"));
            }
            for short in argument.get_all_short_aliases().unwrap_or_default() {
                spellings.push(format!("-{short}"));
            }
            if let Some(long) = argument.get_long() {
                spellings.push(format!("--{long}"));
            }
            for alias in argument.get_all_aliases().unwrap_or_default() {
                spellings.push(format!("--{alias}"));
            }
            let canonical = argument
                .get_long()
                .map(|long| format!("--{long}"))
                .or_else(|| spellings.first().cloned())?;
            Some(Declared {
                spellings,
                canonical,
                arity: arity(argument),
                abbreviable: !exact_only.contains(&argument.get_id().as_str()),
            })
        })
        .collect()
}

fn arity(argument: &Arg) -> Arity {
    if matches!(
        argument.get_action(),
        ArgAction::SetTrue
            | ArgAction::SetFalse
            | ArgAction::Count
            | ArgAction::Help
            | ArgAction::HelpShort
            | ArgAction::HelpLong
            | ArgAction::Version
    ) {
        return Arity::Zero;
    }
    match argument.get_num_args() {
        Some(range) if range.max_values() == 0 => Arity::Zero,
        Some(range) if range.min_values() == 0 => Arity::Optional,
        _ => Arity::One,
    }
}

fn classify_all(tokens: &[OsString], declared: &[Declared]) -> Vec<Token> {
    let mut kinds = Vec::with_capacity(tokens.len());
    let mut separated = false;
    for token in tokens {
        if separated {
            kinds.push(Token::Value);
        } else if token == "--" {
            separated = true;
            kinds.push(Token::Separator);
        } else {
            kinds.push(classify(token, declared));
        }
    }
    kinds
}

/// `ArgumentParser._parse_optional` and `_get_option_tuples`, CPython 3.12.
fn classify(token: &OsString, declared: &[Declared]) -> Token {
    let Some(text) = token.to_str() else {
        // A token that is not text is passed through: shaped like an option,
        // clap refuses it by name; otherwise it is a value.
        return if token.as_encoded_bytes().first() == Some(&b'-') {
            Token::Unknown
        } else {
            Token::Value
        };
    };
    if !text.starts_with('-') {
        return Token::Value;
    }
    if let Some(which) = exact(declared, text) {
        return Token::Option {
            declared: which,
            explicit: None,
        };
    }
    if text.chars().count() == 1 {
        return Token::Value;
    }
    let (option, explicit) = match text.split_once('=') {
        Some((option, explicit)) => (option, Some(explicit.to_owned())),
        None => (text, None),
    };
    if explicit.is_some()
        && let Some(which) = exact(declared, option)
    {
        return Token::Option {
            declared: which,
            explicit,
        };
    }
    if text.starts_with("--") {
        let mut matches: Vec<(usize, &str)> = Vec::new();
        for (which, entry) in declared.iter().enumerate() {
            if !entry.abbreviable {
                continue;
            }
            for spelling in &entry.spellings {
                if spelling.starts_with(option) {
                    matches.push((which, spelling));
                }
            }
        }
        match matches.as_slice() {
            [] => {}
            [(which, _)] => {
                return Token::Option {
                    declared: *which,
                    explicit,
                };
            }
            many => {
                return Token::Ambiguous(
                    many.iter()
                        .map(|(_, spelling)| (*spelling).to_owned())
                        .collect(),
                );
            }
        }
    } else {
        // A single-dash token opening with a declared short is that short with
        // the rest of the token after it.
        let short: String = text.chars().take(2).collect();
        if exact(declared, &short).is_some() {
            return Token::Cluster;
        }
    }
    if looks_like_a_negative_number(text) || text.contains(' ') {
        return Token::Value;
    }
    Token::Unknown
}

fn exact(declared: &[Declared], spelling: &str) -> Option<usize> {
    declared
        .iter()
        .position(|entry| entry.spellings.iter().any(|known| known == spelling))
}

/// argparse's `_negative_number_matcher`: `^-\d+$|^-\d*\.\d+$`.
fn looks_like_a_negative_number(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('-') else {
        return false;
    };
    let digits = |part: &str| part.chars().all(|character| character.is_ascii_digit());
    match rest.split_once('.') {
        None => !rest.is_empty() && digits(rest),
        Some((whole, fraction)) => !fraction.is_empty() && digits(whole) && digits(fraction),
    }
}

/// Python's `repr` of a string, which is how argparse quotes a value it names.
pub(crate) fn python_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut rendered = String::with_capacity(value.len() + 2);
    rendered.push(quote);
    for character in value.chars() {
        match character {
            '\\' => rendered.push_str("\\\\"),
            '\n' => rendered.push_str("\\n"),
            '\r' => rendered.push_str("\\r"),
            '\t' => rendered.push_str("\\t"),
            character if character == quote => {
                rendered.push('\\');
                rendered.push(character);
            }
            character if character.is_control() => {
                rendered.push_str(&format!("\\x{:02x}", u32::from(character)));
            }
            character => rendered.push(character),
        }
    }
    rendered.push(quote);
    rendered
}

#[cfg(test)]
mod reading_tests;
