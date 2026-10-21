//! The reduced pydantic-core schemas, as `scripts/parity/app_server_params.py`
//! writes them into `schema.json`.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::Value;

/// The reduced pydantic-core schemas, compiled in.
const SCHEMA_SOURCE: &str = include_str!("schema.json");

pub(super) static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    #[expect(
        clippy::expect_used,
        reason = "the schema is compiled in and `the_compiled_schema_parses` pins that it parses"
    )]
    serde_json::from_str(SCHEMA_SOURCE).expect("the compiled-in wire schema parses")
});

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Schema {
    pub(super) methods: BTreeMap<String, Option<String>>,
    pub(super) lifecycle: BTreeMap<String, String>,
    /// The model a method's handler validates past its parameters.
    pub(super) nested: BTreeMap<String, String>,
    /// The validator names the schema records.
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the schema tests"))]
    pub(super) functions: Vec<String>,
    pub(super) defs: BTreeMap<String, Model>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Model {
    pub(super) name: String,
    pub(super) fields: Vec<Field>,
    pub(super) extra: Extra,
    #[serde(default = "allow")]
    pub(super) allow_inf_nan: bool,
    #[serde(default)]
    pub(super) validators: Vec<Validator>,
}

fn allow() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub(super) struct Field {
    pub(super) alias: String,
    /// The keys an `AliasChoices` field reads, in the order it tries them;
    /// empty when the field reads `alias` alone.
    #[serde(default)]
    aliases: Vec<String>,
    pub(super) schema: Node,
}

impl Field {
    /// The keys this field reads its value from, first match wins.
    pub(super) fn keys(&self) -> &[String] {
        if self.aliases.is_empty() {
            std::slice::from_ref(&self.alias)
        } else {
            &self.aliases
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Extra {
    Forbid,
    Ignore,
    Allow,
}

#[derive(Debug, Deserialize)]
pub(super) struct Validator {
    pub(super) function: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(super) enum Node {
    Ref {
        name: String,
    },
    Default {
        schema: Box<Node>,
    },
    Nullable {
        schema: Box<Node>,
    },
    FunctionAfter {
        function: String,
        schema: Box<Node>,
    },
    FunctionBefore {
        function: String,
        schema: Box<Node>,
    },
    List {
        items: Box<Node>,
        min_length: Option<usize>,
        max_length: Option<usize>,
    },
    Dict {
        keys: Box<Node>,
        values: Box<Node>,
    },
    TaggedUnion {
        discriminator: String,
        choices: Vec<(String, Node)>,
    },
    Literal {
        expected: Vec<Value>,
    },
    Enum {
        values: Vec<Value>,
    },
    Str {
        #[serde(default)]
        strict: bool,
        min_length: Option<usize>,
        max_length: Option<usize>,
    },
    Int {
        #[serde(default)]
        strict: bool,
        ge: Option<f64>,
        gt: Option<f64>,
        le: Option<f64>,
        lt: Option<f64>,
    },
    Float {
        #[serde(default)]
        strict: bool,
        ge: Option<f64>,
        gt: Option<f64>,
        le: Option<f64>,
        lt: Option<f64>,
    },
    Bool {
        #[serde(default)]
        strict: bool,
    },
    None,
    Any,
    JsonValue,
}
