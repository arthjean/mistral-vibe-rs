//! The declarative configuration field registry.
//!
//! One table declares every configuration field the reference knows about,
//! together with the metadata four consumers used to carry independently: the
//! merge strategy, the published JSON Schema, the editor kind a settings screen
//! renders, and the type an environment override coerces to.
//!
//! Every reference field is declared, published through `config/schema` and
//! carries the reference default, so the four consumers read one table.
//! [`FieldSpec::local`] marks the handful of keys only this port publishes;
//! they reach the schema but never the shipped default document, which has to
//! stay comparable to the reference one field for field.
//!
//! Field names, merge strategies, merge keys, editor kinds, the popular set and
//! default values are behavioral observations taken from the pinned reference.
//! Descriptions are original prose: `NOTICE` forbids reproducing
//! reference-authored text.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::{Map, Value as JsonValue};
use sha2::{Digest, Sha256};
use toml::Value;

use super::THEME_VALUES;
use crate::text::hex_encode;

/// Reference `UNPINNED_ACTIVE_MODEL`: the `active_model` value meaning "the
/// operator pinned nothing". It is resolved when the alias is read rather than
/// when the document is composed, so a routed default can still select the
/// model an installation runs on.
pub const UNPINNED_ACTIVE_MODEL: &str = "";

/// Reference `DEFAULT_ACTIVE_MODEL_CONFIG.alias`, which is what an unpinned
/// installation resolves to when no experiment routed it elsewhere.
pub const DEFAULT_ACTIVE_MODEL_ALIAS: &str = "mistral-medium-3.5";

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
    /// The whole vocabulary, in the order the wire enumeration declares it.
    pub const ALL: [Self; 7] = [
        Self::Bool,
        Self::Enum,
        Self::Int,
        Self::Float,
        Self::Str,
        Self::List,
        Self::Complex,
    ];

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

/// A field's default, in the shapes the published surface needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldDefault {
    /// The key is absent until a layer sets it.
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'static str),
    Strings(&'static [&'static str]),
    /// A JSON literal, for a default no scalar variant expresses. Every literal
    /// is parsed by [`registry_tests`](super::registry_tests), so a malformed
    /// one fails the suite instead of silently dropping a default.
    Json(&'static str),
}

impl FieldDefault {
    /// The TOML value this default composes into a layer document, or `None`
    /// when the field has no default to compose.
    #[must_use]
    pub fn to_toml(self) -> Option<Value> {
        match self {
            Self::None => None,
            Self::Bool(value) => Some(Value::Boolean(value)),
            Self::Int(value) => Some(Value::Integer(value)),
            Self::Float(value) => Some(Value::Float(value)),
            Self::Str(value) => Some(Value::String(value.to_owned())),
            Self::Strings(values) => Some(Value::Array(
                values
                    .iter()
                    .map(|value| Value::String((*value).to_owned()))
                    .collect(),
            )),
            Self::Json(literal) => self
                .to_json()
                .and_then(|value| Value::try_from(value).ok())
                .or_else(|| {
                    debug_assert!(false, "malformed default literal: {literal}");
                    None
                }),
        }
    }

