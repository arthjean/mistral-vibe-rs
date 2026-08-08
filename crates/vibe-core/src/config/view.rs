//! The configuration as the app-server publishes it.
//!
//! `ConfigView` is the wire shape a settings screen renders from: the active
//! model, the toggles a client mirrors, the two audio surfaces and whatever the
//! load repaired. It is deliberately narrower than [`ConfigSnapshot::public_view`],
//! which carries every layer and every effective key, because the wire contract
//! declares exactly these fields and rejects any other.
//!
//! Everything here is read from the effective table rather than from a typed
//! schema: the merge already resolved precedence and filled the per-entry
//! defaults, so a view is a projection rather than a second interpretation.

use serde_json::{Value as JsonValue, json};
use toml::{Table, Value};

use super::ConfigSnapshot;
use crate::middleware::{CompactionSettings, DEFAULT_COMPACTION_PROMPT_ID};

/// The audio client the wire declares. Only one is served today, and an entry
/// naming anything else is published under it rather than failing the view.
const AUDIO_CLIENT: &str = "mistral";
/// The thinking levels a model entry may declare. An entry outside the set is
/// published as `off`, which is what a client renders for a model that does not
/// think.
const THINKING_LEVELS: [&str; 5] = ["off", "low", "medium", "high", "max"];

impl ConfigSnapshot {
    /// The 18-field `ConfigView` the app-server publishes.
    #[must_use]
    pub fn config_view(&self) -> JsonValue {
        json!({
            "activeModel": self.active_model_view(),
            "theme": self.string_field("theme", "default"),
            "disableWelcomeBannerAnimation": self.bool_field("disable_welcome_banner_animation", false),
            "autocopyToClipboard": self.bool_field("autocopy_to_clipboard", false),
            "fileWatcherForAutocomplete": self.bool_field("file_watcher_for_autocomplete", false),
            "askConfirmationOnExit": self.bool_field("ask_confirmation_on_exit", true),
            "voiceModeEnabled": self.bool_field("voice_mode_enabled", false),
            "narratorEnabled": self.bool_field("narrator_enabled", false),
            "showThinkingNodes": self.bool_field("show_thinking_nodes", true),
            "enableUpdateChecks": self.bool_field("enable_update_checks", true),
            "enableNotifications": self.bool_field("enable_notifications", false),
            "vibeCodeEnabled": self.bool_field("vibe_code_enabled", false),
            "models": self.model_views(),
            "transcribeModels": self.audio_aliases("transcribe_models"),
            "ttsModels": self.audio_aliases("tts_models"),
            "transcription": {
                "model": self.transcribe_model_view(),
                "provider": self.audio_provider_view(
                    "transcribe_providers",
                    self.active_audio_entry("transcribe_models", "active_transcribe_model"),
                ),
            },
            "speech": {
                "model": self.tts_model_view(),
                "provider": self.audio_provider_view(
                    "tts_providers",
                    self.active_audio_entry("tts_models", "active_tts_model"),
                ),
            },
            "validationWarnings": self.validation_warnings,
        })
    }

    /// The provider entry the active model is served from, when the two names
    /// resolve. `account/read` classifies the account from its backend, so the
    /// lookup lives beside the view that publishes the model.
    #[must_use]
    pub fn active_provider(&self) -> Option<Table> {
        let provider = self.active_model()?.get("provider")?.as_str()?.to_owned();
        self.entries("providers")
            .into_iter()
            .find(|entry| entry.get("name").and_then(Value::as_str) == Some(provider.as_str()))
    }

    /// The five compaction keys, read as one policy.
    ///
    /// The threshold is the active model's, which the load already filled from
    /// the global value for a model that declares none
    /// (`_apply_global_auto_compact_threshold`); the global key is still read
    /// as a fallback, because a document composed without the shipped defaults
    /// never went through that propagation. A negative threshold cannot be
    /// represented and reads as zero, which is the value that disables
    /// compaction.
    ///
    /// The model is resolved the way `get_compaction_model` resolves it: the
    /// configured `compaction_model` first, the active model otherwise.
    #[must_use]
    pub fn compaction_settings(&self) -> CompactionSettings {
        let active = self.active_model();
        let threshold = active
            .as_ref()
            .and_then(|entry| entry.get("auto_compact_threshold"))
            .or_else(|| self.effective.get("auto_compact_threshold"))
            .and_then(Value::as_integer)
            .and_then(|threshold| u64::try_from(threshold).ok())
            .unwrap_or_default();
        let compaction_model = self
            .effective
            .get("compaction_model")
            .and_then(Value::as_table)
            .and_then(|entry| entry.get("name"))
            .or_else(|| active.as_ref().and_then(|entry| entry.get("name")))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        CompactionSettings {
            auto_compact_threshold: threshold,
            compaction_model,
            compaction_prompt_id: self
                .string_field("compaction_prompt_id", DEFAULT_COMPACTION_PROMPT_ID),
            context_warnings: self.bool_field("context_warnings", false),
            raise_on_compaction_failure: self.bool_field("raise_on_compaction_failure", false),
        }
    }

