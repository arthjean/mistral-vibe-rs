//! The declarations themselves: one [`FieldSpec`] per configuration field,
//! with the schema fragments and JSON defaults they carry.
//!
//! A pure data table. Field names, merge strategies, merge keys, editor kinds,
//! the popular set and default values are behavioral observations taken from
//! the pinned reference; descriptions are original prose.

use super::{
    FieldDefault, FieldKind, FieldSpec, MergeStrategy, UNPINNED_ACTIVE_MODEL,
    UNSET_AUTO_COMPACT_THRESHOLD,
};
use crate::config::THEME_VALUES;
use crate::provider::config::ThinkingLevel;

const REPLACE: MergeStrategy = MergeStrategy::Replace;
const CONCAT: MergeStrategy = MergeStrategy::Concat;
const SHALLOW_MERGE: MergeStrategy = MergeStrategy::Merge;
const DEEP_MERGE: MergeStrategy = MergeStrategy::DeepMerge;

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
            "browser_auth_allow_origin_rewrite": {"type": "boolean"},
            "api_style": {"type": "string"},
            "backend": {"type": "string"},
            "reasoning_field_name": {"type": "string"},
            "emits_finish_reason": {"type": "boolean"},
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
            "thinking_levels": {"type": "array", "items": {"enum": ["off", "low", "medium", "high", "max"]}},
            "supports_images": {"type": "boolean"},
            "max_context_length": {"type": "integer", "minimum": 1},
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
        "thinking_levels": {"type": "array", "items": {"enum": ["off", "low", "medium", "high", "max"]}},
        "supports_images": {"type": "boolean"},
        "max_context_length": {"type": "integer", "minimum": 1},
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
        "thinking_levels": {"type": "array", "items": {"enum": ["off", "low", "medium", "high", "max"]}},
        "supports_images": {"type": "boolean"},
        "max_context_length": {"type": "integer", "minimum": 1},
        "auto_compact_threshold": {"type": "integer"}
    }
}"#;

/// `routed_extra_models` is written by the experiments layer as the JSON text
/// of a list of model definitions and read back as the list itself, so both
/// forms validate. Reference `_coerce_routed_extra_models` is the coercion.
const ROUTED_EXTRA_MODELS: &str = r#"{
    "type": ["array", "string"],
    "items": {"type": "object"}
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
        "enabled": {"type": "boolean"},
        "generate_titles": {"type": "boolean"}
    }
}"#;

const UTILITY_MODELS: &str = r#"{
    "type": "object",
    "properties": {
        "title": {"type": "string"},
        "smart_approve": {"type": "string"}
    }
}"#;

/// Reference `UtilityModelsConfig()`: no feature names a model.
const DEFAULT_UTILITY_MODELS: &str = r#"{"title": "", "smart_approve": ""}"#;

const EXPERIMENTS: &str = r#"{
    "type": "object",
    "properties": {
        "enable": {"type": "boolean"},
        "api_host": {"type": "string"},
        "client_key": {"type": "string"}
    }
}"#;

/// The two providers the reference ships.
pub(super) const DEFAULT_PROVIDERS: &str = r#"[
    {
        "name": "mistral",
        "api_base": "https://api.mistral.ai/v1",
        "api_key_env_var": "MISTRAL_API_KEY",
        "browser_auth_base_url": "https://console.mistral.ai",
        "browser_auth_api_base_url": "https://console.mistral.ai/api",
        "browser_auth_allow_origin_rewrite": false,
        "api_style": "openai",
        "backend": "mistral",
        "reasoning_field_name": "reasoning_content",
        "emits_finish_reason": true,
        "project_id": "",
        "region": "",
        "extra_headers": {}
    },
    {
        "name": "llamacpp",
        "api_base": "http://127.0.0.1:8080/v1",
        "api_key_env_var": "",
        "browser_auth_allow_origin_rewrite": false,
        "api_style": "openai",
        "backend": "generic",
        "reasoning_field_name": "reasoning_content",
        "emits_finish_reason": true,
        "project_id": "",
        "region": "",
        "extra_headers": {}
    }
]"#;

/// The two models the reference ships, in the persisted array form. A field
/// the reference leaves unset is absent rather than null: TOML has no null.
const DEFAULT_MODELS: &str = r#"[
    {
        "name": "mistral-vibe-cli-latest",
        "provider": "mistral",
        "alias": "mistral-medium-3.5",
        "display_name": "Mistral Medium 3.5",
        "temperature": 1.0,
        "input_price": 1.5,
        "output_price": 7.5,
        "cached_input_price": 0.15,
        "thinking": "high",
        "thinking_levels": ["off", "low", "medium", "high", "max"],
        "supports_images": true
    },
    {
        "name": "devstral",
        "provider": "llamacpp",
        "alias": "local",
        "display_name": "Devstral (local)",
        "temperature": 0.2,
        "input_price": 0.0,
        "output_price": 0.0,
        "thinking": "off",
        "thinking_levels": ["off", "low", "medium", "high", "max"],
        "supports_images": false
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
    r#"{"save_dir": "", "session_prefix": "session", "enabled": true, "generate_titles": true}"#;

const DEFAULT_EXPERIMENTS: &str = r#"{
    "enable": true,
    "api_host": "https://experiments.mistral.services/",
    "client_key": "sdk-OE8yJgTXZY6tj"
}"#;

