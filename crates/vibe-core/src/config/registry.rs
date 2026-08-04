//! The declarative configuration field registry.
//!
//! One table declares every configuration field the reference knows about,
//! together with the metadata four consumers used to carry independently: the
//! merge strategy, the published JSON Schema, the editor kind a settings screen
//! renders, and the type an environment override coerces to.
//!
//! Two axes are deliberately separate. [`FieldSpec::strategy`] is declared for
//! *every* reference field, because the merge has to compose a key correctly
//! whether or not this port reads it yet. [`FieldSpec::published`] gates the
//! much smaller set that reaches `config/schema`, because a key only arrives
//! with the feature that consumes it. Filling the remaining defaults and
//! flipping the rest to published belongs to EP-019.
//!
//! Field names, merge strategies, merge keys, editor kinds and the popular set
//! are behavioral observations taken from the pinned reference. Descriptions are
//! original prose: `NOTICE` forbids reproducing reference-authored text.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::{Map, Value as JsonValue};
use toml::Value;

use super::THEME_VALUES;

/// How a field's values from two layers combine. These are the four strategies
/// the reference schema actually declares; `shallow` and `conflict` exist in the
/// reference vocabulary but no field adopts them, so they are unreachable and
/// are not implemented here. [`super::surface_parity_tests`] fails if that ever
/// stops being true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStrategy {
    /// The higher layer wins outright.
    Replace,
    /// Lists append in layer order, duplicates preserved.
    Concat,
    /// Lists are keyed by [`FieldSpec::merge_key`]; the higher layer wins per
    /// key and first-seen order is preserved.
    Union,
    /// Tables merge recursively; keys the higher layer omits survive.
    DeepMerge,
}

impl MergeStrategy {
    /// The wire name the capture script records for this strategy.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Concat => "concat",
            Self::Union => "union",
            Self::DeepMerge => "deep_merge",
        }
    }
}

/// The editor control a settings screen renders for a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Bool,
    Enum,
    Int,
    Float,
    Str,
    /// A list of scalars.
    List,
    /// Anything a settings screen cannot render as a scalar or a scalar list.
    Complex,
}

impl FieldKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Enum => "enum",
            Self::Int => "int",
            Self::Float => "float",
            Self::Str => "str",
            Self::List => "list",
            Self::Complex => "complex",
        }
    }
}

/// A field's default, in the shapes the published surface currently needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldDefault {
    /// The key is absent until a layer sets it.
    None,
    Bool(bool),
    Str(&'static str),
    Strings(&'static [&'static str]),
}

impl FieldDefault {
    /// The TOML value this default composes into a layer document, or `None`
    /// when the field has no default to compose.
    #[must_use]
    pub fn to_toml(self) -> Option<Value> {
        match self {
            Self::None => None,
            Self::Bool(value) => Some(Value::Boolean(value)),
            Self::Str(value) => Some(Value::String(value.to_owned())),
            Self::Strings(values) => Some(Value::Array(
                values
                    .iter()
                    .map(|value| Value::String((*value).to_owned()))
                    .collect(),
            )),
        }
    }

    fn to_json(self) -> Option<JsonValue> {
        match self {
            Self::None => None,
            Self::Bool(value) => Some(JsonValue::from(value)),
            Self::Str(value) => Some(JsonValue::from(value)),
            Self::Strings(values) => Some(JsonValue::from(values.to_vec())),
        }
    }
}

/// One configuration field, declared once for every consumer that needs it.
#[derive(Debug, Clone, Copy)]
pub struct FieldSpec {
    /// The top-level key as it appears in a layer document.
    pub name: &'static str,
    pub kind: FieldKind,
    /// The accepted values when `kind` is [`FieldKind::Enum`].
    pub choices: &'static [&'static str],
    pub default: FieldDefault,
    /// Original prose, empty while the field is declared but not published.
    pub description: &'static str,
    pub strategy: MergeStrategy,
    /// The entry key a [`MergeStrategy::Union`] field is merged by.
    pub merge_key: Option<&'static str>,
    /// Whether the reference settings screen surfaces this field first.
    pub popular: bool,
    /// Whether `config/schema` publishes this field. A declared-but-unpublished
    /// field still merges by its strategy.
    pub published: bool,
    /// A JSON object literal merged over the generated property schema, for the
    /// constraints a kind alone cannot express.
    pub schema_extra: &'static str,
}