    pub(super) fn to_json(self) -> Option<JsonValue> {
        match self {
            Self::None => None,
            Self::Bool(value) => Some(JsonValue::from(value)),
            Self::Int(value) => Some(JsonValue::from(value)),
            Self::Float(value) => Some(JsonValue::from(value)),
            Self::Str(value) => Some(JsonValue::from(value)),
            Self::Strings(values) => Some(JsonValue::from(values.to_vec())),
            Self::Json(literal) => serde_json::from_str(literal).ok(),
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
    /// Whether `config/schema` publishes this field.
    pub published: bool,
    /// Whether only this port declares the field. A local field is published
    /// like any other but stays out of the shipped default document, which is
    /// compared against the reference one field for field.
    pub local: bool,
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
            local: false,
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

    const fn local(self) -> Self {
        Self {
            local: true,
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

pub(super) const THINKING_VALUES: &[&str] = &["off", "low", "medium", "high", "max"];
const NOTIFICATION_VALUES: &[&str] = &["off", "unfocused", "always"];
const OTEL_REDACTION_VALUES: &[&str] = &["default", "none", "strict"];

/// One provider entry, as `providers` and the two voice provider lists carry it.
const PROVIDER_ITEMS: &str = r#"{
    "type": "array",
    "items": {
        "type": "object",
        "required": ["name", "api_base"],
        "properties": {
            "name": {"type": "string", "minLength": 1},
            "api_base": {"type": "string"},
            "api_key_env_var": {"type": "string"},
            "browser_auth_base_url": {"type": ["string", "null"]},
            "browser_auth_api_base_url": {"type": ["string", "null"]},
            "api_style": {"type": "string"},
            "backend": {"type": "string"},
            "reasoning_field_name": {"type": "string"},
            "project_id": {"type": "string"},
            "region": {"type": "string"},
            "extra_headers": {"type": "object", "additionalProperties": {"type": "string"}}
        }
    }
}"#;

/// `models` is written as an array and read back as a map keyed by alias, so
/// both forms are accepted here.
const MODEL_ITEMS: &str = r#"{
    "type": ["array", "object"],
    "items": {
        "type": "object",
        "required": ["name", "provider"],
        "properties": {
            "name": {"type": "string", "minLength": 1},
            "provider": {"type": "string", "minLength": 1},
            "alias": {"type": "string", "minLength": 1},
            "temperature": {"type": "number"},
            "input_price": {"type": "number"},
            "output_price": {"type": "number"},
            "cached_input_price": {"type": ["number", "null"]},
            "thinking": {"enum": ["off", "low", "medium", "high", "max"]},
            "supports_images": {"type": "boolean"},
            "auto_compact_threshold": {"type": "integer"}
        }
    },
    "additionalProperties": {"type": "object"}
}"#;

const COMPACTION_MODEL: &str = r#"{
    "type": ["object", "null"],
    "required": ["name", "provider"],
    "properties": {
        "name": {"type": "string", "minLength": 1},
        "provider": {"type": "string", "minLength": 1},
        "alias": {"type": "string", "minLength": 1},
        "temperature": {"type": "number"},
        "input_price": {"type": "number"},
        "output_price": {"type": "number"},
        "cached_input_price": {"type": ["number", "null"]},
        "thinking": {"enum": ["off", "low", "medium", "high", "max"]},
        "supports_images": {"type": "boolean"},
        "auto_compact_threshold": {"type": "integer"}
    }
}"#;

/// `routed_model_config` is written by the experiments layer as the JSON text
/// of a model definition and read back as the definition itself, so both forms
/// validate. Reference `_coerce_routed_model_config` is the coercion, and
/// [`crate::config`] runs it before the merged document is finalized.
const ROUTED_MODEL_CONFIG: &str = r#"{
    "type": ["object", "string", "null"],
    "required": ["name", "provider"],
    "properties": {
        "name": {"type": "string", "minLength": 1},
        "provider": {"type": "string", "minLength": 1},
        "alias": {"type": "string", "minLength": 1},
        "temperature": {"type": "number"},
        "input_price": {"type": "number"},
        "output_price": {"type": "number"},
        "cached_input_price": {"type": ["number", "null"]},
        "thinking": {"enum": ["off", "low", "medium", "high", "max"]},
        "supports_images": {"type": "boolean"},
        "auto_compact_threshold": {"type": "integer"}
    }
}"#;

const TRANSCRIBE_PROVIDER_ITEMS: &str = r#"{
    "type": "array",
    "items": {
        "type": "object",
        "required": ["name"],
        "properties": {
            "name": {"type": "string", "minLength": 1},
            "api_base": {"type": "string"},
            "api_key_env_var": {"type": "string"},
            "client": {"type": "string"}
        }
    }
}"#;

