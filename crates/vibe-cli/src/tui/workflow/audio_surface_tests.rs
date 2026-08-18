//! US-214: the two audio model choices the voice settings publish.
//!
//! The reference promotes `active_transcribe_model` and `active_tts_model` from
//! plain strings to choice lists when its settings screen renders them, and
//! builds each list from the aliases the projection publishes
//! (`vibe/cli/textual_ui/screens/config/config_screen.py:110-111`). These tests
//! drive the same surface through the overlay a `/voice` command opens, against
//! an app-server whose configuration lives under a temporary root.

use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use tempfile::TempDir;

use vibe_app_server::release3::{Release3Paths, Release3Service};
use vibe_app_server::server::AppServer;

use super::super::chat_input::ChatInputState;
use super::super::controls::ControlState;
use super::super::interaction::{ConfigLayerTarget, OverlayKind};
use super::super::runtime::{
    interactive_test_runtime_with_server, interactive_test_runtime_with_trust,
};
use super::super::setup::{ResolvedTheme, Theme};
use super::super::state::TuiState;
use super::config::configured_value;
use super::{InteractiveRuntime, overlay::select_overlay_item, show_voice, show_voice_model};

/// Two aliases per family beside the shipped default entry the union merge
/// keeps, so a choice list is a choice rather than a single value.
const TWO_PER_FAMILY: &str = r#"
active_transcribe_model = "studio"
active_tts_model = "narrator"

[[transcribe_models]]
alias = "studio"
name = "studio-transcribe"
provider = "mistral"

[[transcribe_models]]
alias = "field"
name = "field-transcribe"
provider = "mistral"

[[tts_models]]
alias = "narrator"
name = "narrator-speech"
provider = "mistral"

[[tts_models]]
alias = "announcer"
name = "announcer-speech"
provider = "mistral"
"#;

fn theme() -> ResolvedTheme {
    ResolvedTheme {
        theme: Theme::Dark,
        colors_enabled: false,
    }
}

/// A runtime whose configuration is a file this test wrote, so a write lands in
/// the temporary root rather than in the operator's own home.
fn runtime_over(document: &str, trusted: bool) -> (TempDir, InteractiveRuntime) {
    let temporary = tempfile::tempdir().expect("a temporary vibe home");
    let vibe_home = temporary.path().join("vibe-home");
    fs::create_dir_all(&vibe_home).expect("the vibe home is created");
    fs::write(vibe_home.join("config.toml"), document).expect("the seed configuration is written");
    let service = Release3Service::new(
        Release3Paths {
            session_root: vibe_home.join("sessions"),
            working_directory: temporary.path().join("workspace"),
            vibe_home,
        },
        trusted,
    )
    .expect("the configuration service builds");
    let server = AppServer::with_release3_service(service);
    let runtime = if trusted {
        interactive_test_runtime_with_server("audio-surface", server)
    } else {
        interactive_test_runtime_with_trust("audio-surface", server, false)
    };
    (temporary, runtime)
}

fn overlay_ids(state: &TuiState) -> Vec<String> {
    state
        .overlay
        .as_ref()
        .expect("an overlay is open")
        .items
        .iter()
        .map(|item| item.id.clone())
        .collect()
}

fn item_description(state: &TuiState, id: &str) -> String {
    state
        .overlay
        .as_ref()
        .expect("an overlay is open")
        .items
        .iter()
        .find(|item| item.id == id)
        .unwrap_or_else(|| panic!("`{id}` is offered"))
        .description
        .clone()
}

/// Moves the highlight the way an operator does, since the selection is the
/// overlay's own state rather than an index a caller sets.
fn select(state: &mut TuiState, id: &str) {
    let overlay = state.overlay.as_mut().expect("an overlay is open");
    for _ in 0..=overlay.items.len() {
        if overlay.selected_item().is_some_and(|item| item.id == id) {
            return;
        }
        overlay.move_selection(1);
    }
    panic!("`{id}` was never highlighted");
}

async fn confirm(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    let mut controls = ControlState::new("audio-surface");
    let mut composer = ChatInputState::default();
    let mut theme = theme();
    let mut holder = Some(std::mem::replace(runtime, placeholder()));
    select_overlay_item(&mut holder, state, &mut controls, &mut composer, &mut theme).await;
    *runtime = holder.expect("the runtime survives the selection");
}

/// `select_overlay_item` takes ownership of the runtime slot, so the caller
/// hands it the real one and holds a throwaway in its place for the duration.
fn placeholder() -> InteractiveRuntime {
    interactive_test_runtime_with_server("audio-surface-placeholder", AppServer::default())
}

/// The two entries are offered with the aliases the view publishes behind them.
#[tokio::test(flavor = "multi_thread")]
async fn the_voice_settings_offer_both_audio_model_choices() {
    let (_temporary, mut runtime) = runtime_over(TWO_PER_FAMILY, true);
    let mut state = TuiState::new("audio-surface");

    show_voice(&mut runtime, &mut state);

    let ids = overlay_ids(&state);
    assert!(
        ids.contains(&"active_transcribe_model".to_owned())
            && ids.contains(&"active_tts_model".to_owned()),
        "both audio model entries are offered: {ids:?}"
    );
    assert_eq!(
        item_description(&state, "active_transcribe_model"),
        "studio · 3 declared"
    );
    assert_eq!(
        item_description(&state, "active_tts_model"),
        "narrator · 3 declared"
    );

    show_voice_model(&mut runtime, &mut state, "active_transcribe_model");
    assert_eq!(
        state.overlay.as_ref().map(|overlay| overlay.kind),
        Some(OverlayKind::VoiceModel)
    );
    assert_eq!(
        overlay_ids(&state),
        [
            "active_transcribe_model:field",
            "active_transcribe_model:studio",
            "active_transcribe_model:voxtral-realtime"
        ],
        "the choice list is the published alias list, defaults included"
    );
    assert_eq!(
        item_description(&state, "active_transcribe_model:studio"),
        "current"
    );

    show_voice_model(&mut runtime, &mut state, "active_tts_model");
    assert_eq!(
        overlay_ids(&state),
        [
            "active_tts_model:announcer",
            "active_tts_model:narrator",
            "active_tts_model:voxtral-tts"
        ],
        "the speech family is offered the same way"
    );
}