impl FieldSpec {
    const fn declared(name: &'static str, kind: FieldKind, strategy: MergeStrategy) -> Self {
        Self {
            name,
            kind,
            choices: &[],
            default: FieldDefault::None,
            description: "",
            strategy,
            merge_key: None,
            popular: false,
            published: false,
            schema_extra: "",
        }
    }

    const fn union(name: &'static str, kind: FieldKind, merge_key: &'static str) -> Self {
        Self {
            merge_key: Some(merge_key),
            ..Self::declared(name, kind, MergeStrategy::Union)
        }
    }

    const fn popular(self) -> Self {
        Self {
            popular: true,
            ..self
        }
    }

    const fn published(
        self,
        default: FieldDefault,
        description: &'static str,
        schema_extra: &'static str,
    ) -> Self {
        Self {
            default,
            description,
            schema_extra,
            published: true,
            ..self
        }
    }

    const fn choices(self, choices: &'static [&'static str]) -> Self {
        Self { choices, ..self }
    }
}

const REPLACE: MergeStrategy = MergeStrategy::Replace;
const CONCAT: MergeStrategy = MergeStrategy::Concat;
const DEEP_MERGE: MergeStrategy = MergeStrategy::DeepMerge;

const THINKING_VALUES: &[&str] = &["off", "low", "medium", "high", "max"];
const NOTIFICATION_VALUES: &[&str] = &["off", "unfocused", "always"];
const OTEL_REDACTION_VALUES: &[&str] = &["default", "none", "strict"];

/// The JSON Schema for one `mcp_servers` entry, which no field kind describes.
const MCP_SERVER_ITEMS: &str = r#"{
    "type": "array",
    "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["name", "transport"],
        "properties": {
            "name": {"type": "string", "minLength": 1},
            "transport": {"enum": ["stdio", "streamable-http"]},
            "command": {"type": "string", "minLength": 1},
            "url": {"type": "string", "format": "uri"},
            "headers": {"type": "object", "additionalProperties": {"type": "string"}},
            "args": {"type": "array", "items": {"type": "string"}},
            "env": {"type": "object", "additionalProperties": {"type": "string"}},
            "cwd": {"type": "string"},
            "disabled": {"type": "boolean"},
            "disabled_tools": {"type": "array", "items": {"type": "string"}},
            "startup_timeout_sec": {"type": "number", "exclusiveMinimum": 0},
            "tool_timeout_sec": {"type": "number", "exclusiveMinimum": 0}
        },
        "allOf": [
            {
                "if": {"properties": {"transport": {"const": "stdio"}}},
                "then": {"required": ["command"]}
            },
            {
                "if": {"properties": {"transport": {"const": "streamable-http"}}},
                "then": {"required": ["url"]}
            }
        ]
    }
}"#;

/// The JSON Schema for one `connectors` entry.
const CONNECTOR_ITEMS: &str = r#"{
    "type": "array",
    "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["name"],
        "properties": {
            "name": {"type": "string", "minLength": 1},
            "disabled": {"type": "boolean"},
            "disabled_tools": {"type": "array", "items": {"type": "string"}}
        }
    }
}"#;

/// Union fields whose matching entries this port merges field by field instead
/// of replacing whole.
///
/// The reference replaces the entire entry, so a higher layer must repeat the
/// whole definition to change one preference. This port persists an enablement
/// preference as a partial entry precisely so a lower layer's secrets are never
/// copied into the writable file, which whole-entry replacement would undo.
/// Recorded as a divergence; reconciling it belongs to EP-022, which owns the
/// MCP configuration surface.
pub const ENTRY_MERGED_UNION_FIELDS: [&str; 2] = ["mcp_servers", "connectors"];