const TRANSCRIBE_MODEL_ITEMS: &str = r#"{
    "type": "array",
    "items": {
        "type": "object",
        "required": ["name", "provider", "alias"],
        "properties": {
            "name": {"type": "string", "minLength": 1},
            "provider": {"type": "string", "minLength": 1},
            "alias": {"type": "string", "minLength": 1},
            "sample_rate": {"type": "integer", "exclusiveMinimum": 0},
            "encoding": {"type": "string"},
            "language": {"type": "string"},
            "target_streaming_delay_ms": {"type": "integer", "minimum": 0}
        }
    }
}"#;

const TTS_PROVIDER_ITEMS: &str = r#"{
    "type": "array",
    "items": {
        "type": "object",
        "required": ["name"],
        "properties": {
            "name": {"type": "string", "minLength": 1},
            "api_base": {"type": "string"},
            "api_key_env_var": {"type": "string"},
            "client": {"type": "string"}
        }
    }
}"#;

/// `response_format` carries the closed vocabulary the reference declares as
/// its `SpeechOutputFormat` literal, so a settings surface offers exactly the
/// containers the speech endpoint answers with rather than a free string.
const TTS_MODEL_ITEMS: &str = r#"{
    "type": "array",
    "items": {
        "type": "object",
        "required": ["name", "provider", "alias"],
        "properties": {
            "name": {"type": "string", "minLength": 1},
            "provider": {"type": "string", "minLength": 1},
            "alias": {"type": "string", "minLength": 1},
            "voice": {"type": "string"},
            "response_format": {
                "type": "string",
                "enum": ["pcm", "wav", "mp3", "flac", "opus"]
            }
        }
    }
}"#;

/// `tools` is keyed by tool name and every tool declares its own options, so
/// only the shape is published.
const TOOL_SETTINGS: &str = r#"{
    "type": "object",
    "additionalProperties": {"type": "object"}
}"#;

const PROJECT_CONTEXT: &str = r#"{
    "type": "object",
    "properties": {
        "default_commit_count": {"type": "integer", "minimum": 0},
        "timeout_seconds": {"type": "number", "exclusiveMinimum": 0}
    }
}"#;

const SESSION_LOGGING: &str = r#"{
    "type": "object",
    "properties": {
        "save_dir": {"type": "string"},
        "session_prefix": {"type": "string"},
        "enabled": {"type": "boolean"}
    }
}"#;

const EXPERIMENTS: &str = r#"{
    "type": "object",
    "properties": {
        "enable": {"type": "boolean"},
        "api_host": {"type": "string"},
        "client_key": {"type": "string"}
    }
}"#;

/// The two providers the reference ships.
const DEFAULT_PROVIDERS: &str = r#"[
    {
        "name": "mistral",
        "api_base": "https://api.mistral.ai/v1",
        "api_key_env_var": "MISTRAL_API_KEY",
        "browser_auth_base_url": "https://console.mistral.ai",
        "browser_auth_api_base_url": "https://console.mistral.ai/api",
        "api_style": "openai",
        "backend": "mistral",
        "reasoning_field_name": "reasoning_content",
        "project_id": "",
        "region": "",
        "extra_headers": {}
    },
    {
        "name": "llamacpp",
        "api_base": "http://127.0.0.1:8080/v1",
        "api_key_env_var": "",
        "api_style": "openai",
        "backend": "generic",
        "reasoning_field_name": "reasoning_content",
        "project_id": "",
        "region": "",
        "extra_headers": {}
    }
]"#;

/// The three models the reference ships, in the persisted array form. A field
/// the reference leaves unset is absent rather than null: TOML has no null.
const DEFAULT_MODELS: &str = r#"[
    {
        "name": "mistral-vibe-cli-latest",
        "provider": "mistral",
        "alias": "mistral-medium-3.5",
        "temperature": 1.0,
        "input_price": 1.5,
        "output_price": 7.5,
        "cached_input_price": 0.15,
        "thinking": "high",
        "supports_images": true,
        "auto_compact_threshold": 200000
    },
    {
        "name": "devstral-small-latest",
        "provider": "mistral",
        "alias": "devstral-small",
        "temperature": 0.2,
        "input_price": 0.1,
        "output_price": 0.3,
        "cached_input_price": 0.01,
        "thinking": "off",
        "supports_images": false,
        "auto_compact_threshold": 200000
    },
    {
        "name": "devstral",
        "provider": "llamacpp",
        "alias": "local",
        "temperature": 0.2,
        "input_price": 0.0,
        "output_price": 0.0,
        "thinking": "off",
        "supports_images": false,
        "auto_compact_threshold": 200000
    }
]"#;