/// A confirmed choice is written through the configuration path every other
/// choice overlay writes through, and the audio surfaces are resolved again
/// from it, so the next session addresses the model that was chosen.
#[tokio::test(flavor = "multi_thread")]
async fn a_confirmed_choice_is_persisted_and_resolves_the_next_session() {
    let (temporary, mut runtime) = runtime_over(TWO_PER_FAMILY, true);
    let mut state = TuiState::new("audio-surface");

    show_voice_model(&mut runtime, &mut state, "active_transcribe_model");
    select(&mut state, "active_transcribe_model:field");
    confirm(&mut runtime, &mut state).await;

    assert_eq!(
        configured_value(&mut runtime, "active_transcribe_model"),
        Some(json!("field")),
        "the write landed in the configuration the session reads"
    );
    assert_eq!(
        resolved_transcription_model(&mut runtime),
        "field-transcribe",
        "the resolved session follows the alias that was chosen"
    );
    let persisted = fs::read_to_string(temporary.path().join("vibe-home").join("config.toml"))
        .expect("the configuration file is readable");
    assert!(
        persisted.contains("active_transcribe_model = \"field\""),
        "the value reached the file rather than only the process: {persisted}"
    );
    assert_eq!(
        state.overlay.as_ref().map(|overlay| overlay.kind),
        Some(OverlayKind::Voice),
        "the settings reopen on the value that landed"
    );
    assert_eq!(
        item_description(&state, "active_transcribe_model"),
        "field · 3 declared"
    );

    show_voice_model(&mut runtime, &mut state, "active_tts_model");
    select(&mut state, "active_tts_model:announcer");
    confirm(&mut runtime, &mut state).await;
    assert_eq!(
        configured_value(&mut runtime, "active_tts_model"),
        Some(json!("announcer"))
    );
    assert_eq!(
        resolved_speech_model(&mut runtime),
        "announcer-speech",
        "the narrator reads the entry the chosen alias names"
    );
}

/// A family declaring one entry still shows it: the reference offers a
/// one-option list rather than hiding the field.
#[tokio::test(flavor = "multi_thread")]
async fn a_family_with_one_entry_is_still_offered() {
    let (_temporary, mut runtime) = runtime_over("", true);
    let mut state = TuiState::new("audio-surface");

    show_voice(&mut runtime, &mut state);
    let description = item_description(&state, "active_transcribe_model");
    assert!(
        description.ends_with("1 declared"),
        "the shipped configuration declares exactly one transcription model: {description}"
    );

    show_voice_model(&mut runtime, &mut state, "active_transcribe_model");
    assert_eq!(
        overlay_ids(&state).len(),
        1,
        "the single value is offered rather than the entry being hidden"
    );
}

/// A write that cannot land leaves the previous value selected and reports why,
/// rather than showing a value the configuration never took.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_write_keeps_the_previous_value_and_reports_it() {
    let (_temporary, mut runtime) = runtime_over(TWO_PER_FAMILY, false);
    // The project file is the write target and the workspace is untrusted, so
    // the write is refused inside the transaction that would have applied it.
    runtime.config_target = Some(ConfigLayerTarget::Project);
    let mut state = TuiState::new("audio-surface");

    show_voice_model(&mut runtime, &mut state, "active_transcribe_model");
    select(&mut state, "active_transcribe_model:field");
    confirm(&mut runtime, &mut state).await;

    assert_eq!(
        configured_value(&mut runtime, "active_transcribe_model"),
        Some(json!("studio")),
        "the refused write left the configuration alone"
    );
    assert_eq!(
        item_description(&state, "active_transcribe_model"),
        "studio · 3 declared",
        "the reopened settings show the value that is actually configured"
    );
    let diagnostics = state.diagnostics().map(str::to_owned).collect::<Vec<_>>();
    assert!(
        diagnostics
            .iter()
            .any(|message| message.to_lowercase().contains("trust")),
        "the operator is told why the write did not land: {diagnostics:?}"
    );
}

fn resolved_transcription_model(runtime: &mut InteractiveRuntime) -> String {
    view_string(runtime, "/transcription/model/name")
}

fn resolved_speech_model(runtime: &mut InteractiveRuntime) -> String {
    view_string(runtime, "/speech/model/name")
}

fn view_string(runtime: &mut InteractiveRuntime, pointer: &str) -> String {
    runtime
        .published_config()
        .as_ref()
        .and_then(|view| view.pointer(pointer))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The seed document is written under a directory this test owns, so no
/// assertion here can depend on the operator's own configuration.
#[test]
fn the_fixture_configuration_is_written_under_a_temporary_root() {
    let (temporary, _runtime) = runtime_over(TWO_PER_FAMILY, true);
    let seeded = temporary.path().join("vibe-home").join("config.toml");
    assert!(seeded.exists());
    assert!(Path::new(&seeded).starts_with(temporary.path()));
}