    /// The active model entry, keyed by the alias `active_model` names.
    ///
    /// `models` is written as an array and read back keyed by alias, so both
    /// forms are resolved here rather than at every call site.
    fn active_model(&self) -> Option<Table> {
        let alias = self.effective.get("active_model")?.as_str()?;
        match self.effective.get("models") {
            Some(Value::Table(models)) => {
                models
                    .get(alias)
                    .and_then(Value::as_table)
                    .cloned()
                    .map(|mut entry| {
                        entry
                            .entry("alias".to_owned())
                            .or_insert_with(|| Value::String(alias.to_owned()));
                        entry
                    })
            }
            Some(Value::Array(models)) => models
                .iter()
                .filter_map(Value::as_table)
                .find(|entry| entry.get("alias").and_then(Value::as_str) == Some(alias))
                .cloned(),
            _ => None,
        }
    }

    fn active_model_view(&self) -> JsonValue {
        self.active_model()
            .map_or_else(empty_model_view, |entry| model_view(&entry, ""))
    }

    fn model_views(&self) -> JsonValue {
        match self.effective.get("models") {
            Some(Value::Table(models)) => JsonValue::Array(
                models
                    .iter()
                    .filter_map(|(alias, entry)| {
                        entry.as_table().map(|entry| model_view(entry, alias))
                    })
                    .collect(),
            ),
            Some(Value::Array(models)) => JsonValue::Array(
                models
                    .iter()
                    .filter_map(Value::as_table)
                    .map(|entry| model_view(entry, ""))
                    .collect(),
            ),
            _ => JsonValue::Array(Vec::new()),
        }
    }

    /// The aliases of an audio model list, which is what a client offers as
    /// choices.
    fn audio_aliases(&self, key: &str) -> JsonValue {
        JsonValue::Array(
            self.entries(key)
                .into_iter()
                .filter_map(|entry| {
                    entry
                        .get("alias")
                        .and_then(Value::as_str)
                        .map(|alias| JsonValue::String(alias.to_owned()))
                })
                .collect(),
        )
    }

    /// The entry of `key` whose alias matches the value of `active`, falling
    /// back to the first one so a mistyped active alias still renders.
    fn active_audio_entry(&self, key: &str, active: &str) -> Option<Table> {
        let entries = self.entries(key);
        let alias = self.effective.get(active).and_then(Value::as_str);
        entries
            .iter()
            .find(|entry| entry.get("alias").and_then(Value::as_str) == alias)
            .or_else(|| entries.first())
            .cloned()
    }

    fn transcribe_model_view(&self) -> JsonValue {
        let entry = self.active_audio_entry("transcribe_models", "active_transcribe_model");
        let entry = entry.unwrap_or_default();
        json!({
            "name": table_str(&entry, "name", ""),
            "sampleRate": table_int(&entry, "sample_rate"),
            "encoding": table_str(&entry, "encoding", "pcm_s16le"),
            "language": table_str(&entry, "language", ""),
            "targetStreamingDelayMs": table_int(&entry, "target_streaming_delay_ms"),
        })
    }

    fn tts_model_view(&self) -> JsonValue {
        let entry = self.active_audio_entry("tts_models", "active_tts_model");
        let entry = entry.unwrap_or_default();
        json!({
            "name": table_str(&entry, "name", ""),
            "voice": table_str(&entry, "voice", ""),
            "responseFormat": table_str(&entry, "response_format", "wav"),
        })
    }

    /// The provider serving `model`, published under the audio provider shape.
    fn audio_provider_view(&self, key: &str, model: Option<Table>) -> JsonValue {
        let wanted = model
            .as_ref()
            .and_then(|entry| entry.get("provider"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let entries = self.entries(key);
        let provider = entries
            .iter()
            .find(|entry| entry.get("name").and_then(Value::as_str) == wanted.as_deref())
            .or_else(|| entries.first())
            .cloned()
            .unwrap_or_default();
        json!({
            "apiBase": table_str(&provider, "api_base", ""),
            "apiKeyEnvVar": table_str(&provider, "api_key_env_var", ""),
            "client": AUDIO_CLIENT,
        })
    }

    /// The tables of a top-level array key, skipping anything that is not one.
    fn entries(&self, key: &str) -> Vec<Table> {
        self.effective
            .get(key)
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_table)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn bool_field(&self, key: &str, fallback: bool) -> bool {
        self.effective
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(fallback)
    }

    fn string_field(&self, key: &str, fallback: &str) -> String {
        self.effective
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or(fallback)
            .to_owned()
    }
}

fn model_view(entry: &Table, alias: &str) -> JsonValue {
    let declared = table_str(entry, "alias", alias);
    let thinking = table_str(entry, "thinking", "off");
    json!({
        "name": table_str(entry, "name", ""),
        "alias": declared,
        "thinking": if THINKING_LEVELS.contains(&thinking.as_str()) {
            thinking
        } else {
            "off".to_owned()
        },
        "supportsImages": entry
            .get("supports_images")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// The view a configuration with no resolvable active model publishes.
///
/// The reference raises instead, but a raise here would take the whole
/// response down; a client renders an empty model the same way it renders a
/// missing one.
fn empty_model_view() -> JsonValue {
    json!({"name": "", "alias": "", "thinking": "off", "supportsImages": false})
}

fn table_str(entry: &Table, key: &str, fallback: &str) -> String {
    entry
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_owned()
}

fn table_int(entry: &Table, key: &str) -> i64 {
    entry.get(key).and_then(Value::as_integer).unwrap_or(0)
}
