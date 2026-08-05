//! Addressing configuration values by JSON Pointer.
//!
//! The protocol expresses a configuration write as a pointer plus an operation,
//! so the store speaks the same language. Two operations reach a layer document
//! from the wire, [`PatchOperation::Set`] and [`PatchOperation::Remove`], joined
//! by the [`PatchOperation::Replace`] that [`resolve_upsert`] produces when an
//! entry lands back in a list it already belongs to. Reference
//! `vibe/core/config/patch.py`.
//!
//! Pointer syntax is RFC 6901: an empty string addresses the whole document,
//! every other pointer starts with `/`, and `~1` is decoded before `~0` so a
//! token spelling a literal `~1` survives the round trip.

use std::fmt::Write as _;

use thiserror::Error;
use toml::{Table, Value};

/// A parsed RFC 6901 pointer: the spelling that crossed the wire, and the
/// tokens it decodes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPointer {
    raw: String,
    tokens: Vec<String>,
}

impl JsonPointer {
    /// Parses `raw`, which is either empty or starts with `/`.
    pub fn parse(raw: &str) -> Result<Self, PatchError> {
        if raw.is_empty() {
            return Ok(Self {
                raw: String::new(),
                tokens: Vec::new(),
            });
        }
        let Some(body) = raw.strip_prefix('/') else {
            return Err(PatchError::InvalidPointer {
                pointer: raw.to_owned(),
            });
        };
        Ok(Self {
            raw: raw.to_owned(),
            tokens: body.split('/').map(decode_token).collect(),
        })
    }

    /// Builds a pointer from decoded segments, escaping each one.
    #[must_use]
    pub fn from_segments(segments: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let tokens: Vec<String> = segments.into_iter().map(Into::into).collect();
        let mut raw = String::new();
        for token in &tokens {
            // Writing into a `String` cannot fail; discarding the result keeps
            // this free of a panicking unwrap.
            let _ = write!(raw, "/{}", escape_token(token));
        }
        Self { raw, tokens }
    }

    /// The pointer as it crosses the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// This pointer with `token` appended.
    fn child(&self, token: &str) -> Self {
        let mut tokens = self.tokens.clone();
        tokens.push(token.to_owned());
        Self {
            raw: format!("{}/{}", self.raw, escape_token(token)),
            tokens,
        }
    }
}

/// Escapes one pointer token: `~` first, so an escaped `/` is not escaped twice.
#[must_use]
pub(super) fn escape_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// Decodes one pointer token: `~1` before `~0`, as RFC 6901 requires.
fn decode_token(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

/// The three operations a configuration document accepts.
#[derive(Debug, Clone, PartialEq)]
pub enum PatchOperation {
    /// Insert or overwrite. On a list, a numeric token inserts at that position
    /// and `-` appends, as the RFC 6902 `add` this port maps `set` onto does.
    Set(Value),
    /// Overwrite a value that must already exist.
    Replace(Value),
    /// Delete a value that must already exist.
    Remove,
}

/// One addressed change to a configuration document.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigMutation {
    pub pointer: JsonPointer,
    pub operation: PatchOperation,
}

impl ConfigMutation {
    /// Sets the value at a path of literal keys.
    #[must_use]
    pub fn set(path: impl IntoIterator<Item = impl Into<String>>, value: Value) -> Self {
        Self {
            pointer: JsonPointer::from_segments(path),
            operation: PatchOperation::Set(value),
        }
    }

    /// Removes the value at a path of literal keys.
    #[must_use]
    pub fn remove(path: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            pointer: JsonPointer::from_segments(path),
            operation: PatchOperation::Remove,
        }
    }

    /// The mutation an addressed operation expresses.
    #[must_use]
    pub const fn new(pointer: JsonPointer, operation: PatchOperation) -> Self {
        Self { pointer, operation }
    }
}

