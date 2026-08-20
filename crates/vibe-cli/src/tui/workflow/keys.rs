//! Which key an open overlay claims, and what it does with it.
//!
//! Every list overlay navigates, filters, closes and activates the same way.
//! What separates them is declared once in [`policy`], so a new overlay states
//! its behavior in one exhaustive table rather than by adding a guard to the
//! shared navigation. The three overlays that are not lists at all -- the
//! rewind panel, the session picker and the remote-project form -- own their
//! own reducers and are routed to before the shared one runs.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::super::chat_input::ChatInputState;
use super::super::controls::ControlState;
use super::super::interaction::RemoteProjectField;
use super::super::interaction::{
    OverlayAction, OverlayKind, RemoteProjectAction, TeleportPushAction,
};
use super::super::pickers::remote_project_create_overlay;
use super::super::session_picker::{SessionPickerEffect, reduce_key as reduce_session_picker_key};
use super::super::setup::ResolvedTheme;
use super::super::state::TuiState;
use super::super::{InteractiveRuntime, preview_theme};
use super::config::persisted_theme;
use super::mcp::{McpEffect, refresh_selected_mcp, set_selected_mcp};
use super::overlay::select_overlay_item;
use super::{
    OverlayEffect, OverlayKeyResult, delete_selected_session, handle_rewind_key,
    reset_selected_config, resume_selected_session,
};

/// What separates one list overlay from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OverlayPolicy {
    /// What `Esc` answers before the overlay closes.
    escape: Escape,
    /// Whether `Enter` and `Space` activate the selected item. A read-only
    /// panel has nothing to activate.
    activates: bool,
    /// Whether moving or filtering the selection previews the highlighted
    /// value, which only the theme catalog does.
    previews: bool,
    /// The extra chords this overlay claims for itself.
    chords: Chords,
}

/// What an overlay answers when the operator dismisses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escape {
    /// Closing is the whole answer.
    Close,
    /// Reference: dismissing the push approval refuses the push rather than
    /// leaving the operation waiting.
    RefusePush,
    /// Dismissing the project picker cancels the selection it was opened for.
    CancelRemoteProject,
    /// Reference `ThemePickerApp.Cancelled`: cancelling restores the persisted
    /// theme, discarding every preview.
    RestoreTheme,
}

/// The chords an overlay claims beyond navigation and filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chords {
    None,
    /// `Ctrl+R` refreshes, `e` and `d` enable and disable, and `Backspace`
    /// leaves a server's detail for the list it was opened from.
    Mcp {
        detail: bool,
    },
    /// `Ctrl+R` restores the selected field to its default.
    ConfigReset,
}

/// Every overlay's behavior, in one exhaustive table: adding a kind without
/// deciding how it behaves does not compile.
const fn policy(kind: OverlayKind) -> OverlayPolicy {
    const LIST: OverlayPolicy = OverlayPolicy {
        escape: Escape::Close,
        activates: true,
        previews: false,
        chords: Chords::None,
    };
    match kind {
        // Read-only panels: they scroll and close, and have nothing to activate.
        OverlayKind::Debug | OverlayKind::Status | OverlayKind::DataRetention => OverlayPolicy {
            activates: false,
            ..LIST
        },
        OverlayKind::Theme => OverlayPolicy {
            escape: Escape::RestoreTheme,
            previews: true,
            ..LIST
        },
        OverlayKind::TeleportApproval => OverlayPolicy {
            escape: Escape::RefusePush,
            ..LIST
        },
        OverlayKind::RemoteProjects => OverlayPolicy {
            escape: Escape::CancelRemoteProject,
            ..LIST
        },
        OverlayKind::Mcp | OverlayKind::Connectors => OverlayPolicy {
            chords: Chords::Mcp { detail: false },
            ..LIST
        },
        OverlayKind::McpDetail => OverlayPolicy {
            chords: Chords::Mcp { detail: true },
            ..LIST
        },
        OverlayKind::Config => OverlayPolicy {
            chords: Chords::ConfigReset,
            ..LIST
        },
        OverlayKind::ConfigChoice
        | OverlayKind::ConfigTarget
        | OverlayKind::Model
        | OverlayKind::Thinking
        | OverlayKind::Sessions
        | OverlayKind::McpAuth
        | OverlayKind::Voice
        | OverlayKind::VoiceModel
        | OverlayKind::Proxy
        | OverlayKind::RemoteProjectCreate => LIST,
    }
}