/// Every configuration field the reference declares, in declaration order, plus
/// the fields only this port publishes.
pub static FIELDS: &[FieldSpec] = &[
    // Models
    FieldSpec::declared("active_model", FieldKind::Str, REPLACE)
        .popular()
        .published(
            FieldDefault::Str(""),
            "Model used for new turns.",
            "",
        ),
    FieldSpec::union("providers", FieldKind::Complex, "name"),
    FieldSpec::declared("models", FieldKind::Complex, DEEP_MERGE),
    FieldSpec::declared("compaction_model", FieldKind::Complex, REPLACE),
    FieldSpec::declared("auto_compact_threshold", FieldKind::Int, REPLACE).popular(),
    FieldSpec::declared("active_transcribe_model", FieldKind::Str, REPLACE),
    FieldSpec::union("transcribe_providers", FieldKind::Complex, "name"),
    FieldSpec::union("transcribe_models", FieldKind::Complex, "alias"),
    FieldSpec::declared("active_tts_model", FieldKind::Str, REPLACE),
    FieldSpec::union("tts_providers", FieldKind::Complex, "name"),
    FieldSpec::union("tts_models", FieldKind::Complex, "alias"),
    // Tools
    FieldSpec::declared("tools", FieldKind::Complex, DEEP_MERGE),
    FieldSpec::declared("tool_paths", FieldKind::List, CONCAT),
    FieldSpec::declared("enabled_tools", FieldKind::List, REPLACE).published(
        FieldDefault::Strings(&[]),
        "Tool names or patterns to publish. When set, only matching tools are published. Globs and `re:` regular expressions are supported.",
        "",
    ),
    FieldSpec::declared("disabled_tools", FieldKind::List, CONCAT).published(
        FieldDefault::Strings(&[]),
        "Tool names or patterns to withhold, applied after `enabled_tools`. Globs and `re:` regular expressions are supported.",
        "",
    ),
    FieldSpec::union("mcp_servers", FieldKind::Complex, "name")
        .popular()
        .published(
            FieldDefault::None,
            "MCP server definitions.",
            MCP_SERVER_ITEMS,
        ),
    FieldSpec::declared("enable_connectors", FieldKind::Bool, REPLACE),
    FieldSpec::union("connectors", FieldKind::Complex, "name").published(
        FieldDefault::None,
        "Persistent connector enablement preferences.",
        CONNECTOR_ITEMS,
    ),
    // Agents
    FieldSpec::declared("agent_paths", FieldKind::List, CONCAT),
    FieldSpec::declared("enabled_agents", FieldKind::List, CONCAT),
    FieldSpec::declared("disabled_agents", FieldKind::List, CONCAT),
    FieldSpec::declared("installed_agents", FieldKind::List, CONCAT),
    FieldSpec::declared("default_agent", FieldKind::Str, REPLACE).popular(),
    // Skills
    FieldSpec::declared("skill_paths", FieldKind::List, CONCAT),
    FieldSpec::declared("enabled_skills", FieldKind::List, CONCAT),
    FieldSpec::declared("disabled_skills", FieldKind::List, CONCAT),
    FieldSpec::declared(
        "experimental_enable_registry_skills",
        FieldKind::Bool,
        REPLACE,
    ),
    // Internal
    FieldSpec::declared("vibe_code_enabled", FieldKind::Bool, REPLACE),
    FieldSpec::declared("vibe_code_api_key_env_var", FieldKind::Str, REPLACE),
    FieldSpec::declared("enable_otel", FieldKind::Bool, REPLACE),
    FieldSpec::declared("otel_endpoint", FieldKind::Str, REPLACE),
    FieldSpec::declared("otel_redaction", FieldKind::Enum, REPLACE).choices(OTEL_REDACTION_VALUES),
    FieldSpec::declared("console_base_url", FieldKind::Str, REPLACE),
    // Top-level scalars
    FieldSpec::declared("theme", FieldKind::Enum, REPLACE)
        .choices(&THEME_VALUES)
        .popular()
        .published(
            FieldDefault::Str("system"),
            "Terminal color theme.",
            "",
        ),
    FieldSpec::declared("applied_migrations", FieldKind::List, CONCAT),
    FieldSpec::declared("disable_welcome_banner_animation", FieldKind::Bool, REPLACE),
    FieldSpec::declared("autocopy_to_clipboard", FieldKind::Bool, REPLACE).popular(),
    FieldSpec::declared("file_watcher_for_autocomplete", FieldKind::Bool, REPLACE),
    FieldSpec::declared("ask_confirmation_on_exit", FieldKind::Bool, REPLACE).popular(),
    FieldSpec::declared("displayed_workdir", FieldKind::Str, REPLACE),
    FieldSpec::declared("context_warnings", FieldKind::Bool, REPLACE),
    FieldSpec::declared("voice_mode_enabled", FieldKind::Bool, REPLACE)
        .popular()
        .published(
            FieldDefault::Bool(false),
            "Enable voice input.",
            "",
        ),
    FieldSpec::declared("narrator_enabled", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(false),
        "Read eligible assistant responses aloud.",
        "",
    ),
    FieldSpec::declared("show_thinking_nodes", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(true),
        "Show reasoning regions in the transcript.",
        "",
    ),
    FieldSpec::declared("bypass_tool_permissions", FieldKind::Bool, REPLACE).popular(),
    FieldSpec::declared("raise_on_compaction_failure", FieldKind::Bool, REPLACE),
    FieldSpec::declared("enable_telemetry", FieldKind::Bool, REPLACE).popular(),
    FieldSpec::declared("system_prompt_id", FieldKind::Str, REPLACE),
    FieldSpec::declared("compaction_prompt_id", FieldKind::Str, REPLACE),
    FieldSpec::declared("include_commit_signature", FieldKind::Bool, REPLACE),
    FieldSpec::declared("include_model_info", FieldKind::Bool, REPLACE),
    FieldSpec::declared("include_project_context", FieldKind::Bool, REPLACE),
    FieldSpec::declared("include_prompt_detail", FieldKind::Bool, REPLACE),
    FieldSpec::declared("enable_update_checks", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(true),
        "Check for new releases in the background.",
        "",
    ),
    FieldSpec::declared("enable_auto_update", FieldKind::Bool, REPLACE).popular(),
    FieldSpec::declared("enable_notifications", FieldKind::Bool, REPLACE).popular(),
    FieldSpec::declared("enable_system_trust_store", FieldKind::Bool, REPLACE),
    FieldSpec::declared("api_timeout", FieldKind::Float, REPLACE),
    FieldSpec::declared("api_retry_max_elapsed_time", FieldKind::Float, REPLACE),
    FieldSpec::declared("vibe_base_url", FieldKind::Str, REPLACE),
    FieldSpec::declared("vibe_code_sessions_base_url", FieldKind::Str, REPLACE),
    // Nested configs
    FieldSpec::declared("project_context", FieldKind::Complex, REPLACE),
    FieldSpec::declared("session_logging", FieldKind::Complex, REPLACE),
    FieldSpec::declared("experiments", FieldKind::Complex, REPLACE),
    // Fields only this port publishes. US-064 decides whether each maps onto a
    // reference field or stays a recorded divergence.
    FieldSpec::declared("thinking", FieldKind::Enum, REPLACE)
        .choices(THINKING_VALUES)
        .published(
            FieldDefault::Str("off"),
            "Reasoning effort for the active model.",
            "",
        ),
    FieldSpec::declared("notifications", FieldKind::Enum, REPLACE)
        .choices(NOTIFICATION_VALUES)
        .published(
            FieldDefault::Str("unfocused"),
            "When desktop notifications may be sent.",
            "",
        ),
    FieldSpec::declared("proxy", FieldKind::Str, REPLACE).published(
        FieldDefault::None,
        "Legacy proxy URL. Prefer /proxy-setup for protocol-specific values.",
        r#"{"type": ["string", "null"], "format": "uri"}"#,
    ),
    FieldSpec::declared("tls_ca_path", FieldKind::Str, REPLACE).published(
        FieldDefault::None,
        "Legacy TLS certificate path. Prefer /proxy-setup.",
        r#"{"type": ["string", "null"]}"#,
    ),
    FieldSpec::declared("dotenv_path", FieldKind::Str, REPLACE).published(
        FieldDefault::None,
        "Optional dotenv file loaded by the runtime.",
        r#"{"type": ["string", "null"]}"#,
    ),
];