/// Applies every mutation to a copy of `document` and returns the result.
///
/// The input is never touched, so a failure part way through leaves the caller's
/// document byte-identical; the reference obtains the same guarantee by deep
/// copying before it patches.
pub fn apply_all(document: &Table, mutations: &[ConfigMutation]) -> Result<Table, PatchError> {
    let mut draft = document.clone();
    ensure_parent_paths(&mut draft, mutations);
    for mutation in mutations {
        apply(&mut draft, mutation)?;
    }
    Ok(draft)
}

/// Creates the intermediate tables a `set` needs, so a leaf like
/// `/tools/bash/allowlist` lands on a document carrying no `[tools.bash]`.
///
/// Reference `ensure_parent_paths`: index and append tokens stop the walk, and
/// an existing non-table parent is left for [`apply`] to reject.
fn ensure_parent_paths(document: &mut Table, mutations: &[ConfigMutation]) {
    for mutation in mutations {
        if !matches!(mutation.operation, PatchOperation::Set(_)) {
            continue;
        }
        let Some((_, parents)) = mutation.pointer.tokens.split_last() else {
            continue;
        };
        let mut cursor = &mut *document;
        for token in parents {
            if token == "-" || token.parse::<usize>().is_ok() {
                break;
            }
            let entry = cursor
                .entry(token.clone())
                .or_insert_with(|| Value::Table(Table::new()));
            let Some(table) = entry.as_table_mut() else {
                break;
            };
            cursor = table;
        }
    }
}

/// Applies one mutation in place. A failure can leave the document partially
/// patched, which is why [`apply_all`] works on a copy.
fn apply(document: &mut Table, mutation: &ConfigMutation) -> Result<(), PatchError> {
    let pointer = &mutation.pointer;
    let Some((leaf, parents)) = pointer.tokens.split_last() else {
        return Err(PatchError::WholeDocument);
    };
    let mut cursor = Cursor::Table(document);
    for token in parents {
        cursor = cursor.descend(token, pointer)?;
    }
    match cursor {
        Cursor::Table(table) => apply_to_table(table, leaf, mutation),
        Cursor::Array(entries) => apply_to_array(entries, leaf, mutation),
    }
}

fn apply_to_table(
    table: &mut Table,
    leaf: &str,
    mutation: &ConfigMutation,
) -> Result<(), PatchError> {
    let missing = || PatchError::MissingPath {
        pointer: mutation.pointer.as_str().to_owned(),
    };
    match &mutation.operation {
        PatchOperation::Set(value) => {
            table.insert(leaf.to_owned(), value.clone());
        }
        PatchOperation::Replace(value) => {
            if !table.contains_key(leaf) {
                return Err(missing());
            }
            table.insert(leaf.to_owned(), value.clone());
        }
        PatchOperation::Remove => {
            table.remove(leaf).ok_or_else(missing)?;
        }
    }
    Ok(())
}

fn apply_to_array(
    entries: &mut Vec<Value>,
    leaf: &str,
    mutation: &ConfigMutation,
) -> Result<(), PatchError> {
    let pointer = || mutation.pointer.as_str().to_owned();
    if leaf == "-" {
        return match &mutation.operation {
            PatchOperation::Set(value) => {
                entries.push(value.clone());
                Ok(())
            }
            // `-` names the position after the last entry: nothing lives there
            // to overwrite or delete.
            PatchOperation::Replace(_) | PatchOperation::Remove => {
                Err(PatchError::MissingPath { pointer: pointer() })
            }
        };
    }
    let index = leaf
        .parse::<usize>()
        .map_err(|_| PatchError::ArrayIndex { pointer: pointer() })?;
    match &mutation.operation {
        // The RFC 6902 `add` this port maps `set` onto inserts, and accepts one
        // position past the end as an append.
        PatchOperation::Set(value) => {
            if index > entries.len() {
                return Err(PatchError::IndexOutOfRange { pointer: pointer() });
            }
            entries.insert(index, value.clone());
        }
        PatchOperation::Replace(value) => {
            let entry = entries
                .get_mut(index)
                .ok_or_else(|| PatchError::IndexOutOfRange { pointer: pointer() })?;
            *entry = value.clone();
        }
        PatchOperation::Remove => {
            if index >= entries.len() {
                return Err(PatchError::IndexOutOfRange { pointer: pointer() });
            }
            entries.remove(index);
        }
    }
    Ok(())
}