const DEFAULT_TRANSCRIBE_PROVIDERS: &str = r#"[
    {
        "name": "mistral",
        "api_base": "wss://api.mistral.ai",
        "api_key_env_var": "MISTRAL_API_KEY",
        "client": "mistral"
    }
]"#;

const DEFAULT_TRANSCRIBE_MODELS: &str = r#"[
    {
        "name": "voxtral-mini-transcribe-realtime-2602",
        "provider": "mistral",
        "alias": "voxtral-realtime",
        "sample_rate": 16000,
        "encoding": "pcm_s16le",
        "language": "en",
        "target_streaming_delay_ms": 500
    }
]"#;

const DEFAULT_TTS_PROVIDERS: &str = r#"[
    {
        "name": "mistral",
        "api_base": "https://api.mistral.ai",
        "api_key_env_var": "MISTRAL_API_KEY",
        "client": "mistral"
    }
]"#;

const DEFAULT_TTS_MODELS: &str = r#"[
    {
        "name": "voxtral-mini-tts-latest",
        "provider": "mistral",
        "alias": "voxtral-tts",
        "voice": "gb_jane_neutral",
        "response_format": "wav"
    }
]"#;

const DEFAULT_PROJECT_CONTEXT: &str = r#"{"default_commit_count": 5, "timeout_seconds": 2.0}"#;

/// `save_dir` is empty in the declaration and resolved under the vibe home when
/// the configuration loads, as the reference resolves it from `SESSION_LOG_DIR`.
const DEFAULT_SESSION_LOGGING: &str =
    r#"{"save_dir": "", "session_prefix": "session", "enabled": true}"#;

const DEFAULT_EXPERIMENTS: &str = r#"{
    "enable": true,
    "api_host": "https://experiments.mistral.services/",
    "client_key": "sdk-OE8yJgTXZY6tj"
}"#;

const EMPTY_LIST: &str = "[]";

/// The compaction threshold this build ships, which the reference declares once
/// and reads in two places: as the default of the global `auto_compact_threshold`
/// field, and as the default of the same field on one model.
///
/// Reference `DEFAULT_AUTO_COMPACT_THRESHOLD`.
pub const DEFAULT_AUTO_COMPACT_THRESHOLD: i64 = 200_000;

