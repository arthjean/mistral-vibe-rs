//! US-090 and US-091: the configuration as `ConfigView` publishes it.
//!
//! The census the app-server replay reads declares 18 fields, so the assertions
//! here are about which keys exist and what they carry, not about how a client
//! renders them.

use super::registry::default_document;
use super::*;

/// A configuration whose defaults are the shipped document, with `user` written
/// to the selected file.
fn loaded(user: &str) -> (tempfile::TempDir, ConfigSnapshot) {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    if !user.is_empty() {
        fs::write(home.join(CONFIG_FILE), user).expect("user fixture");
    }
    let snapshot = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: temporary.path().join("project"),
        },
        default_document(),
    )
    .load()
    .expect("the configuration loads");
    (temporary, snapshot)
}

#[test]
fn the_view_carries_every_field_the_wire_declares() {
    let (_temporary, snapshot) = loaded("");
    let view = snapshot.config_view();
    let keys = view
        .as_object()
        .expect("the view is an object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            "activeModel",
            "askConfirmationOnExit",
            "autocopyToClipboard",
            "disableWelcomeBannerAnimation",
            "enableNotifications",
            "enableUpdateChecks",
            "fileWatcherForAutocomplete",
            "models",
            "narratorEnabled",
            "showThinkingNodes",
            "speech",
            "theme",
            "transcribeModels",
            "transcription",
            "ttsModels",
            "validationWarnings",
            "vibeCodeEnabled",
            "voiceModeEnabled",
        ],
        "the view is exactly the 18 fields the census declares"
    );
}

#[test]
fn the_active_model_is_the_entry_its_alias_names() {
    let (_temporary, snapshot) = loaded("active_model = \"devstral-small\"\n");
    let view = snapshot.config_view();
    assert_eq!(view["activeModel"]["alias"], "devstral-small");
    assert_eq!(view["activeModel"]["name"], "devstral-small-latest");
    assert_eq!(view["activeModel"]["thinking"], "off");
    assert_eq!(view["activeModel"]["supportsImages"], false);
    // Every configured model is published, so a picker renders from the view
    // rather than from a second call.
    let aliases = view["models"]
        .as_array()
        .expect("models is a list")
        .iter()
        .filter_map(|model| model["alias"].as_str())
        .collect::<Vec<_>>();
    assert!(
        aliases.contains(&"devstral-small") && aliases.contains(&"local"),
        "the shipped models are published: {aliases:?}"
    );
}

/// An `active_model` naming nothing configured already falls back during the
/// load; a view built from a table that still names nothing publishes an empty
/// model rather than failing the response.
#[test]
fn an_unresolvable_active_model_publishes_an_empty_model() {
    let (_temporary, mut snapshot) = loaded("");
    snapshot.effective.insert(
        "active_model".to_owned(),
        Value::String("nothing-configured".to_owned()),
    );
    let view = snapshot.config_view();
    assert_eq!(
        view["activeModel"],
        serde_json::json!({"name": "", "alias": "", "thinking": "off", "supportsImages": false})
    );
}

#[test]
fn the_audio_surfaces_publish_their_active_model_and_its_provider() {
    let (_temporary, snapshot) = loaded("");
    let view = snapshot.config_view();
    assert_eq!(
        view["transcribeModels"],
        serde_json::json!(["voxtral-realtime"])
    );
    assert_eq!(view["ttsModels"], serde_json::json!(["voxtral-tts"]));
    assert_eq!(
        view["transcription"]["model"]["name"],
        "voxtral-mini-transcribe-realtime-2602"
    );
    assert_eq!(view["transcription"]["model"]["sampleRate"], 16_000);
    assert_eq!(view["transcription"]["model"]["encoding"], "pcm_s16le");
    assert_eq!(
        view["transcription"]["model"]["targetStreamingDelayMs"],
        500
    );
    assert_eq!(
        view["transcription"]["provider"]["apiBase"],
        "wss://api.mistral.ai"
    );
    assert_eq!(
        view["transcription"]["provider"]["apiKeyEnvVar"],
        "MISTRAL_API_KEY"
    );
    assert_eq!(view["transcription"]["provider"]["client"], "mistral");
    assert_eq!(view["speech"]["model"]["name"], "voxtral-mini-tts-latest");
    assert_eq!(view["speech"]["model"]["voice"], "gb_jane_neutral");
    assert_eq!(view["speech"]["model"]["responseFormat"], "wav");
    assert_eq!(
        view["speech"]["provider"]["apiBase"],
        "https://api.mistral.ai"
    );
}