/// Routes one key to whatever is open over the transcript.
pub(in crate::tui) async fn handle_overlay_key(
    key: KeyEvent,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
    composer: &mut ChatInputState,
    theme: &mut ResolvedTheme,
) -> OverlayKeyResult {
    // The three overlays below are state machines rather than filtered lists,
    // so each owns its own reducer instead of bending the shared one.
    if state.rewind.is_some() {
        handle_rewind_key(key, runtime, state, controls, composer);
        return OverlayKeyResult::Handled;
    }
    let Some(kind) = state.overlay.as_ref().map(|overlay| overlay.kind) else {
        return OverlayKeyResult::Unhandled;
    };
    match kind {
        OverlayKind::RemoteProjectCreate => handle_remote_project_create_key(key, runtime, state),
        OverlayKind::Sessions => {
            handle_session_picker_key(key, runtime, state, controls);
            OverlayKeyResult::Handled
        }
        kind => reduce_list_key(policy(kind), key, runtime, state, controls, composer, theme).await,
    }
}

fn handle_session_picker_key(
    key: KeyEvent,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
) {
    let current_session_id = runtime
        .as_ref()
        .map_or("", |runtime| runtime.session_id.as_str());
    let Some(overlay) = state.overlay.as_mut() else {
        return;
    };
    match reduce_session_picker_key(overlay, &mut state.session_delete, current_session_id, key) {
        SessionPickerEffect::None => {}
        SessionPickerEffect::Close => state.overlay = None,
        SessionPickerEffect::Resume(session_id) => {
            if let Some(runtime) = runtime.as_mut() {
                resume_selected_session(runtime, state, controls, &session_id);
            }
        }
        SessionPickerEffect::Delete(session_id) => {
            delete_selected_session(runtime, state, &session_id);
        }
    }
}

/// The remote-project form: three rows, two of which edit a draft field.
///
/// Every keystroke rebuilds the overlay from the draft, because the rows render
/// the values they hold; the selection is restored by the row it was on.
pub(in crate::tui) fn handle_remote_project_create_key(
    key: KeyEvent,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
) -> OverlayKeyResult {
    let Some(runtime) = runtime.as_mut() else {
        state.overlay = None;
        return OverlayKeyResult::Handled;
    };
    let Some(mut draft) = runtime.remote_project_draft.clone() else {
        state.push_diagnostic("Remote project draft is unavailable");
        state.overlay.clone_from(&runtime.remote_project_overlay);
        return OverlayKeyResult::Handled;
    };
    let selected = state
        .overlay
        .as_ref()
        .and_then(|overlay| overlay.selected_item())
        .and_then(|item| RemoteProjectField::from_id(&item.id));
    match key.code {
        KeyCode::Esc => {
            runtime.remote_project_draft = None;
            state.overlay.clone_from(&runtime.remote_project_overlay);
            return OverlayKeyResult::Handled;
        }
        KeyCode::Up | KeyCode::BackTab => {
            move_selection(state, -1);
            return OverlayKeyResult::Handled;
        }
        // A field answers `Enter` by moving on; only the button submits.
        KeyCode::Down | KeyCode::Tab | KeyCode::Enter
            if selected != Some(RemoteProjectField::Submit) =>
        {
            move_selection(state, 1);
            return OverlayKeyResult::Handled;
        }
        KeyCode::Enter => {
            return OverlayKeyResult::Effect(OverlayEffect::RemoteProject(
                RemoteProjectAction::Create {
                    name: draft.name.trim().to_owned(),
                    default_branch: draft.default_branch.trim().to_owned(),
                },
            ));
        }
        KeyCode::Backspace | KeyCode::Char(_) if key.modifiers.is_empty() => {
            let Some(edited) = selected.and_then(|field| field.edited(&mut draft)) else {
                return OverlayKeyResult::Handled;
            };
            match key.code {
                KeyCode::Backspace => {
                    edited.pop();
                }
                KeyCode::Char(character) => edited.push(character),
                _ => return OverlayKeyResult::Handled,
            }
        }
        _ => return OverlayKeyResult::Handled,
    }
    let mut overlay = remote_project_create_overlay(&draft);
    if let Some(field) = selected {
        overlay.select_by_id(field.id());
    }
    runtime.remote_project_draft = Some(draft);
    state.overlay = Some(overlay);
    OverlayKeyResult::Handled
}