/// The per-entry defaults the reference `ModelConfig` fills in once a merged
/// model reaches validation. A merged entry only carries what its layers set,
/// so the load completes it from here.
///
/// `cached_input_price` is absent on purpose: it defaults to null upstream and
/// TOML carries no null, so an entry that sets none stays without the key.
/// `auto_compact_threshold` is absent too: an entry of `models` is filled from
/// the global value rather than from a per-model constant. The one definition
/// that is not an entry of `models` falls back to
/// [`DEFAULT_AUTO_COMPACT_THRESHOLD`] instead.
pub const MODEL_DEFAULTS: &str = r#"{
    "temperature": 0.2,
    "input_price": 0.0,
    "output_price": 0.0,
    "thinking": "off",
    "supports_images": false
}"#;

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
            FieldDefault::Str(UNPINNED_ACTIVE_MODEL),
            "Alias of the model new turns run on. Empty means the resolved default.",
            "",
        ),
    FieldSpec::declared("routed_default_model", FieldKind::Str, REPLACE).published(
        FieldDefault::Str(""),
        "Alias an experiment routes an unpinned installation onto.",
        "",
    ),
    FieldSpec::declared("routed_model_config", FieldKind::Complex, REPLACE).published(
        FieldDefault::None,
        "Definition of the routed alias, supplied with it by the same experiment.",
        ROUTED_MODEL_CONFIG,
    ),
    FieldSpec::union("providers", FieldKind::Complex, "name").published(
        FieldDefault::Json(DEFAULT_PROVIDERS),
        "API providers models are served from.",
        PROVIDER_ITEMS,
    ),
    FieldSpec::declared("models", FieldKind::Complex, DEEP_MERGE).published(
        FieldDefault::Json(DEFAULT_MODELS),
        "Model definitions. Written as a list, read back keyed by alias.",
        MODEL_ITEMS,
    ),
    FieldSpec::declared("compaction_model", FieldKind::Complex, REPLACE).published(
        FieldDefault::None,
        "Model conversations are compacted with. Defaults to the active model.",
        COMPACTION_MODEL,
    ),
    FieldSpec::declared("auto_compact_threshold", FieldKind::Int, REPLACE)
        .popular()
        .published(
            FieldDefault::Int(DEFAULT_AUTO_COMPACT_THRESHOLD),
            "Token count above which a conversation is compacted. A model may set its own.",
            r#"{"minimum": 0}"#,
        ),
    FieldSpec::declared("active_transcribe_model", FieldKind::Str, REPLACE).published(
        FieldDefault::Str("voxtral-realtime"),
        "Alias of the model voice input is transcribed with.",
        "",
    ),
    FieldSpec::union("transcribe_providers", FieldKind::Complex, "name").published(
        FieldDefault::Json(DEFAULT_TRANSCRIBE_PROVIDERS),
        "Providers serving transcription models.",
        TRANSCRIBE_PROVIDER_ITEMS,
    ),
    FieldSpec::union("transcribe_models", FieldKind::Complex, "alias").published(
        FieldDefault::Json(DEFAULT_TRANSCRIBE_MODELS),
        "Transcription model definitions.",
        TRANSCRIBE_MODEL_ITEMS,
    ),
    FieldSpec::declared("active_tts_model", FieldKind::Str, REPLACE).published(
        FieldDefault::Str("voxtral-tts"),
        "Alias of the model responses are read aloud with.",
        "",
    ),
    FieldSpec::union("tts_providers", FieldKind::Complex, "name").published(
        FieldDefault::Json(DEFAULT_TTS_PROVIDERS),
        "Providers serving speech models.",
        TTS_PROVIDER_ITEMS,
    ),
    FieldSpec::union("tts_models", FieldKind::Complex, "alias").published(
        FieldDefault::Json(DEFAULT_TTS_MODELS),
        "Speech model definitions.",
        TTS_MODEL_ITEMS,
    ),
    // Tools
    FieldSpec::declared("tools", FieldKind::Complex, DEEP_MERGE).published(
        FieldDefault::None,
        "Per-tool settings, keyed by tool name.",
        TOOL_SETTINGS,
    ),
    FieldSpec::declared("tool_paths", FieldKind::List, CONCAT).published(
        FieldDefault::Strings(&[]),
        "Extra directories or files to load custom tools from.",
        "",
    ),
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
            FieldDefault::Json(EMPTY_LIST),
            "MCP server definitions.",
            MCP_SERVER_ITEMS,
        ),
    FieldSpec::declared("enable_connectors", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(true),
        "Publish the tools remote connectors expose.",
        "",
    ),
    FieldSpec::union("connectors", FieldKind::Complex, "name").published(
        FieldDefault::Json(EMPTY_LIST),
        "Persistent connector enablement preferences.",
        CONNECTOR_ITEMS,
    ),
    // Agents
    FieldSpec::declared("agent_paths", FieldKind::List, CONCAT).published(
        FieldDefault::Strings(&[]),
        "Extra directories to load agent profiles from.",
        "",
    ),
    FieldSpec::declared("enabled_agents", FieldKind::List, CONCAT).published(
        FieldDefault::Strings(&[]),
        "Agent names or patterns to publish. When set, only matching agents are available. Globs and `re:` regular expressions are supported.",
        "",
    ),
    FieldSpec::declared("disabled_agents", FieldKind::List, CONCAT).published(
        FieldDefault::Strings(&[]),
        "Agent names or patterns to withhold. Ignored when `enabled_agents` is set.",
        "",
    ),
    FieldSpec::declared("installed_agents", FieldKind::List, CONCAT).published(
        FieldDefault::Strings(&[]),
        "Opt-in builtin agents this machine has installed.",
        "",
    ),
    FieldSpec::declared("default_agent", FieldKind::Str, REPLACE)
        .popular()
        .published(
            FieldDefault::Str("default"),
            "Agent profile used when none is requested on the command line.",
            "",
        ),
    // Skills
    FieldSpec::declared("skill_paths", FieldKind::List, CONCAT).published(
        FieldDefault::Strings(&[]),
        "Extra directories to load skills from.",
        "",
    ),
    FieldSpec::declared("enabled_skills", FieldKind::List, CONCAT).published(
        FieldDefault::Strings(&[]),
        "Skill names or patterns to publish. When set, only matching skills are available.",
        "",
    ),
    FieldSpec::declared("disabled_skills", FieldKind::List, CONCAT).published(
        FieldDefault::Strings(&[]),
        "Skill names or patterns to withhold. Ignored when `enabled_skills` is set.",
        "",
    ),
    FieldSpec::declared(
        "experimental_enable_registry_skills",
        FieldKind::Bool,
        REPLACE,
    )
    .published(
        FieldDefault::Bool(false),
        "Experimental: also load workspace skills published by the Mistral registry.",
        "",
    ),
    // Internal
    FieldSpec::declared("vibe_code_enabled", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(true),
        "Allow the hosted Vibe Code surface.",
        "",
    ),
    FieldSpec::declared("vibe_code_api_key_env_var", FieldKind::Str, REPLACE).published(
        FieldDefault::Str("MISTRAL_API_KEY"),
        "Environment variable holding the key the hosted surface authenticates with.",
        "",
    ),
    FieldSpec::declared("enable_otel", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(false),
        "Export OpenTelemetry spans.",
        "",
    ),
    FieldSpec::declared("otel_endpoint", FieldKind::Str, REPLACE).published(
        FieldDefault::Str(""),
        "Collector the OpenTelemetry exporter sends spans to.",
        "",
    ),
    FieldSpec::declared("otel_redaction", FieldKind::Enum, REPLACE)
        .choices(OTEL_REDACTION_VALUES)
        .published(
            FieldDefault::Str("default"),
            "How much span content is removed before export.",
            "",
        ),
    FieldSpec::declared("console_base_url", FieldKind::Str, REPLACE).published(
        FieldDefault::Str("https://console.mistral.ai"),
        "Console the browser sign-in flow opens.",
        "",
    ),
    // Top-level scalars
    FieldSpec::declared("theme", FieldKind::Enum, REPLACE)
        .choices(&THEME_VALUES)
        .popular()
        .published(
            FieldDefault::Str("auto"),
            "Terminal color theme.",
            "",
        ),
    FieldSpec::declared("applied_migrations", FieldKind::List, CONCAT).published(
        FieldDefault::Strings(&[]),
        "Configuration migrations already applied to this file.",
        "",
    ),
    FieldSpec::declared("disable_welcome_banner_animation", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(false),
        "Draw the welcome banner without its animation.",
        "",
    ),
    FieldSpec::declared("autocopy_to_clipboard", FieldKind::Bool, REPLACE)
        .popular()
        .published(
            FieldDefault::Bool(true),
            "Copy selected transcript text to the clipboard as it is selected.",
            "",
        ),
    FieldSpec::declared("file_watcher_for_autocomplete", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(false),
        "Watch the workspace so path completion stays current.",
        "",
    ),
    FieldSpec::declared("ask_confirmation_on_exit", FieldKind::Bool, REPLACE)
        .popular()
        .published(
            FieldDefault::Bool(true),
            "Ask before leaving a session.",
            "",
        ),
    FieldSpec::declared("displayed_workdir", FieldKind::Str, REPLACE).published(
        FieldDefault::Str(""),
        "Working directory shown in the interface, when it should differ from the real one.",
        "",
    ),
    FieldSpec::declared("context_warnings", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(false),
        "Warn as the conversation approaches its compaction threshold.",
        "",
    ),
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
        FieldDefault::Bool(false),
        "Show reasoning regions in the transcript.",
        "",
    ),
    FieldSpec::declared("bypass_tool_permissions", FieldKind::Bool, REPLACE)
        .popular()
        .published(
            FieldDefault::Bool(false),
            "Run every tool without asking for approval.",
            "",
        ),
    FieldSpec::declared("raise_on_compaction_failure", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(false),
        "Fail the turn when compaction fails instead of carrying on.",
        "",
    ),
    FieldSpec::declared("enable_telemetry", FieldKind::Bool, REPLACE)
        .popular()
        .published(
            FieldDefault::Bool(true),
            "Send anonymous usage telemetry.",
            "",
        ),
    FieldSpec::declared("system_prompt_id", FieldKind::Str, REPLACE).published(
        FieldDefault::Str("cli"),
        "Identifier of the system prompt the agent runs with.",
        "",
    ),
    FieldSpec::declared("managed_shell_tools_enabled", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(false),
        "Publish the managed shell session family in place of the one-shot command tool.",
        "",
    ),
    FieldSpec::declared("compaction_prompt_id", FieldKind::Str, REPLACE).published(
        FieldDefault::Str("compact"),
        "Identifier of the prompt a conversation is compacted with.",
        "",
    ),
    FieldSpec::declared("include_commit_signature", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(true),
        "Sign commits the agent creates.",
        "",
    ),
    FieldSpec::declared("include_model_info", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(true),
        "Show which model answered.",
        "",
    ),
    FieldSpec::declared("include_project_context", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(true),
        "Send repository context with the first prompt of a session.",
        "",
    ),
    FieldSpec::declared("include_prompt_detail", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(true),
        "Show the expanded prompt detail in the transcript.",
        "",
    ),
    FieldSpec::declared("enable_update_checks", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(true),
        "Check for new releases in the background.",
        "",
    ),
    FieldSpec::declared("enable_auto_update", FieldKind::Bool, REPLACE)
        .popular()
        .published(
            FieldDefault::Bool(true),
            "Install a new release once one is found.",
            "",
        ),
    FieldSpec::declared("enable_notifications", FieldKind::Bool, REPLACE)
        .popular()
        .published(
            FieldDefault::Bool(true),
            "Allow desktop notifications.",
            "",
        ),
    FieldSpec::declared("enable_system_trust_store", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(false),
        "Validate TLS against the operating system trust store.",
        "",
    ),
    FieldSpec::declared("api_timeout", FieldKind::Float, REPLACE).published(
        FieldDefault::Float(720.0),
        "Seconds a provider request may take before it is abandoned.",
        r#"{"exclusiveMinimum": 0}"#,
    ),
    FieldSpec::declared("api_retry_max_elapsed_time", FieldKind::Float, REPLACE).published(
        FieldDefault::Float(300.0),
        "Seconds spent retrying a provider request before giving up.",
        r#"{"exclusiveMinimum": 0}"#,
    ),
    FieldSpec::declared("vibe_base_url", FieldKind::Str, REPLACE).published(
        FieldDefault::Str("https://chat.mistral.ai"),
        "Base URL of the hosted chat surface.",
        "",
    ),
    FieldSpec::declared("vibe_code_sessions_base_url", FieldKind::Str, REPLACE).published(
        FieldDefault::Str("https://chat.mistral.ai"),
        "Base URL sessions are shared through.",
        "",
    ),
    // Nested configs
    FieldSpec::declared("project_context", FieldKind::Complex, REPLACE).published(
        FieldDefault::Json(DEFAULT_PROJECT_CONTEXT),
        "How much repository context is gathered.",
        PROJECT_CONTEXT,
    ),
    FieldSpec::declared("session_logging", FieldKind::Complex, REPLACE).published(
        FieldDefault::Json(DEFAULT_SESSION_LOGGING),
        "Where session transcripts are written.",
        SESSION_LOGGING,
    ),
    FieldSpec::declared("experiments", FieldKind::Complex, REPLACE).published(
        FieldDefault::Json(DEFAULT_EXPERIMENTS),
        "Remote experiment service settings.",
        EXPERIMENTS,
    ),
    // Fields only this port declares. Each is a recorded divergence rather than
    // a mapping onto a reference field: `thinking` is per-model upstream,
    // `notifications` is a tri-state where upstream carries a boolean, and the
    // remaining three name no reference field at all. The reasons are written
    // out in the divergence table of `tasks/prd-config-parity.md`.
    FieldSpec::declared("thinking", FieldKind::Enum, REPLACE)
        .local()
        .choices(THINKING_VALUES)
        .published(
            FieldDefault::Str("off"),
            "Reasoning effort for the active model.",
            "",
        ),
    FieldSpec::declared("notifications", FieldKind::Enum, REPLACE)
        .local()
        .choices(NOTIFICATION_VALUES)
        .published(
            FieldDefault::Str("unfocused"),
            "When desktop notifications may be sent.",
            "",
        ),
    FieldSpec::declared("proxy", FieldKind::Str, REPLACE)
        .local()
        .published(
            FieldDefault::None,
            "Legacy proxy URL. Prefer /proxy-setup for protocol-specific values.",
            r#"{"type": ["string", "null"], "format": "uri"}"#,
        ),
    FieldSpec::declared("tls_ca_path", FieldKind::Str, REPLACE)
        .local()
        .published(
            FieldDefault::None,
            "Legacy TLS certificate path. Prefer /proxy-setup.",
            r#"{"type": ["string", "null"]}"#,
        ),
    FieldSpec::declared("dotenv_path", FieldKind::Str, REPLACE)
        .local()
        .published(
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

/// The document the Defaults layer composes: one entry per declared default.
///
/// [`FieldSpec::local`] fields stay out, so the shipped document keeps matching
/// the reference `create_default_config()` field for field, which the corpus
/// asserts. `tools` carries no default either: its contents come from tool
/// discovery, which this port owns.
///
/// `session_logging.save_dir` is empty here and resolved under the vibe home
/// when the configuration loads, exactly where the reference resolves it.
#[must_use]
pub fn default_document() -> toml::Table {
    FIELDS
        .iter()
        .filter(|spec| !spec.local)
        .filter_map(|spec| Some((spec.name.to_owned(), spec.default.to_toml()?)))
        .collect()
}

/// The JSON Schema published by `config/schema`, generated from the registry.
///
/// Built once per process, so a client caching it by
/// [`schema_version`] never sees two different encodings of one surface.
#[must_use]
pub fn json_schema() -> JsonValue {
    published_schema().0.clone()
}

/// The token identifying the published schema, as `sha256:<hex>` over its
/// canonical encoding. Reference `config_schema_response`.
#[must_use]
pub fn schema_version() -> &'static str {
    &published_schema().1
}

fn published_schema() -> &'static (JsonValue, String) {
    static SCHEMA: OnceLock<(JsonValue, String)> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let mut properties = Map::new();
        for spec in FIELDS.iter().filter(|spec| spec.published) {
            properties.insert(spec.name.to_owned(), property_schema(spec));
        }
        let schema = JsonValue::Object(Map::from_iter([
            ("type".to_owned(), JsonValue::from("object")),
            ("additionalProperties".to_owned(), JsonValue::from(true)),
            ("properties".to_owned(), JsonValue::Object(properties)),
        ]));
        // `serde_json::Map` is ordered, so this encoding is the sorted, compact
        // one the reference hashes.
        let encoded = schema.to_string();
        let digest = Sha256::digest(encoded.as_bytes());
        let version = format!("sha256:{}", hex_encode(&digest));
        (schema, version)
    })
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
