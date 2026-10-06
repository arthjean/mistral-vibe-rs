//! The global dotenv file.
//!
//! Reference `load_dotenv_values` reads `{vibe_home}/.env` at startup so an API
//! key kept there is visible to the process, with a non-empty value already in
//! the environment winning and a FIFO accepted in place of a regular file.
//!
//! The reference mutates `os.environ`; this port cannot. `std::env::set_var` is
//! `unsafe` under edition 2024 and `unsafe_code` is forbidden workspace-wide,
//! so the file's values are resolved through [`DotenvValues::variable`] and
//! [`DotenvValues::environment`] instead. Every credential reader and the
//! `VIBE_*` environment layer go through them, which is where the reference
//! makes the values observable.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

const DOTENV_FILE: &str = ".env";

/// The path of the global dotenv, `{vibe_home}/.env`, shared by the readers
/// here and by the credential fallback writer in `crate::auth`.
#[must_use]
pub fn global_env_file(vibe_home: &Path) -> std::path::PathBuf {
    vibe_home.join(DOTENV_FILE)
}

/// The variables a dotenv file declares, and how they combine with the process
/// environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DotenvValues {
    values: BTreeMap<String, String>,
}

impl DotenvValues {
    /// Reads `{vibe_home}/.env`. A missing, unreadable or malformed file yields
    /// no values rather than an error: startup proceeds either way.
    #[must_use]
    pub fn global(vibe_home: &Path) -> Self {
        Self::load(&global_env_file(vibe_home))
    }

    /// Reads one dotenv file.
    ///
    /// The path is opened rather than stat-ed, so a FIFO standing in for the
    /// file is read like any other source.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let Ok(contents) = fs::read_to_string(path) else {
            return Self::default();
        };
        Self::parse(&contents)
    }

    /// Parses dotenv text as python-dotenv's `dotenv_values` reads it, which
    /// is what reference `load_dotenv_values` calls: an optional `export`, a
    /// bare or single-quoted key, a single-quoted, double-quoted or bare value,
    /// comments, quoted values spanning lines, and `${NAME}` or
    /// `${NAME:-default}` expanded against the process environment and the
    /// lines above. A line that does not parse is skipped, a later line wins
    /// over an earlier one, and an empty value leaves the variable unset.
    #[must_use]
    pub fn parse(contents: &str) -> Self {
        let mut expanded: BTreeMap<String, Option<String>> = BTreeMap::new();
        for (key, value) in bindings(contents) {
            let value = value.map(|value| interpolate(&value, &expanded));
            expanded.insert(key, value);
        }
        let values = expanded
            .into_iter()
            .filter_map(|(key, value)| Some((key, value.filter(|value| !value.is_empty())?)))
            .collect();
        Self { values }
    }

    /// The value `name` resolves to: the process value when it is set and not
    /// empty, otherwise the file's.
    #[must_use]
    pub fn variable(&self, name: &str) -> Option<String> {
        resolve(std::env::var(name).ok(), self.values.get(name))
    }

    /// The value the file alone declares for `name`, without consulting the
    /// process. What the auth-state assessment reads, because it classifies
    /// the file as a source distinct from the environment.
    #[must_use]
    pub fn file_variable(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// The process environment with the file's variables filled in where the
    /// process has none, which is the environment the reference leaves behind
    /// after loading the file.
    #[must_use]
    pub fn environment(&self) -> BTreeMap<String, String> {
        let mut merged = self.values.clone();
        for (key, value) in std::env::vars() {
            if !value.is_empty() {
                merged.insert(key, value);
            }
        }
        merged
    }
}

/// What [`DotenvValues::publish_to_children`] published, read by every spawn
/// that inherits the process environment.
static INHERITED: OnceLock<BTreeMap<String, String>> = OnceLock::new();

