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

use crate::pycompat::pydantic;
use crate::text::hex_encode;

mod fields;

pub use fields::FIELDS;

/// Reference `UNPINNED_ACTIVE_MODEL`: the `active_model` value meaning "the
/// operator pinned nothing". It is resolved when the alias is read rather than
/// when the document is composed, so a routed default can still select the
/// model an installation runs on.
pub const UNPINNED_ACTIVE_MODEL: &str = "";

/// Reference `DEFAULT_ACTIVE_MODEL_CONFIG.alias`, which is what an unpinned
/// installation resolves to when no experiment routed it elsewhere.
pub const DEFAULT_ACTIVE_MODEL_ALIAS: &str = "mistral-medium-3.5";

/// How a field's values from two layers combine. These are the five strategies
/// the reference schema actually declares; `conflict` exists in the reference
/// vocabulary but no field adopts it, so it is unreachable and is not
/// implemented here. [`super::surface_parity_tests`] fails if that ever stops
/// being true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStrategy {
    /// The higher layer wins outright.
    Replace,
    /// Lists append in layer order, duplicates preserved.
    Concat,
    /// Lists are keyed by [`FieldSpec::merge_key`]; the higher layer wins per
    /// key and first-seen order is preserved.
    Union,
    /// Tables merge one level deep: each top-level key the higher layer sets
    /// wins whole, nested tables and lists included, and the keys it omits
    /// survive. Reference `WithShallowMerge`.
    Merge,
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
            Self::Merge => "merge",
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
    /// Whether `config/schema` publishes [`Self::default`]. A sentinel default
    /// that resolves to a rule rather than a number stays out of the schema.
    pub advertises_default: bool,
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
            advertises_default: true,
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

    /// Reference `_suppress_unset_default`: the default is composed into the
    /// Defaults layer and kept out of the published schema.
    const fn unadvertised_default(self) -> Self {
        Self {
            advertises_default: false,
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

pub(super) const THINKING_VALUES: &[&str] = &["off", "low", "medium", "high", "max"];

/// The providers the reference ships, which a provider named by an onboarding
/// client resolves against after the configured ones (reference
/// `DEFAULT_PROVIDERS`).
#[must_use]
pub fn shipped_providers() -> Vec<toml::Table> {
    serde_json::from_str::<JsonValue>(fields::DEFAULT_PROVIDERS)
        .ok()
        .and_then(|value| toml::Value::try_from(value).ok())
        .and_then(|value| match value {
            toml::Value::Array(entries) => Some(
                entries
                    .into_iter()
                    .filter_map(|entry| match entry {
                        toml::Value::Table(table) => Some(table),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

/// The compaction threshold a model compacts at when no layer set one and it
/// declares no context window. Reference `DEFAULT_AUTO_COMPACT_THRESHOLD`.
pub const DEFAULT_AUTO_COMPACT_THRESHOLD: i64 = 200_000;

/// "No layer set a threshold", the default of the global
/// `auto_compact_threshold` and of the same field on every model. The Defaults
/// layer composes field defaults into the merge, so unset-ness has to survive
/// it as a value; a `-1` written by hand is read the same way, and `0` stays a
/// cap meaning "never compact". The load resolves it on every entry of
/// `models`, and no written document carries it. Reference
/// `UNSET_AUTO_COMPACT_THRESHOLD`.
pub const UNSET_AUTO_COMPACT_THRESHOLD: i64 = -1;

/// The share of a declared context window a model that sets no threshold
/// compacts at: compaction is itself a call carrying the conversation, so it
/// needs room below the ceiling. Reference `AUTO_COMPACT_WINDOW_RATIO`.
pub const AUTO_COMPACT_WINDOW_RATIO: f64 = 0.8;

/// The per-entry defaults the reference `ModelConfig` fills in once a merged
/// model reaches validation. A merged entry only carries what its layers set,
/// so the load completes it from here.
///
/// `cached_input_price` and `max_context_length` are absent on purpose: they
/// default to null upstream and TOML carries no null, so an entry that sets
/// none stays without the key. `auto_compact_threshold` defaults to
/// [`UNSET_AUTO_COMPACT_THRESHOLD`], which the load resolves on the entries of
/// `models` and leaves on the model fields that are not entries of it.
pub const MODEL_DEFAULTS: &str = r#"{
    "temperature": 0.2,
    "input_price": 0.0,
    "output_price": 0.0,
    "thinking": "off",
    "thinking_levels": ["off", "low", "medium", "high", "max"],
    "supports_images": false,
    "auto_compact_threshold": -1
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
    if let Some(default) = spec.default.to_json().filter(|_| spec.advertises_default) {
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

/// Reference `VibeConfig` validation of a patched document: a top-level
/// field whose value its declared type cannot take refuses the whole patch.
///
/// Only what pydantic's lax mode would refuse is refused: a string field
/// takes nothing but a string, a boolean a boolean or one of the spellings
/// lax mode reads as one, a number a number or a numeric string, and a list
/// an array. Fields this registry does not type are left to their readers.
pub fn validate_field_types(table: &toml::Table) -> Result<(), String> {
    for (name, value) in table {
        let Some(spec) = field(name) else {
            continue;
        };
        let accepted = match spec.kind {
            FieldKind::Str | FieldKind::Enum => value.is_str(),
            FieldKind::Bool => match value {
                toml::Value::Boolean(_) => true,
                toml::Value::Integer(number) => matches!(number, 0 | 1),
                toml::Value::Float(number) => *number == 0.0 || *number == 1.0,
                toml::Value::String(text) => pydantic::lax_bool(text).is_some(),
                _ => false,
            },
            FieldKind::Int => match value {
                toml::Value::Integer(_) | toml::Value::Boolean(_) => true,
                toml::Value::Float(number) => number.fract() == 0.0,
                toml::Value::String(text) => pydantic::lax_int(text).is_some(),
                _ => false,
            },
            FieldKind::Float => match value {
                toml::Value::Integer(_) | toml::Value::Float(_) | toml::Value::Boolean(_) => true,
                toml::Value::String(text) => pydantic::lax_float(text).is_some(),
                _ => false,
            },
            FieldKind::List => value.is_array(),
            FieldKind::Complex => true,
        };
        if !accepted {
            return Err(format!("`{name}` cannot take the value it was given"));
        }
    }
    Ok(())
}
