//! The machine's edges: the corpus replay drives the full screen graph, so
//! these tests hold the behaviors the corpus does not script, chiefly the
//! Ctrl+C paths, the URL help, the copy action, stale attempt events, and
//! the exit-path map.

use std::path::Path;

use toml::{Table, Value};
use vibe_core::auth::{PersistOutcome, SignInStatus};

use super::context::{OnboardingContext, default_mistral_provider};
use super::model::{
    KeyPress, ModelEffect, ModelEvent, OnboardingModel, OnboardingOutcome, OnboardingPorts,
    SignInVariant,
};
use super::{ExitPlan, exit_plan};

/// Records every persistence call and answers with a scripted outcome.
#[derive(Default)]
struct RecorderPorts {
    persist_calls: Vec<(String, String, bool)>,
    provider_writes: usize,
    persist_outcome: Option<PersistOutcome>,
    provider_write_ok: Option<bool>,
}

impl OnboardingPorts for RecorderPorts {
    fn persist_api_key(
        &mut self,
        env_key: &str,
        _provider: &Table,
        api_key: &str,
        custom_domain: bool,
    ) -> PersistOutcome {
        self.persist_calls
            .push((env_key.to_owned(), api_key.to_owned(), custom_domain));
        self.persist_outcome
            .clone()
            .unwrap_or(PersistOutcome::Completed)
    }

    fn persist_provider(&mut self, _provider: &Table) -> bool {
        self.provider_writes += 1;
        self.provider_write_ok.unwrap_or(true)
    }
}

fn browser_context() -> OnboardingContext {
    OnboardingContext {
        provider: default_mistral_provider(),
        vibe_base_url: "https://chat.mistral.ai".to_owned(),
        theme: "auto".to_owned(),
    }
}

fn manual_context() -> OnboardingContext {
    let mut provider = default_mistral_provider();
    provider.insert(
        "browser_auth_base_url".to_owned(),
        Value::String(String::new()),
    );
    OnboardingContext {
        provider,
        vibe_base_url: "https://chat.mistral.ai".to_owned(),
        theme: "auto".to_owned(),
    }
}

fn key(
    model: &mut OnboardingModel,
    ports: &mut RecorderPorts,
    press: KeyPress,
) -> Vec<ModelEffect> {
    model.handle(ModelEvent::Key(press), ports)
}

/// Walks a browser-capable model onto the browser sign-in screen with one
/// running attempt.
fn onto_browser_sign_in(ports: &mut RecorderPorts) -> OnboardingModel {
    let mut model = OnboardingModel::new(browser_context());
    model.handle(ModelEvent::WelcomeTypingFinished, ports);
    key(&mut model, ports, KeyPress::Enter);
    key(&mut model, ports, KeyPress::Enter);
    key(&mut model, ports, KeyPress::Enter);
    let effects = key(&mut model, ports, KeyPress::Enter);
    assert_eq!(effects, vec![ModelEffect::StartSignIn { attempt: 1 }]);
    model
}

#[test]
fn ctrl_c_cancels_from_every_cancel_screen_and_backs_out_nowhere() {
    for presses in [0usize, 1, 2] {
        let mut ports = RecorderPorts::default();
        let mut model = OnboardingModel::new(browser_context());
        model.handle(ModelEvent::WelcomeTypingFinished, &mut ports);
        for _ in 0..presses {
            key(&mut model, &mut ports, KeyPress::Enter);
        }
        let effects = key(&mut model, &mut ports, KeyPress::CtrlC);
        assert_eq!(
            effects,
            vec![ModelEffect::Exit(OnboardingOutcome::Cancelled)],
            "Ctrl+C after {presses} advances"
        );
        assert!(ports.persist_calls.is_empty());
    }
}

#[test]
fn ctrl_c_on_the_target_screen_cancels_where_escape_backs_out() {
    let mut ports = RecorderPorts::default();
    let mut model = OnboardingModel::new(browser_context());
    model.handle(ModelEvent::WelcomeTypingFinished, &mut ports);
    key(&mut model, &mut ports, KeyPress::Enter);
    key(&mut model, &mut ports, KeyPress::Enter);
    key(&mut model, &mut ports, KeyPress::Enter);
    assert_eq!(model.current_screen().name(), "sign_in_target");
    let effects = key(&mut model, &mut ports, KeyPress::CtrlC);
    assert_eq!(
        effects,
        vec![ModelEffect::Exit(OnboardingOutcome::Cancelled)]
    );
}

