use serde_json::{Value, json};

use super::super::chat_input::ChatInputState;
use super::super::controls::ControlState;
use super::super::interaction::{ConfigLayerTarget, OverlayAction, OverlayKind};
use super::super::pickers::{
    VOICE_MODEL_FIELDS, config_choice_overlay, config_target_overlay,
    remote_project_create_overlay, theme_overlay, thinking_overlay,
};
use super::super::setup::ResolvedTheme;
use super::super::state::TuiState;
use super::super::switching::{self, SwitchRequest};
use super::super::{InteractiveRuntime, call_runtime, persist_user_setting, update_theme};
use super::config::{
    apply_thinking, config_field_schema, configured_value, persist_config_setting,
    selected_config_target,
};
use super::mcp::{McpEffect, reduce_auth_action};
use super::{
    OverlayEffect, resume_selected_session, show_config, show_model, show_voice, show_voice_model,
    sync_voice_preference,
};

pub(super) async fn select_overlay_item(
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
    composer: &mut ChatInputState,
    theme: &mut ResolvedTheme,
) -> Option<OverlayEffect> {
    let (kind, item) = state.overlay.as_ref().and_then(|overlay| {
        overlay
            .selected_item()
            .cloned()
            .map(|item| (overlay.kind, item))
    })?;
    let Some(runtime) = runtime.as_mut() else {
        state.overlay = None;
        return None;
    };
    match kind {
        OverlayKind::Config => match item.id.as_str() {
            "config-target" => {
                let current = runtime.config_target.unwrap_or_else(|| {
                    ConfigLayerTarget::from_selected_target(
                        selected_config_target(runtime).as_deref(),
                    )
                });
                state.overlay = Some(config_target_overlay(current));
            }
            "active_model" => {
                show_model(runtime, state);
                mark_config_special(state, "active_model");
            }
            "thinking" => {
                state.overlay = Some(thinking_overlay(&runtime.thinking));
                mark_config_special(state, "thinking");
            }
            "theme" => {
                let current = configured_value(runtime, "theme")
                    .and_then(|value| value.as_str().map(ToOwned::to_owned))
                    .unwrap_or_else(|| "system".to_owned());
                state.overlay = Some(theme_overlay(&current));
                mark_config_special(state, "theme");
            }
            key => {
                let Some(schema) = config_field_schema(runtime, key, state) else {
                    state.push_diagnostic("Configuration field schema is unavailable");
                    return None;
                };
                let field_types = schema
                    .get("type")
                    .map(|kind| match kind {
                        Value::String(kind) => vec![kind.as_str()],
                        Value::Array(kinds) => kinds.iter().filter_map(Value::as_str).collect(),
                        _ => Vec::new(),
                    })
                    .unwrap_or_default();
                if schema.get("enum").and_then(Value::as_array).is_some() {
                    let current = configured_value(runtime, key);
                    state.overlay = Some(config_choice_overlay(key, &schema, current.as_ref()));
                } else if field_types.contains(&"boolean") {
                    let current = configured_value(runtime, key)
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    let path = key.split('.').collect::<Vec<_>>();
                    let target =
                        selected_config_target(runtime).unwrap_or_else(|| "user".to_owned());
                    if persist_config_setting(
                        runtime,
                        &target,
                        &path,
                        json!(!current),
                        false,
                        state,
                    ) {
                        show_config(runtime, state);
                    }
                } else {
                    let target =
                        selected_config_target(runtime).unwrap_or_else(|| "user".to_owned());
                    composer.replace_text(format!("/config set --target {target} {key} "));
                    state.overlay = None;
                }
            }
        },
        OverlayKind::ConfigChoice => {
            let OverlayAction::ConfigChoice { path, value } = item.action else {
                state.push_diagnostic("Configuration choice is malformed");
                return None;
            };
            let target = selected_config_target(runtime).unwrap_or_else(|| "user".to_owned());
            let segments = path.split('.').collect::<Vec<_>>();
            if persist_config_setting(runtime, &target, &segments, value, false, state) {
                show_config(runtime, state);
            }
        }
        OverlayKind::ConfigTarget => {
            let OverlayAction::ConfigTarget(target) = item.action else {
                state.push_diagnostic("Configuration layer choice is malformed");
                return None;
            };
            runtime.config_target = Some(target);
            show_config(runtime, state);
        }
        OverlayKind::Model => {
            let (model, target) = config_special_value(runtime, &item.action, "active_model")
                .map_or((item.id, None), |(value, target)| (value, Some(target)));
            switching::request(
                runtime,
                composer,
                state,
                SwitchRequest::Model { model, target },
            );
            state.overlay = None;
        }
        OverlayKind::Thinking => {
            let (value, target) = config_special_value(runtime, &item.action, "thinking")
                .map_or((item.id, None), |(value, target)| (value, Some(target)));
            apply_thinking(runtime, &value, target.as_deref(), state);
            state.overlay = None;
        }
        OverlayKind::Theme => {
            let (value, target) = config_special_value(runtime, &item.action, "theme")
                .map_or((item.id, None), |(value, target)| (value, Some(target)));
            update_theme(runtime, &value, target.as_deref(), state, theme);
            state.overlay = None;
        }
        OverlayKind::Sessions => {
            resume_selected_session(runtime, state, controls, &item.id);
        }
        OverlayKind::Mcp | OverlayKind::Connectors => {
            let OverlayAction::Integration(target) = item.action else {
                state.push_diagnostic("Integration selection is malformed");
                return None;
            };
            if target.requires_auth {
                return Some(OverlayEffect::Mcp(McpEffect::BeginAuth {
                    kind: target.kind,
                    source: target.source,
                    enable_on_complete: false,
                }));
            }
            return Some(OverlayEffect::Mcp(McpEffect::ShowDetail { target }));
        }
        OverlayKind::McpDetail => {
            let OverlayAction::Integration(target) = item.action else {
                return None;
            };
            if target.requires_setup {
                return Some(OverlayEffect::Mcp(McpEffect::Refresh {
                    kind: target.kind,
                    source: target.source,
                }));
            }
        }
        OverlayKind::McpAuth => {
            let OverlayAction::Authenticate(action) = item.action else {
                state.push_diagnostic("Authentication action is malformed");
                return None;
            };
            return Some(OverlayEffect::Mcp(reduce_auth_action(&action)));
        }
        OverlayKind::Voice => {
            // The two active-model fields open their own choice list; every
            // other entry of this overlay is a toggle.
            if VOICE_MODEL_FIELDS
                .into_iter()
                .any(|(field, _, _)| field == item.id)
            {
                show_voice_model(runtime, state, &item.id);
                return None;
            }
            let current = configured_value(runtime, &item.id)
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if persist_user_setting(runtime, &[&item.id], json!(!current), false, state) {
                sync_voice_preference(runtime, composer);
                // Reference `_handle_voice_settings_closed`: the narrator is
                // synced where the voice settings are saved, so toggling
                // `narrator_enabled` here rebuilds the speech client and moves
                // the gate on the next turn rather than at the next start.
                super::config::apply_render_preferences(runtime, state);
                show_voice(runtime, state);
            }
        }
        OverlayKind::VoiceModel => {
            let OverlayAction::ConfigChoice { path, value } = item.action else {
                state.push_diagnostic("Audio model choice is malformed");
                return None;
            };
            let target = selected_config_target(runtime).unwrap_or_else(|| "user".to_owned());
            if persist_config_setting(runtime, &target, &[path.as_str()], value, false, state) {
                // Both audio surfaces are resolved again where the write
                // landed: `sync_voice_preference` rebuilds the transcription
                // session and `apply_render_preferences` the speech client, so
                // the next recording and the next narrated turn address the
                // model that was just chosen.
                sync_voice_preference(runtime, composer);
                super::config::apply_render_preferences(runtime, state);
            }
            // A write that failed already reported why; reopening the settings
            // from the configuration as it stands leaves the previous value
            // selected rather than showing one that never landed.
            show_voice(runtime, state);
        }
        OverlayKind::Proxy => {
            let current =
                call_runtime(runtime, "config/proxy/read", json!({}), state).and_then(|result| {
                    result
                        .get("settings")
                        .and_then(|settings| settings.get("values"))
                        .and_then(|values| values.get(&item.id))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                });
            let value = current
                .as_deref()
                .and_then(|value| shlex::try_quote(value).ok())
                .map(|value| format!(" {value}"))
                .unwrap_or_default();
            composer.replace_text(format!("/proxy-setup {}{value}", item.id));
            state.overlay = None;
        }
        OverlayKind::RemoteProjects => {
            let OverlayAction::RemoteProject(action) = item.action else {
                state.push_diagnostic("Remote project action is malformed");
                return None;
            };
            if let super::super::interaction::RemoteProjectAction::Create {
                name,
                default_branch,
            } = action
            {
                let draft = super::super::interaction::RemoteProjectDraft {
                    name,
                    default_branch,
                };
                state.overlay = Some(remote_project_create_overlay(&draft));
                runtime.remote_project_draft = Some(draft);
                return None;
            }
            return Some(OverlayEffect::RemoteProject(action));
        }
        OverlayKind::RemoteProjectCreate => {
            let OverlayAction::RemoteProject(action) = item.action else {
                return None;
            };
            return Some(OverlayEffect::RemoteProject(action));
        }
        OverlayKind::TeleportApproval => {
            let OverlayAction::TeleportPush(action) = item.action else {
                state.push_diagnostic("Teleport approval action is malformed");
                return None;
            };
            return Some(OverlayEffect::TeleportPush(action));
        }
        OverlayKind::Help
        | OverlayKind::Debug
        | OverlayKind::Status
        | OverlayKind::DataRetention => {}
    }
    None
}

fn mark_config_special(state: &mut TuiState, path: &str) {
    if let Some(overlay) = state.overlay.as_mut() {
        for item in &mut overlay.items {
            item.action = OverlayAction::ConfigSpecial {
                path: path.to_owned(),
                value: item.id.clone(),
            };
        }
    }
}

fn config_special_value(
    runtime: &mut InteractiveRuntime,
    action: &OverlayAction,
    expected_path: &str,
) -> Option<(String, String)> {
    let OverlayAction::ConfigSpecial { path, value } = action else {
        return None;
    };
    (path == expected_path).then(|| {
        (
            value.clone(),
            selected_config_target(runtime).unwrap_or_else(|| "user".to_owned()),
        )
    })
}