/// The shared list behavior: dismiss, navigate, filter, activate, plus the
/// chords the policy claims.
async fn reduce_list_key(
    policy: OverlayPolicy,
    key: KeyEvent,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
    composer: &mut ChatInputState,
    theme: &mut ResolvedTheme,
) -> OverlayKeyResult {
    if let Some(result) = reduce_chord(policy.chords, key, runtime, state) {
        return result;
    }
    match key.code {
        KeyCode::Esc => return dismiss(policy.escape, runtime, state, theme),
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            move_selection(state, -1);
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            move_selection(state, 1);
        }
        KeyCode::Backspace if key.modifiers.is_empty() => {
            if let Some(overlay) = state.overlay.as_mut() {
                overlay.pop_query();
            }
        }
        KeyCode::Enter | KeyCode::Char(' ') if policy.activates && key.modifiers.is_empty() => {
            if let Some(effect) =
                select_overlay_item(runtime, state, controls, composer, theme).await
            {
                return OverlayKeyResult::Effect(effect);
            }
            return OverlayKeyResult::Handled;
        }
        KeyCode::Char(character) if key.modifiers.is_empty() => {
            if let Some(overlay) = state.overlay.as_mut() {
                overlay.push_query(character);
            }
        }
        _ => return OverlayKeyResult::Handled,
    }
    if policy.previews {
        preview_selected(state, theme);
    }
    OverlayKeyResult::Handled
}

fn move_selection(state: &mut TuiState, delta: isize) {
    if let Some(overlay) = state.overlay.as_mut() {
        overlay.move_selection(delta);
    }
}

/// Reference `on_option_list_option_highlighted`: the highlighted theme
/// previews immediately, without touching the configuration.
fn preview_selected(state: &TuiState, theme: &mut ResolvedTheme) {
    if let Some(selected) = state
        .overlay
        .as_ref()
        .and_then(super::super::interaction::Overlay::selected_item)
    {
        preview_theme(&selected.id, theme);
    }
}

fn dismiss(
    escape: Escape,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    theme: &mut ResolvedTheme,
) -> OverlayKeyResult {
    match escape {
        Escape::RefusePush => {
            let refusal = state.overlay.as_ref().and_then(|overlay| {
                overlay.items.iter().find_map(|item| match &item.action {
                    OverlayAction::TeleportPush(action) => Some(TeleportPushAction {
                        operation_id: action.operation_id.clone(),
                        approved: false,
                    }),
                    _ => None,
                })
            });
            // A panel that named no operation has nothing to refuse, so it just
            // closes rather than answering a push nobody asked for.
            return refusal.map_or(OverlayKeyResult::Handled, |action| {
                OverlayKeyResult::Effect(OverlayEffect::TeleportPush(action))
            });
        }
        Escape::CancelRemoteProject => {
            return OverlayKeyResult::Effect(OverlayEffect::RemoteProject(
                RemoteProjectAction::Cancel,
            ));
        }
        Escape::RestoreTheme => {
            if let Some(runtime) = runtime.as_mut() {
                preview_theme(&persisted_theme(runtime), theme);
            }
        }
        Escape::Close => {}
    }
    state.overlay = None;
    OverlayKeyResult::Handled
}

/// The chords an overlay claims for itself, answered before the shared list
/// behavior sees the key.
fn reduce_chord(
    chords: Chords,
    key: KeyEvent,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
) -> Option<OverlayKeyResult> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let bare = key.modifiers.is_empty();
    match (chords, key.code) {
        (Chords::Mcp { detail: true }, KeyCode::Backspace) if bare => Some(
            OverlayKeyResult::Effect(OverlayEffect::Mcp(McpEffect::Show { filter: None })),
        ),
        (Chords::Mcp { .. }, KeyCode::Char('r')) if control => {
            Some(mcp_effect(refresh_selected_mcp(state)))
        }
        (Chords::Mcp { .. }, KeyCode::Char('d')) if bare => {
            Some(mcp_effect(set_selected_mcp(state, false)))
        }
        (Chords::Mcp { .. }, KeyCode::Char('e')) if bare => {
            Some(mcp_effect(set_selected_mcp(state, true)))
        }
        (Chords::ConfigReset, KeyCode::Char('r')) if control => {
            reset_selected_config(runtime, state);
            Some(OverlayKeyResult::Handled)
        }
        _ => None,
    }
}

