//! What the four file tools publish: each one's schema, and the argument type
//! a call is read into once that schema has accepted it.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Number, Value, json};

use crate::schema::{ObjectSchema, Property};
use crate::tools::config::declared_document;
use crate::tools::{
    ToolAvailability, ToolError, ToolInvocation, ToolPresentationKind, ToolSource, ToolSpec,
};

/// The arguments of one call, read into the shape its schema declares.
///
/// The registry coerces and validates a call against that schema and applies
/// its defaults before any handler runs, so a payload that does not read here
/// means a schema and its argument type disagree.
pub(super) fn parsed<T: DeserializeOwned>(invocation: &ToolInvocation) -> Result<T, ToolError> {
    T::deserialize(&invocation.arguments).map_err(|error| ToolError::Execution(error.to_string()))
}

/// Reference `ReadFileArgs`.
#[derive(Deserialize)]
pub(super) struct ReadFileArgs {
    pub(super) file_path: String,
    /// Null starts the read at line one.
    pub(super) offset: Option<usize>,
    pub(super) limit: usize,
}

/// Reference `GrepArgs`.
#[derive(Deserialize)]
pub(super) struct GrepArgs {
    pub(super) pattern: String,
    pub(super) path: String,
    /// Any integer the schema accepts; only a non-negative one is a cap, and
    /// anything else falls back to the configured default.
    pub(super) max_matches: Option<Number>,
    pub(super) use_default_ignore: bool,
}

/// Reference `EditArgs`.
#[derive(Deserialize)]
pub(super) struct EditArgs {
    pub(super) file_path: String,
    pub(super) old_string: String,
    pub(super) new_string: String,
    pub(super) replace_all: bool,
}

/// Reference `WriteFileArgs`.
#[derive(Deserialize)]
pub(super) struct WriteFileArgs {
    pub(super) file_path: String,
    pub(super) content: String,
}

/// Reference `ReadFileArgs.limit` default.
const DEFAULT_READ_LINE_LIMIT: usize = 2000;

pub(super) fn read_file_spec() -> ToolSpec {
    ToolSpec {
        name: "read_file".to_owned(),
        description: "Read one file from an absolute path. Page through a long file with \
                      `offset` and `limit` instead of reading it whole, reach for `grep` when \
                      looking for specific content, and leave binary and model-weight files \
                      (.bin, .safetensors, .pt, .gguf) alone."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "file_path",
                Property::string().described("The absolute path of the file to read"),
            )
            .optional(
                "offset",
                Property::integer()
                    .constrained("minimum", 1)
                    .described("The 1-indexed line the read starts at")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "limit",
                Property::integer()
                    .constrained("exclusiveMinimum", 0)
                    .described("How many lines to read at most")
                    .with_default(json!(DEFAULT_READ_LINE_LIMIT)),
            )
            .build(),
        output_schema: None,
        config: declared_document("read_file"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Read,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

pub(super) fn grep_spec() -> ToolSpec {
    ToolSpec {
        name: "grep".to_owned(),
        description: "Search file contents against a regular expression, reporting the path, \
                      the line number and the matching line. Narrow the walk with `path`."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "pattern",
                Property::string().described("The regular expression matched against file contents"),
            )
            .optional(
                "path",
                Property::string()
                    .described(
                        "The file or directory the search walks. Defaults to the working directory.",
                    )
                    .with_default("."),
            )
            .optional(
                "max_matches",
                Property::integer()
                    .described("Raises or lowers the default cap on returned matches.")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "use_default_ignore",
                Property::boolean()
                    .described("Whether .gitignore and .ignore entries are honored.")
                    .with_default(true),
            )
            .build(),
        output_schema: None,
        config: declared_document("grep"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Search,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

pub(super) fn edit_spec() -> ToolSpec {
    ToolSpec {
        name: "edit".to_owned(),
        description: "Replace an exact string in a file. Call `read_file` first, and never carry \
                      any part of its line-number prefix into `old_string` or `new_string`. When \
                      `old_string` is absent or matches more than once, add surrounding context \
                      until it is unique or set `replace_all`. Re-read the file before retrying a \
                      failed edit."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "file_path",
                Property::string().described("The absolute path of the file to modify"),
            )
            .required(
                "old_string",
                Property::string().described("The text being replaced"),
            )
            .required(
                "new_string",
                Property::string()
                    .described("The replacement text, which must differ from old_string"),
            )
            .optional(
                "replace_all",
                Property::boolean()
                    .described("Replace every occurrence of old_string instead of a single one")
                    .with_default(false),
            )
            .build(),
        output_schema: None,
        config: declared_document("edit"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Diff,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

/// Directive coverage for `write_file`, whose reference description this port
/// must cover without reproducing (`NOTICE`).
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The path is absolute | "absolute path" in the `file_path` description |
/// | An existing file is not overwritten; `edit` modifies it | "never overwrites an existing file: reach for `edit`" |
/// | Missing parent directories are created | "Missing parent directories are created" |
/// | The content replaces the whole file | "the whole file content" in the `content` description |
pub(super) fn write_file_spec() -> ToolSpec {
    ToolSpec {
        name: "write_file".to_owned(),
        description: "Create a file at an absolute path and write it whole. It never overwrites \
                      an existing file: reach for `edit` to change one. Missing parent \
                      directories are created."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "file_path",
                Property::string()
                    .described("The absolute path of the file to write, which must not exist yet"),
            )
            .required(
                "content",
                Property::string().described("The whole file content to write"),
            )
            .build(),
        output_schema: None,
        config: declared_document("write_file"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Diff,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}
