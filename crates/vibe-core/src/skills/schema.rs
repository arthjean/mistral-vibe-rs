//! The skill metadata schema, validating a parsed frontmatter mapping the way
//! the reference's model does (`vibe/core/skills/models.py:37-93` at the
//! pinned commit).
//!
//! The semantics reproduced here are the reference validator's lax mode, not a
//! stricter Rust reading: both hyphenated aliases are accepted and win over
//! the underscore spellings when both appear, `allowed-tools` coerces from a
//! space-delimited string, `metadata` values normalize to their Python string
//! forms, and `user-invocable` accepts the validator's whole boolean word list
//! from a string, which is wider than the YAML vocabulary and includes the
//! single letters.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use thiserror::Error;

use crate::pycompat::pydantic::lax_bool_value;

/// A frontmatter mapping the schema rejected, with the field and the reason.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct SkillValidationError(String);

impl SkillValidationError {
    fn new(field: &str, reason: &str) -> Self {
        Self(format!("`{field}` {reason}"))
    }
}

/// The validated frontmatter, carrying all eight fields the reference schema
/// declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub allowed_tools: Vec<String>,
    pub user_invocable: bool,
    /// Reference v2.25.5 `disable-model-invocation`: the skill stays in the
    /// slash menu but the model can neither see nor load it.
    pub disable_model_invocation: bool,
}

impl SkillMetadata {
    /// Validates one parsed frontmatter mapping. Unknown keys are ignored, as
    /// the reference model ignores them.
    pub fn validate(frontmatter: &Map<String, Value>) -> Result<Self, SkillValidationError> {
        let name = required_text(frontmatter, "name", 64)?;
        if !valid_skill_name(&name) {
            return Err(SkillValidationError::new(
                "name",
                "must be lowercase words of letters and digits joined by single hyphens",
            ));
        }
        let description = required_text(frontmatter, "description", 1024)?;
        let license = optional_text(frontmatter, "license", usize::MAX)?;
        let compatibility = optional_text(frontmatter, "compatibility", 500)?;
        let metadata = normalized_metadata(frontmatter.get("metadata"))?;
        let allowed_tools =
            parsed_allowed_tools(aliased(frontmatter, "allowed-tools", "allowed_tools"))?;
        let user_invocable = match aliased(frontmatter, "user-invocable", "user_invocable") {
            None => true,
            Some(value) => lax_bool_value(value)
                .ok_or_else(|| SkillValidationError::new("user-invocable", "must be a boolean"))?,
        };
        let disable_model_invocation = match aliased(
            frontmatter,
            "disable-model-invocation",
            "disable_model_invocation",
        ) {
            None => false,
            Some(value) => lax_bool_value(value).ok_or_else(|| {
                SkillValidationError::new("disable-model-invocation", "must be a boolean")
            })?,
        };
        Ok(Self {
            name,
            description,
            license,
            compatibility,
            metadata,
            allowed_tools,
            user_invocable,
            disable_model_invocation,
        })
    }
}

/// The hyphenated validation alias wins over the underscore field name when
/// both spellings are present, matching the reference validator's priority.
fn aliased<'a>(frontmatter: &'a Map<String, Value>, alias: &str, name: &str) -> Option<&'a Value> {
    frontmatter.get(alias).or_else(|| frontmatter.get(name))
}

/// A mandatory string of one to `maximum` characters. A non-string value is
/// rejected rather than coerced, as the reference's string fields reject it.
fn required_text(
    frontmatter: &Map<String, Value>,
    field: &str,
    maximum: usize,
) -> Result<String, SkillValidationError> {
    let Some(value) = frontmatter.get(field) else {
        return Err(SkillValidationError::new(field, "is required"));
    };
    let Value::String(text) = value else {
        return Err(SkillValidationError::new(field, "must be a string"));
    };
    let length = text.chars().count();
    if length == 0 || length > maximum {
        return Err(SkillValidationError::new(field, "has an invalid length"));
    }
    Ok(text.clone())
}

/// An optional string of at most `maximum` characters; an explicit null reads
/// as absent.
fn optional_text(
    frontmatter: &Map<String, Value>,
    field: &str,
    maximum: usize,
) -> Result<Option<String>, SkillValidationError> {
    match frontmatter.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            if text.chars().count() > maximum {
                return Err(SkillValidationError::new(field, "is too long"));
            }
            Ok(Some(text.clone()))
        }
        Some(_) => Err(SkillValidationError::new(field, "must be a string")),
    }
}

fn valid_skill_name(name: &str) -> bool {
    name.split('-').all(|part| {
        !part.is_empty()
            && part
                .chars()
                .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
    })
}

/// The `metadata` mapping with every value normalized to its Python string
/// form, which is what the reference's before-validator produces.
fn normalized_metadata(
    value: Option<&Value>,
) -> Result<BTreeMap<String, String>, SkillValidationError> {
    match value {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(entries)) => Ok(entries
            .iter()
            .map(|(key, value)| (key.clone(), crate::pycompat::str_json(value)))
            .collect()),
        Some(_) => Err(SkillValidationError::new("metadata", "must be a mapping")),
    }
}

/// `allowed-tools` as the reference's before-validator reads it: null is an
/// empty list, a string splits on whitespace, and a list is taken as given
/// with every element required to be a string.
fn parsed_allowed_tools(value: Option<&Value>) -> Result<Vec<String>, SkillValidationError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(text.split_whitespace().map(ToOwned::to_owned).collect()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => Ok(text.clone()),
                _ => Err(SkillValidationError::new(
                    "allowed-tools",
                    "must hold strings",
                )),
            })
            .collect(),
        Some(_) => Err(SkillValidationError::new(
            "allowed-tools",
            "must be a string, a list of strings, or null",
        )),
    }
}