/// A chord that resolved to nothing still belongs to the overlay: it is
/// swallowed rather than typed into the filter.
fn mcp_effect(effect: Option<McpEffect>) -> OverlayKeyResult {
    effect.map_or(OverlayKeyResult::Handled, |effect| {
        OverlayKeyResult::Effect(OverlayEffect::Mcp(effect))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::interaction::{Overlay, OverlayItem};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn open(kind: OverlayKind, items: Vec<OverlayItem>) -> TuiState {
        let mut state = TuiState::new("session");
        state.overlay = Some(Overlay::new(kind, "title", items));
        state
    }

    async fn press(kind: OverlayKind, items: Vec<OverlayItem>, code: KeyCode) -> OverlayKeyResult {
        let mut state = open(kind, items);
        let mut theme = ResolvedTheme {
            theme: crate::tui::setup::Theme::Dark,
            colors_enabled: false,
        };
        handle_overlay_key(
            key(code),
            &mut None,
            &mut state,
            &mut ControlState::new("session"),
            &mut ChatInputState::default(),
            &mut theme,
        )
        .await
    }

    /// Dismissing the project picker answers the selection it was opened for
    /// rather than leaving the caller waiting on one.
    #[tokio::test]
    async fn dismissing_the_project_picker_cancels_the_selection() {
        assert_eq!(
            press(
                OverlayKind::RemoteProjects,
                vec![OverlayItem::new("p1", "project", "", false)],
                KeyCode::Esc,
            )
            .await,
            OverlayKeyResult::Effect(OverlayEffect::RemoteProject(RemoteProjectAction::Cancel))
        );
    }

    /// Dismissing the push approval refuses the push, so the operation the
    /// server is holding settles instead of hanging.
    #[tokio::test]
    async fn dismissing_the_push_approval_refuses_the_push() {
        let item = OverlayItem::new("push", "Push", "", false).with_action(
            OverlayAction::TeleportPush(TeleportPushAction {
                operation_id: "op-1".to_owned(),
                approved: true,
            }),
        );
        assert_eq!(
            press(OverlayKind::TeleportApproval, vec![item], KeyCode::Esc).await,
            OverlayKeyResult::Effect(OverlayEffect::TeleportPush(TeleportPushAction {
                operation_id: "op-1".to_owned(),
                approved: false,
            }))
        );
    }

    /// A read-only panel has nothing to activate, so `Enter` closes nothing and
    /// selects nothing rather than reaching the selection handler.
    #[tokio::test]
    async fn a_read_only_panel_activates_nothing() {
        for kind in [
            OverlayKind::Debug,
            OverlayKind::Status,
            OverlayKind::DataRetention,
        ] {
            assert!(!policy(kind).activates, "{kind:?} is read-only");
            assert_eq!(
                press(
                    kind,
                    vec![OverlayItem::new("a", "a", "", false)],
                    KeyCode::Enter
                )
                .await,
                OverlayKeyResult::Handled
            );
        }
    }

    /// `Backspace` leaves a server's detail for the list it was opened from,
    /// and edits the filter everywhere else.
    #[tokio::test]
    async fn backspace_leaves_the_server_detail_and_filters_elsewhere() {
        let item = || vec![OverlayItem::new("a", "a", "", false)];
        assert_eq!(
            press(OverlayKind::McpDetail, item(), KeyCode::Backspace).await,
            OverlayKeyResult::Effect(OverlayEffect::Mcp(McpEffect::Show { filter: None }))
        );
        assert_eq!(
            press(OverlayKind::Mcp, item(), KeyCode::Backspace).await,
            OverlayKeyResult::Handled
        );
    }

    /// Only the theme catalog previews what the selection highlights.
    #[test]
    fn the_theme_catalog_is_the_only_previewing_overlay() {
        for kind in [
            OverlayKind::Config,
            OverlayKind::Model,
            OverlayKind::Mcp,
            OverlayKind::Sessions,
            OverlayKind::Proxy,
        ] {
            assert!(!policy(kind).previews, "{kind:?} previews nothing");
        }
        assert!(policy(OverlayKind::Theme).previews);
    }

    /// A key no overlay claims is still swallowed: an open overlay is modal.
    #[tokio::test]
    async fn an_open_overlay_swallows_the_keys_it_does_not_claim() {
        assert_eq!(
            press(
                OverlayKind::Debug,
                vec![OverlayItem::new("a", "a", "", false)],
                KeyCode::F(5),
            )
            .await,
            OverlayKeyResult::Handled
        );
    }

    /// With nothing open the key belongs to the composer.
    #[tokio::test]
    async fn a_closed_overlay_claims_nothing() {
        let mut state = TuiState::new("session");
        let mut theme = ResolvedTheme {
            theme: crate::tui::setup::Theme::Dark,
            colors_enabled: false,
        };
        assert_eq!(
            handle_overlay_key(
                key(KeyCode::Enter),
                &mut None,
                &mut state,
                &mut ControlState::new("session"),
                &mut ChatInputState::default(),
                &mut theme,
            )
            .await,
            OverlayKeyResult::Unhandled
        );
    }
}