#[test]
fn the_url_help_appears_only_after_the_delay_and_for_the_active_attempt() {
    let mut ports = RecorderPorts::default();
    let mut model = onto_browser_sign_in(&mut ports);
    let effects = model.handle(
        ModelEvent::SignInStarted {
            attempt: 1,
            sign_in_url: "https://console.mistral.ai/sign-in/p1".to_owned(),
        },
        &mut ports,
    );
    assert_eq!(effects, vec![ModelEffect::ScheduleUrlHelp { attempt: 1 }]);
    assert!(!model.sign_in_view().show_url_help);
    // A stale attempt's timer reveals nothing.
    model.handle(ModelEvent::UrlHelpElapsed { attempt: 7 }, &mut ports);
    assert!(!model.sign_in_view().show_url_help);
    model.handle(ModelEvent::UrlHelpElapsed { attempt: 1 }, &mut ports);
    assert!(model.sign_in_view().show_url_help);
}

#[test]
fn the_copy_action_reveals_the_url_and_the_url_stays_retrievable() {
    let mut ports = RecorderPorts::default();
    let mut model = onto_browser_sign_in(&mut ports);
    model.handle(
        ModelEvent::SignInStarted {
            attempt: 1,
            sign_in_url: "https://console.mistral.ai/sign-in/p1".to_owned(),
        },
        &mut ports,
    );
    let effects = key(&mut model, &mut ports, KeyPress::Char('c'));
    assert_eq!(
        effects,
        vec![ModelEffect::CopyUrl {
            url: "https://console.mistral.ai/sign-in/p1".to_owned()
        }]
    );
    assert!(model.sign_in_view().reveal_url);
    assert_eq!(
        model.sign_in_view().sign_in_url.as_deref(),
        Some("https://console.mistral.ai/sign-in/p1")
    );
}

#[test]
fn events_from_a_previous_attempt_are_ignored_after_a_retry() {
    let mut ports = RecorderPorts::default();
    let mut model = onto_browser_sign_in(&mut ports);
    model.handle(
        ModelEvent::SignInFailed {
            attempt: 1,
            code: None,
            message: "scripted".to_owned(),
        },
        &mut ports,
    );
    assert_eq!(model.sign_in_view().variant, SignInVariant::Error);
    let effects = key(&mut model, &mut ports, KeyPress::Char('r'));
    assert_eq!(effects, vec![ModelEffect::StartSignIn { attempt: 2 }]);
    // The first attempt's late completion must not persist anything.
    let effects = model.handle(
        ModelEvent::SignInCompleted {
            attempt: 1,
            api_key: "stale-key".to_owned(),
        },
        &mut ports,
    );
    assert!(effects.is_empty());
    assert!(ports.persist_calls.is_empty());
    // The active attempt's status still lands.
    model.handle(
        ModelEvent::SignInStatus {
            attempt: 2,
            status: SignInStatus::WaitingForBrowserSignIn,
        },
        &mut ports,
    );
    assert_eq!(model.sign_in_view().step, 1);
}

#[test]
fn a_success_holds_until_the_delay_elapses_and_manual_and_cancel_turn_inert() {
    let mut ports = RecorderPorts::default();
    let mut model = onto_browser_sign_in(&mut ports);
    let effects = model.handle(
        ModelEvent::SignInCompleted {
            attempt: 1,
            api_key: "signed-in-key".to_owned(),
        },
        &mut ports,
    );
    assert_eq!(effects, vec![ModelEffect::ScheduleSuccessExit]);
    assert_eq!(model.sign_in_view().variant, SignInVariant::Success);
    assert!(key(&mut model, &mut ports, KeyPress::Char('m')).is_empty());
    assert!(key(&mut model, &mut ports, KeyPress::Escape).is_empty());
    let effects = model.handle(ModelEvent::SuccessDelayElapsed, &mut ports);
    assert_eq!(
        effects,
        vec![ModelEffect::Exit(OnboardingOutcome::Completed)]
    );
    assert_eq!(
        ports.persist_calls,
        vec![(
            "MISTRAL_API_KEY".to_owned(),
            "signed-in-key".to_owned(),
            false
        )]
    );
}

