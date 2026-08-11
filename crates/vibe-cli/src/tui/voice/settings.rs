//! What a transcription session is resolved from.
//!
//! The reference builds its transcribe client from the projected configuration
//! and nothing else: the active model entry supplies the model name, the sample
//! rate, the encoding and the streaming delay, the provider entry supplies the
//! endpoint and the name of the variable the credential is read from, and the
//! client resolves all five before it opens anything
//! (`vibe/cli/lazy_audio_managers.py:219-231` and
//! `vibe/cli/transcribe/mistral_transcribe_client.py:36-42`). This module reads
//! the same values off the view this port publishes through `config/read`, so
//! changing `active_transcribe_model` changes the session.

use std::path::Path;

use serde_json::Value;

use vibe_core::config::DotenvValues;

use crate::tui::setup::PersistedCredentialStore;

/// The transcription surface of the published configuration view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TranscriptionSettings {
    pub(super) model: String,
    pub(super) sample_rate: u32,
    pub(super) encoding: String,
    pub(super) target_streaming_delay_ms: u32,
    pub(super) api_base: String,
    pub(super) api_key_env_var: String,
}

impl TranscriptionSettings {
    /// Reads the `transcription` object of a `ConfigView`.
    ///
    /// A view whose active entry resolves to no model name is a configuration
    /// that declares no transcription model at all: the reference raises there
    /// and keeps a null client, so the message is carried to the caller rather
    /// than a default being addressed.
    pub(super) fn from_config_view(view: &Value) -> Result<Self, String> {
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

    /// The credential a session presents, read under the variable the provider
    /// entry names.
    pub(super) fn credential(&self, fallback: &str, vibe_home: &Path) -> Result<String, String> {
        resolve_credential(&self.api_key_env_var, fallback, |name| {
            // The store is opened only where a variable is actually named, so a
            // provider that names none never reaches the keyring.
            DotenvValues::global(vibe_home)
                .variable(name)
                .filter(|credential| !credential.is_empty())
                .or_else(|| {
                    PersistedCredentialStore::new(vibe_core::config::global_env_file(vibe_home))
                        .resolve(name)
                })
        })
    }
}

/// Reference `resolve_api_key` (`vibe/utils/api_keys.py:8-11`): an empty
/// variable name is never looked up, and a name that is declared is read from
/// the environment first and the credential store second.
///
/// Where the reference then hands the client an empty key, this port falls back
/// to the credential the session was started with, so a provider entry that
/// names no variable keeps working on the shipped deployment. A variable that is
/// declared and resolves to nothing is a configuration the operator meant and
/// this build cannot satisfy, so it fails naming the variable rather than
/// presenting another credential.
///
/// `lookup` is injected so the environment and the keyring stay out of a test.
pub(super) fn resolve_credential(
    env_var: &str,
    fallback: &str,
    lookup: impl FnOnce(&str) -> Option<String>,
) -> Result<String, String> {
    if env_var.is_empty() {
        return Ok(fallback.to_owned());
    }
    lookup(env_var)
        .filter(|credential| !credential.is_empty())
        .ok_or_else(|| {
            format!(
                "Voice provider credential `{env_var}` is not set; export it or store it before \
                 starting voice mode"
            )
        })
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