/// The registry indexed by field name, built once per process.
///
/// Duplicate names collapse here, which would hide a declaration mistake; the
/// index is therefore only reachable through [`field`], and
/// [`registry_tests`](super::registry_tests) asserts the index is as long as
/// [`FIELDS`].
fn index() -> &'static BTreeMap<&'static str, &'static FieldSpec> {
    static INDEX: OnceLock<BTreeMap<&'static str, &'static FieldSpec>> = OnceLock::new();
    INDEX.get_or_init(|| FIELDS.iter().map(|spec| (spec.name, spec)).collect())
}

/// The declaration for `name`, or `None` when the field is unregistered.
#[must_use]
pub fn field(name: &str) -> Option<&'static FieldSpec> {
    index().get(name).copied()
}

/// The strategy `name` merges by. An unregistered key merges by
/// [`MergeStrategy::Replace`], which is what a scalar-shaped unknown key needs
/// and what the previous implementation did for every key.
#[must_use]
pub fn strategy(name: &str) -> MergeStrategy {
    field(name).map_or(MergeStrategy::Replace, |spec| spec.strategy)
}

/// The JSON Schema published by `config/schema`, generated from the registry.
///
/// # Panics
///
/// Never in practice: every `schema_extra` literal is parsed by
/// [`registry_tests`](super::registry_tests), so a malformed one fails the
/// suite rather than reaching a caller.
#[must_use]
pub fn json_schema() -> JsonValue {
    let mut properties = Map::new();
    for spec in FIELDS.iter().filter(|spec| spec.published) {
        properties.insert(spec.name.to_owned(), property_schema(spec));
    }
    JsonValue::Object(Map::from_iter([
        ("type".to_owned(), JsonValue::from("object")),
        ("additionalProperties".to_owned(), JsonValue::from(true)),
        ("properties".to_owned(), JsonValue::Object(properties)),
    ]))
}