#[test]
fn a_failed_provider_write_terminates_with_the_provider_config_error() {
    let mut ports = RecorderPorts {
        provider_write_ok: Some(false),
        ..RecorderPorts::default()
    };
    let mut model = OnboardingModel::new(browser_context());
    model.handle(ModelEvent::WelcomeTypingFinished, &mut ports);
    key(&mut model, &mut ports, KeyPress::Enter);
    key(&mut model, &mut ports, KeyPress::Enter);
    key(&mut model, &mut ports, KeyPress::Enter);
    key(&mut model, &mut ports, KeyPress::Down);
    key(&mut model, &mut ports, KeyPress::Enter);
    for character in "console.internal.example".chars() {
        key(&mut model, &mut ports, KeyPress::Char(character));
    }
    key(&mut model, &mut ports, KeyPress::Enter);
    let effects = model.handle(
        ModelEvent::SignInCompleted {
            attempt: 1,
            api_key: "signed-in-key".to_owned(),
        },
        &mut ports,
    );
    assert_eq!(ports.provider_writes, 1);
    let Some(ModelEffect::Exit(OnboardingOutcome::ProviderConfigError { .. })) = effects.first()
    else {
        panic!("a failed provider write terminates immediately, got {effects:?}");
    };
}

#[test]
fn the_manual_screen_keeps_the_key_masked_and_out_of_debug_output() {
    let mut ports = RecorderPorts::default();
    let mut model = OnboardingModel::new(manual_context());
    model.handle(ModelEvent::WelcomeTypingFinished, &mut ports);
    key(&mut model, &mut ports, KeyPress::Enter);
    key(&mut model, &mut ports, KeyPress::Enter);
    assert_eq!(model.current_screen().name(), "api_key");
    for character in "secret-key".chars() {
        key(&mut model, &mut ports, KeyPress::Char(character));
    }
    assert!(model.key_masked());
    assert_eq!(model.key_value_len(), "secret-key".len());
    assert_eq!(super::model::masked(3), "\u{2022}\u{2022}\u{2022}");
}

#[test]
fn the_exit_plan_maps_the_five_values_onto_the_reference_exit_paths() {
    let env_file = Path::new("/home/operator/.vibe/.env");
    let plans: Vec<(OnboardingOutcome, Option<u8>, bool)> = vec![
        (OnboardingOutcome::Cancelled, Some(0), false),
        (
            OnboardingOutcome::EnvVarError {
                detail: "BAD KEY".to_owned(),
            },
            Some(1),
            false,
        ),
        (
            OnboardingOutcome::SaveError {
                detail: "disk full".to_owned(),
            },
            None,
            true,
        ),
        (
            OnboardingOutcome::ProviderConfigError {
                detail: "write failed".to_owned(),
            },
            None,
            true,
        ),
        (OnboardingOutcome::Completed, None, true),
    ];
    for (outcome, exit_code, persist_theme) in plans {
        let ExitPlan {
            exit_code: planned_code,
            message,
            persist_theme: planned_theme,
        } = exit_plan(&outcome, env_file);
        assert_eq!(planned_code, exit_code, "{outcome:?}");
        assert_eq!(planned_theme, persist_theme, "{outcome:?}");
        assert!(!message.is_empty());
    }
    let plan = exit_plan(
        &OnboardingOutcome::SaveError {
            detail: "disk full".to_owned(),
        },
        env_file,
    );
    assert!(plan.message.contains("/home/operator/.vibe/.env"));
}

#[test]
fn the_retired_setup_steps_stay_reachable_from_their_existing_commands() {
    use crate::tui::commands::{COMMANDS, CommandId};
    // The onboarding never asks for a proxy, a certificate path or a model;
    // each stays reachable where it already lives.
    assert!(
        COMMANDS
            .iter()
            .any(|command| command.id == CommandId::ProxySetup),
        "/proxy-setup keeps the network settings reachable"
    );
    assert!(
        COMMANDS
            .iter()
            .any(|command| command.id == CommandId::Model),
        "/model keeps the model selection reachable"
    );
    for field in ["proxy", "tls_ca_path", "active_model"] {
        assert!(
            vibe_core::config::registry::field(field).is_some(),
            "the configuration still declares {field}"
        );
    }
}

#[test]
fn the_theme_carried_out_survives_wrapping_navigation() {
    let mut ports = RecorderPorts::default();
    let mut model = OnboardingModel::new(browser_context());
    model.handle(ModelEvent::WelcomeTypingFinished, &mut ports);
    key(&mut model, &mut ports, KeyPress::Enter);
    assert_eq!(model.selected_theme(), "auto");
    key(&mut model, &mut ports, KeyPress::Up);
    assert_eq!(
        model.selected_theme(),
        *model.themes().last().expect("catalog is non-empty"),
        "navigating up from the first entry wraps to the last"
    );
    key(&mut model, &mut ports, KeyPress::Down);
    assert_eq!(model.selected_theme(), "auto");
}