#[test]
fn the_toggles_come_from_the_effective_table() {
    let (_temporary, snapshot) = loaded(
        "theme = \"nord\"\nvoice_mode_enabled = true\nask_confirmation_on_exit = false\nvibe_code_enabled = true\n",
    );
    let view = snapshot.config_view();
    assert_eq!(view["theme"], "nord");
    assert_eq!(view["voiceModeEnabled"], true);
    assert_eq!(view["askConfirmationOnExit"], false);
    assert_eq!(view["vibeCodeEnabled"], true);
    // Unset in the fixture, so the view publishes the shipped default rather
    // than omitting the field.
    assert_eq!(view["showThinkingNodes"], false);
}

#[test]
fn the_active_provider_is_the_one_the_active_model_names() {
    let (_temporary, snapshot) = loaded("active_model = \"local\"\n");
    let provider = snapshot.active_provider().expect("the provider resolves");
    assert_eq!(
        provider.get("name").and_then(Value::as_str),
        Some("llamacpp")
    );
    let (_temporary, mistral) = loaded("");
    assert_eq!(
        mistral.active_provider().and_then(|provider| provider
            .get("backend")
            .and_then(Value::as_str)
            .map(str::to_owned)),
        Some("mistral".to_owned()),
        "the shipped active model is served by the Mistral backend"
    );
}

/// US-148: the five compaction keys, read as one policy.
#[test]
fn the_compaction_settings_carry_the_five_declared_keys() {
    let (_temporary, shipped) = loaded("");
    let settings = shipped.compaction_settings();
    assert_eq!(
        settings.auto_compact_threshold, 200_000,
        "`auto_compact_threshold` reaches the settings from the active model"
    );
    assert_eq!(
        settings.compaction_prompt_id, "compact",
        "`compaction_prompt_id` defaults to the built-in request"
    );
    assert!(
        !settings.context_warnings,
        "`context_warnings` ships disabled"
    );
    assert!(
        !settings.raise_on_compaction_failure,
        "`raise_on_compaction_failure` ships disabled"
    );

    let (_temporary, configured) = loaded(concat!(
        "auto_compact_threshold = 40000\n",
        "compaction_prompt_id = \"terse\"\n",
        "context_warnings = true\n",
        "raise_on_compaction_failure = true\n",
        "active_model = \"tuned\"\n",
        "[[models]]\n",
        "name = \"tuned-model\"\n",
        "provider = \"mistral\"\n",
        "alias = \"tuned\"\n",
    ));
    let changed = configured.compaction_settings();
    assert_eq!(
        changed.auto_compact_threshold, 40_000,
        "`auto_compact_threshold` changes the threshold policies read"
    );
    assert_eq!(
        changed.compaction_prompt_id, "terse",
        "`compaction_prompt_id` changes the prompt identifier the request resolves"
    );
    assert!(
        changed.context_warnings,
        "`context_warnings` changes whether the conversation is warned"
    );
    assert!(
        changed.raise_on_compaction_failure,
        "`raise_on_compaction_failure` changes whether a failure is fatal"
    );
}

/// US-148: `compaction_model` names the model a summarization is sent to, and
/// an unset one resolves to the active model, as `get_compaction_model` does.
#[test]
fn the_compaction_model_falls_back_to_the_active_model() {
    let (_temporary, shipped) = loaded("");
    assert_eq!(
        shipped.compaction_settings().compaction_model.as_deref(),
        Some("mistral-vibe-cli-latest"),
        "an unset `compaction_model` resolves to the active model"
    );

    let (_temporary, configured) = loaded(concat!(
        "[compaction_model]\n",
        "name = \"devstral-small-latest\"\n",
        "provider = \"mistral\"\n",
        "alias = \"cheap\"\n",
    ));
    assert_eq!(
        configured.compaction_settings().compaction_model.as_deref(),
        Some("devstral-small-latest"),
        "`compaction_model` changes the model a summarization is sent to"
    );
}

/// US-148: a model that declares no threshold of its own inherits the global
/// one, which is what the load's propagation already writes.
#[test]
fn an_undeclared_model_threshold_falls_back_to_the_global_one() {
    let (_temporary, snapshot) = loaded(concat!(
        "auto_compact_threshold = 90000\n",
        "active_model = \"tiny\"\n",
        "[[models]]\n",
        "name = \"tiny-model\"\n",
        "provider = \"mistral\"\n",
        "alias = \"tiny\"\n",
    ));
    assert_eq!(
        snapshot.compaction_settings().auto_compact_threshold,
        90_000
    );
}
