//! What an audio session is resolved from.
//!
//! The reference builds its transcribe and speech clients from the projected
//! configuration and nothing else: the active model entry supplies the model
//! name and its wire values, the provider entry supplies the endpoint and the
//! name of the variable the credential is read from
//! (`vibe/cli/lazy_audio_managers.py:219-231`,
//! `vibe/cli/transcribe/mistral_transcribe_client.py:36-42` and
//! `vibe/cli/tts/mistral_tts_client.py:16-27`). This module reads the same
//! values off the view this port publishes through `config/read`, so changing
//! `active_transcribe_model` changes the session.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use vibe_core::auth::KeyringStore;
use vibe_core::config::DotenvValues;

/// Where a credential variable is read from: the process environment, the
/// global dotenv file and the system keyring, in the order the adapter that
/// owns the lookup reads them. Reference `resolve_api_key`.
pub type CredentialLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The view's `transcription` member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptionSettings {
    pub model: String,
    pub sample_rate: u32,
    pub encoding: String,
    pub target_streaming_delay_ms: u32,
    pub api_base: String,
    pub api_key_env_var: String,
}

impl TranscriptionSettings {
    /// The active transcription entry and its provider, or why the view names
    /// none. Reference `config.transcription.model`, which raises `KeyError`
    /// on a configuration that resolves no entry.
    ///
    /// # Errors
    ///
    /// A view whose transcription member names no model.
    pub fn from_config_view(view: &Value) -> Result<Self, String> {
        let model = string_at(view, "/transcription/model/name");
        if model.is_empty() {
            return Err(
                "Voice mode has no transcription model configured; declare a [[transcribe_models]] \
                 entry and name its alias in `active_transcribe_model`"
                    .to_owned(),
            );
        }
        Ok(Self {
            model,
            sample_rate: unsigned_at(view, "/transcription/model/sampleRate"),
            encoding: string_at(view, "/transcription/model/encoding"),
            target_streaming_delay_ms: unsigned_at(
                view,
                "/transcription/model/targetStreamingDelayMs",
            ),
            api_base: string_at(view, "/transcription/provider/apiBase"),
            api_key_env_var: string_at(view, "/transcription/provider/apiKeyEnvVar"),
        })
    }
}

/// The view's `speech` member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeechSettings {
    pub model: String,
    pub voice: String,
    pub response_format: String,
    pub api_base: String,
    pub api_key_env_var: String,
}

impl SpeechSettings {
    /// The active speech entry and its provider. Reference
    /// `config.speech.model`, which raises `KeyError` on a configuration that
    /// resolves no entry.
    ///
    /// # Errors
    ///
    /// A view whose speech member names no model.
    pub fn from_config_view(view: &Value) -> Result<Self, String> {
        let model = string_at(view, "/speech/model/name");
        if model.is_empty() {
            return Err(
                "Narration has no speech model configured; declare a [[tts_models]] entry and \
                 name its alias in `active_tts_model`"
                    .to_owned(),
            );
        }
        Ok(Self {
            model,
            voice: string_at(view, "/speech/model/voice"),
            response_format: string_at(view, "/speech/model/responseFormat"),
            api_base: string_at(view, "/speech/provider/apiBase"),
            api_key_env_var: string_at(view, "/speech/provider/apiKeyEnvVar"),
        })
    }
}

/// Reference `resolve_api_key(env_var) or ""`: the credential a client is built
/// with. A provider that names no variable, or a variable that resolves to
/// nothing, leaves the client with an empty key rather than refusing it; the
/// endpoint answers that request.
#[must_use]
pub fn resolve_credential(env_var: &str, lookup: &CredentialLookup) -> String {
    if env_var.is_empty() {
        return String::new();
    }
    lookup(env_var)
        .filter(|credential| !credential.is_empty())
        .unwrap_or_default()
}

/// Reference `resolve_api_key` for a process that loaded `{vibe_home}/.env`:
/// the environment, the file, then the system keyring.
#[must_use]
pub fn ambient_credentials(vibe_home: &Path) -> CredentialLookup {
    let vibe_home = vibe_home.to_path_buf();
    Arc::new(move |name: &str| {
        DotenvValues::global(&vibe_home)
            .variable(name)
            .filter(|credential| !credential.is_empty())
            .or_else(|| {
                KeyringStore::native()
                    .get_api_key(name)
                    .filter(|credential| !credential.is_empty())
            })
    })
}

/// Whether `env_var` names a variable that resolves to nothing, which is the
/// one case reference `start_recording` refuses before opening anything.
#[must_use]
pub fn credential_missing(env_var: &str, lookup: &CredentialLookup) -> bool {
    !env_var.is_empty() && resolve_credential(env_var, lookup).is_empty()
}

fn string_at(view: &Value, pointer: &str) -> String {
    view.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn unsigned_at(view: &Value, pointer: &str) -> u32 {
    view.pointer(pointer)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_default()
}