/// A borrowed position inside the document being patched.
enum Cursor<'a> {
    Table(&'a mut Table),
    Array(&'a mut Vec<Value>),
}

impl<'a> Cursor<'a> {
    fn descend(self, token: &str, pointer: &JsonPointer) -> Result<Self, PatchError> {
        let value = match self {
            Self::Table(table) => table
                .get_mut(token)
                .ok_or_else(|| PatchError::MissingPath {
                    pointer: pointer.as_str().to_owned(),
                })?,
            Self::Array(entries) => {
                let index = token.parse::<usize>().map_err(|_| PatchError::ArrayIndex {
                    pointer: pointer.as_str().to_owned(),
                })?;
                entries
                    .get_mut(index)
                    .ok_or_else(|| PatchError::IndexOutOfRange {
                        pointer: pointer.as_str().to_owned(),
                    })?
            }
        };
        match value {
            Value::Table(table) => Ok(Self::Table(table)),
            Value::Array(entries) => Ok(Self::Array(entries)),
            // A scalar carries no fields, and overwriting it would discard a
            // value the operator set on purpose.
            _ => Err(PatchError::NonTableParent {
                pointer: pointer.as_str().to_owned(),
            }),
        }
    }
}

/// Picks the operation that lands `entry` in the list at `pointer`.
///
/// Reference `resolve_upsert_op`: an existing entry whose `key_field` matches is
/// replaced where it stands, anything else is appended, and a missing, empty or
/// non-list target is created holding this entry alone.
#[must_use]
pub fn resolve_upsert(
    existing: Option<&Value>,
    pointer: &JsonPointer,
    key_field: &str,
    entry: Table,
) -> ConfigMutation {
    let identity = entry.get(key_field);
    let entries = existing
        .and_then(Value::as_array)
        .filter(|entries| !entries.is_empty());
    let Some(entries) = entries else {
        return ConfigMutation::new(
            pointer.clone(),
            PatchOperation::Set(Value::Array(vec![Value::Table(entry)])),
        );
    };
    let position = entries.iter().position(|candidate| {
        candidate
            .as_table()
            .is_some_and(|candidate| candidate.get(key_field) == identity)
    });
    position.map_or_else(
        || {
            ConfigMutation::new(
                pointer.child("-"),
                PatchOperation::Set(Value::Table(entry.clone())),
            )
        },
        |index| {
            ConfigMutation::new(
                pointer.child(&index.to_string()),
                PatchOperation::Replace(Value::Table(entry.clone())),
            )
        },
    )
}

/// Why a pointer could not be resolved.
///
/// Every variant names the pointer and nothing else: a configuration value can
/// carry a token or a URL with credentials, and an operator only needs to know
/// which address failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PatchError {
    #[error("`{pointer}` is not a JSON Pointer; it must be empty or start with `/`")]
    InvalidPointer { pointer: String },
    #[error("a configuration operation must address a field, not the whole document")]
    WholeDocument,
    #[error("configuration path `{pointer}` does not exist")]
    MissingPath { pointer: String },
    #[error("configuration path `{pointer}` traverses a value that holds no fields")]
    NonTableParent { pointer: String },
    #[error("configuration path `{pointer}` addresses a list entry by a non-numeric key")]
    ArrayIndex { pointer: String },
    #[error("configuration path `{pointer}` is past the end of its list")]
    IndexOutOfRange { pointer: String },
}