const EMPTY_LIST: &str = "[]";

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
    FieldSpec::declared("routed_extra_models", FieldKind::Complex, REPLACE).published(
        FieldDefault::Json(EMPTY_LIST),
        "Models an experiment adds to the picker without changing the default.",
        ROUTED_EXTRA_MODELS,
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
    FieldSpec::declared("allowed_models", FieldKind::List, REPLACE).published(
        FieldDefault::Strings(&[]),
        "Model names, glob or `re:` patterns, that may be selected. Empty allows every model.",
        "",
    ),
    FieldSpec::declared("compaction_model", FieldKind::Complex, SHALLOW_MERGE).published(
        FieldDefault::None,
        "Model conversations are compacted with. Defaults to the active model.",
        COMPACTION_MODEL,
    ),
    FieldSpec::declared("vision_model", FieldKind::Complex, SHALLOW_MERGE).published(
        FieldDefault::None,
        "Image-capable model that describes images for an active model that cannot see them.",
        COMPACTION_MODEL,
    ),
    FieldSpec::declared("utility_models", FieldKind::Complex, SHALLOW_MERGE).published(
        FieldDefault::Json(DEFAULT_UTILITY_MODELS),
        "Model alias each background feature (session titles, the smart-approve classifier) \
         runs on, or `active` for the session's model. Empty selects automatically.",
        UTILITY_MODELS,
    ),
    FieldSpec::declared("auto_compact_threshold", FieldKind::Int, REPLACE)
        .popular()
        .published(
            FieldDefault::Int(UNSET_AUTO_COMPACT_THRESHOLD),
            "Token count above which a conversation is compacted, for a model that sets no \
             threshold of its own and declares no context window; one with a window compacts \
             at 80% of it. 0 never compacts.",
            "",
        )
        .unadvertised_default(),
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
            FieldDefault::Str("accept-edits"),
            "Agent profile used when none is requested on the command line.",
            "",
        ),
    FieldSpec::declared("smart_approve_available", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(false),
        "Offer the smart-approve agent in the agent picker and cycle.",
        "",
    ),
    FieldSpec::declared("smart_approve_default", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(false),
        "Start sessions under the smart-approve agent, which also offers it.",
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
        FieldDefault::Str(crate::auth::DEFAULT_CONSOLE_BASE_URL),
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
    FieldSpec::declared("show_greeting", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(true),
        "Greet the signed-in user under the banner, at most once a day, on Mistral providers.",
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
        FieldDefault::Bool(true),
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
    FieldSpec::declared("show_subagent_status_list", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(true),
        "List running subagents above the prompt and let their transcripts be opened.",
        "",
    ),
    FieldSpec::declared("worktree_limit", FieldKind::Int, REPLACE).published(
        FieldDefault::Int(15),
        "How many inactive managed worktrees are kept before the oldest are pruned.",
        r#"{"minimum": 0, "maximum": 100}"#,
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
    FieldSpec::declared("experimental_enable_tab_status", FieldKind::Bool, REPLACE).published(
        FieldDefault::Bool(true),
        "Prefix the terminal title with the session's running or waiting state.",
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
    FieldSpec::declared("api_connect_timeout", FieldKind::Float, REPLACE).published(
        FieldDefault::Float(10.0),
        "Seconds a provider connection may take to open.",
        r#"{"exclusiveMinimum": 0}"#,
    ),
    FieldSpec::declared("api_write_timeout", FieldKind::Float, REPLACE).published(
        FieldDefault::Float(30.0),
        "Seconds a provider request may take to send.",
        r#"{"exclusiveMinimum": 0}"#,
    ),
    FieldSpec::declared("api_pool_timeout", FieldKind::Float, REPLACE).published(
        FieldDefault::Float(10.0),
        "Seconds a provider request may wait for a free connection.",
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
    FieldSpec::declared("log_level", FieldKind::Str, REPLACE).published(
        FieldDefault::None,
        "Lowest severity written to the session log; LOG_LEVEL and /log-level take precedence.",
        r#"{"type": ["string", "null"]}"#,
    ),
    // Nested configs
    FieldSpec::declared("project_context", FieldKind::Complex, SHALLOW_MERGE).published(
        FieldDefault::Json(DEFAULT_PROJECT_CONTEXT),
        "How much repository context is gathered.",
        PROJECT_CONTEXT,
    ),
    FieldSpec::declared("session_logging", FieldKind::Complex, SHALLOW_MERGE).published(
        FieldDefault::Json(DEFAULT_SESSION_LOGGING),
        "Where session transcripts are written.",
        SESSION_LOGGING,
    ),
    FieldSpec::declared("experiments", FieldKind::Complex, SHALLOW_MERGE).published(
        FieldDefault::Json(DEFAULT_EXPERIMENTS),
        "Remote experiment service settings.",
        EXPERIMENTS,
    ),
    // Fields only this port declares. Each is a recorded divergence rather than
    // a mapping onto a reference field: `thinking` is per-model upstream, and
    // the remaining three name no reference field at all. The reasons are written
    // out in the divergence table of `docs/parity.md`.
    FieldSpec::declared("thinking", FieldKind::Enum, REPLACE)
        .local()
        .choices(&ThinkingLevel::NAMES)
        .published(
            FieldDefault::Str("off"),
            "Reasoning effort for the active model.",
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