fn property_schema(spec: &FieldSpec) -> JsonValue {
    let mut property = Map::new();
    match spec.kind {
        FieldKind::Bool => {
            property.insert("type".to_owned(), JsonValue::from("boolean"));
        }
        FieldKind::Enum => {
            property.insert("enum".to_owned(), JsonValue::from(spec.choices.to_vec()));
        }
        FieldKind::Int => {
            property.insert("type".to_owned(), JsonValue::from("integer"));
        }
        FieldKind::Float => {
            property.insert("type".to_owned(), JsonValue::from("number"));
        }
        FieldKind::Str => {
            property.insert("type".to_owned(), JsonValue::from("string"));
        }
        FieldKind::List => {
            property.insert("type".to_owned(), JsonValue::from("array"));
            property.insert(
                "items".to_owned(),
                JsonValue::Object(Map::from_iter([(
                    "type".to_owned(),
                    JsonValue::from("string"),
                )])),
            );
        }
        // A complex field's shape is not derivable from its kind, so it is
        // carried by `schema_extra` instead of generated.
        FieldKind::Complex => {}
    }
    if let Some(default) = spec.default.to_json() {
        property.insert("default".to_owned(), default);
    }
    if !spec.description.is_empty() {
        property.insert("description".to_owned(), JsonValue::from(spec.description));
    }
    for (key, value) in parse_schema_extra(spec) {
        property.insert(key, value);
    }
    JsonValue::Object(property)
}

fn parse_schema_extra(spec: &FieldSpec) -> Map<String, JsonValue> {
    if spec.schema_extra.is_empty() {
        return Map::new();
    }
    match serde_json::from_str::<JsonValue>(spec.schema_extra) {
        Ok(JsonValue::Object(extra)) => extra,
        // Unreachable in a passing suite: `registry_tests` parses every literal.
        Ok(_) | Err(_) => Map::new(),
    }
}