impl DotenvValues {
    /// Makes the file's values part of the environment every child process
    /// inherits from here on, as reference `load_dotenv_values` makes them part
    /// of `os.environ` at startup: a shell command, a terminal session and a
    /// hook all see a variable the file declares and the process does not.
    ///
    /// The process environment cannot be written (`std::env::set_var` is
    /// `unsafe` and `unsafe_code` is forbidden), so the spawns read the
    /// published values instead. The first publication wins, as the reference
    /// loads the file once.
    pub fn publish_to_children(&self) {
        let _ = INHERITED.set(self.values.clone());
    }
}

/// The published dotenv variables a child inherits: those the process
/// environment leaves unset or empty, since an explicit non-empty value wins.
#[must_use]
pub fn inherited_by_children() -> Vec<(String, String)> {
    INHERITED
        .get()
        .into_iter()
        .flatten()
        .filter(|(key, _)| std::env::var(key).map_or(true, |value| value.is_empty()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The precedence rule: an explicit non-empty process value wins over the file,
/// and an empty or absent one falls back on it. Reference `load_dotenv_values`,
/// which skips a key `os.environ` already answers with a truthy value.
pub(super) fn resolve(process: Option<String>, file: Option<&String>) -> Option<String> {
    process
        .filter(|value| !value.is_empty())
        .or_else(|| file.cloned())
}

/// The grammar python-dotenv's `parser.py` reads, one pattern per step. Each
/// is anchored, because it is matched against the text that is left.
struct Grammar {
    multiline_whitespace: Regex,
    whitespace: Regex,
    export: Regex,
    single_quoted_key: Regex,
    unquoted_key: Regex,
    equal_sign: Regex,
    single_quoted_value: Regex,
    double_quoted_value: Regex,
    unquoted_value: Regex,
    comment: Regex,
    end_of_line: Regex,
    rest_of_line: Regex,
    trailing_comment: Regex,
    variable: Regex,
}

fn grammar() -> &'static Grammar {
    static GRAMMAR: OnceLock<Grammar> = OnceLock::new();
    GRAMMAR.get_or_init(|| {
        // Every pattern is a literal checked by the suite, so a failure to
        // compile is a programming error rather than an input condition.
        let compile = |pattern: &str| {
            Regex::new(pattern).unwrap_or_else(|_| unreachable!("dotenv pattern {pattern}"))
        };
        Grammar {
            multiline_whitespace: compile(r"^\s*"),
            whitespace: compile(r"^[^\S\r\n]*"),
            export: compile(r"^(?:export[^\S\r\n]+)?"),
            single_quoted_key: compile(r"^'([^']+)'"),
            unquoted_key: compile(r"^([^=#\s]+)"),
            equal_sign: compile(r"^=[^\S\r\n]*"),
            single_quoted_value: compile(r"^'((?:\\'|[^'])*)'"),
            double_quoted_value: compile(r#"^"((?:\\"|[^"])*)""#),
            unquoted_value: compile(r"^[^\r\n]*"),
            comment: compile(r"^(?:[^\S\r\n]*#[^\r\n]*)?"),
            end_of_line: compile(r"^[^\S\r\n]*(?:\r\n|\n|\r|$)"),
            rest_of_line: compile(r"^[^\r\n]*(?:\r\n|\r|\n)?"),
            trailing_comment: compile(r"\s+#.*"),
            variable: compile(r"\$\{([^}:]*)(?::-([^}]*))?\}"),
        }
    })
}

/// A position in the text being read, and the patterns it is read with.
struct Reader<'a> {
    text: &'a str,
    position: usize,
    grammar: &'static Grammar,
}

impl<'a> Reader<'a> {
    fn rest(&self) -> &'a str {
        self.text.get(self.position..).unwrap_or_default()
    }

    /// Consumes what `pattern` matches at the position, or `None` when it
    /// matches nothing there, leaving the position where it was.
    fn read(&mut self, pattern: &Regex) -> Option<regex::Captures<'a>> {
        let rest = self.rest();
        let captures = pattern.captures(rest)?;
        self.position += captures.get(0).map_or(0, |whole| whole.end());
        Some(captures)
    }

    fn group(&mut self, pattern: &Regex) -> Option<String> {
        self.read(pattern)
            .and_then(|captures| captures.get(1).map(|group| group.as_str().to_owned()))
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }
}

/// Every binding the text declares, in order, skipping comments, blank lines
/// and lines that do not parse. A key with no `=` binds to `None`.
fn bindings(contents: &str) -> Vec<(String, Option<String>)> {
    let mut reader = Reader {
        text: contents,
        position: 0,
        grammar: grammar(),
    };
    let mut found = Vec::new();
    while reader.position < contents.len() {
        match binding(&mut reader) {
            Ok(Some(entry)) => found.push(entry),
            Ok(None) => {}
            Err(()) => {
                reader.read(&reader.grammar.rest_of_line);
            }
        }
    }
    found
}

/// One binding, `Ok(None)` for a comment or the trailing whitespace, and
/// `Err` where the line does not parse.
fn binding(reader: &mut Reader<'_>) -> Result<Option<(String, Option<String>)>, ()> {
    let grammar = reader.grammar;
    reader.read(&grammar.multiline_whitespace);
    if reader.position >= reader.text.len() {
        return Ok(None);
    }
    reader.read(&grammar.export);
    let key = match reader.peek() {
        Some('#') => None,
        Some('\'') => Some(reader.group(&grammar.single_quoted_key).ok_or(())?),
        _ => Some(reader.group(&grammar.unquoted_key).ok_or(())?),
    };
    reader.read(&grammar.whitespace);
    let value = if reader.peek() == Some('=') {
        reader.read(&grammar.equal_sign);
        Some(value(reader)?)
    } else {
        None
    };
    reader.read(&grammar.comment);
    reader.read(&grammar.end_of_line).ok_or(())?;
    Ok(key.map(|key| (key, value)))
}

fn value(reader: &mut Reader<'_>) -> Result<String, ()> {
    let grammar = reader.grammar;
    match reader.peek() {
        Some('\'') => reader
            .group(&grammar.single_quoted_value)
            .map(|value| decode_escapes(&value, &['\\', '\''])),
        Some('"') => reader.group(&grammar.double_quoted_value).map(|value| {
            decode_escapes(
                &value,
                &['\\', '\'', '"', 'a', 'b', 'f', 'n', 'r', 't', 'v'],
            )
        }),
        None | Some('\n' | '\r') => Some(String::new()),
        Some(_) => reader.read(&grammar.unquoted_value).map(|captures| {
            let raw = captures.get(0).map_or("", |whole| whole.as_str());
            grammar
                .trailing_comment
                .replace(raw, "")
                .trim_end()
                .to_owned()
        }),
    }
    .ok_or(())
}

/// Decodes the backslash escapes a quoted value admits, leaving any other
/// backslash as written.
fn decode_escapes(value: &str, admitted: &[char]) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match characters
            .peek()
            .copied()
            .filter(|next| admitted.contains(next))
        {
            Some(escaped) => {
                characters.next();
                decoded.push(match escaped {
                    'a' => '\u{7}',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    'v' => '\u{b}',
                    other => other,
                });
            }
            None => decoded.push(character),
        }
    }
    decoded
}

/// Expands `${NAME}` and `${NAME:-default}` as python-dotenv's
/// `resolve_variables` does with `override=True`: a name the file declared
/// above wins over the process environment, and a name neither knows reads as
/// its default or as nothing.
fn interpolate(value: &str, declared: &BTreeMap<String, Option<String>>) -> String {
    grammar()
        .variable
        .replace_all(value, |captures: &regex::Captures<'_>| {
            let name = captures.get(1).map_or("", |name| name.as_str());
            match declared.get(name) {
                Some(known) => known.clone().unwrap_or_default(),
                None => std::env::var(name).ok().unwrap_or_else(|| {
                    captures
                        .get(2)
                        .map_or_else(String::new, |default| default.as_str().to_owned())
                }),
            }
        })
        .into_owned()
}
