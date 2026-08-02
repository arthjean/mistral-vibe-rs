use std::path::Path;

use serde_json::json;

use super::attachments::{PreparedSubmission, PromptDraft, prepare_submission};
use super::clipboard_images::ClipboardImageManager;
use super::controls::ControlState;
use super::state::TuiState;
use super::{
    ActiveTurn, CliError, InteractiveRuntime, RuntimeSkill, settle_unstarted_reservation,
    start_active_turn,
};

pub(super) struct PromptContext<'a> {
    pub working_directory: &'a Path,
    pub runtime: &'a mut Option<InteractiveRuntime>,
    pub active: &'a mut Option<ActiveTurn>,
    pub state: &'a mut TuiState,
    pub controls: &'a mut ControlState,
    pub clipboard_images: &'a mut ClipboardImageManager,
}

impl<'a> PromptContext<'a> {
    pub(super) fn new(
        working_directory: &'a Path,
        runtime: &'a mut Option<InteractiveRuntime>,
        active: &'a mut Option<ActiveTurn>,
        state: &'a mut TuiState,
        controls: &'a mut ControlState,
        clipboard_images: &'a mut ClipboardImageManager,
    ) -> Self {
        Self {
            working_directory,
            runtime,
            active,
            state,
            controls,
            clipboard_images,
        }
    }

    pub(super) fn reborrow(&mut self) -> PromptContext<'_> {
        PromptContext {
            working_directory: self.working_directory,
            runtime: self.runtime,
            active: self.active,
            state: self.state,
            controls: self.controls,
            clipboard_images: self.clipboard_images,
        }
    }
}

pub(super) async fn enqueue_prompt(
    working_directory: &Path,
    prompt: &PromptDraft,
    runtime: &Option<InteractiveRuntime>,
    state: &mut TuiState,
) -> Result<bool, CliError> {
    let Some(prepared) =
        prepare_prompt_for_runtime(working_directory, prompt, runtime, state).await?
    else {
        return Ok(false);
    };
    state.prompt_queue.push_prepared(prompt.clone(), prepared);
    state.prompt_queue.resume();
    Ok(true)
}

pub(super) async fn prepare_prompt_for_runtime(
    working_directory: &Path,
    prompt: &PromptDraft,
    runtime: &Option<InteractiveRuntime>,
    state: &mut TuiState,
) -> Result<Option<PreparedSubmission>, CliError> {
    let Some(runtime) = runtime.as_ref() else {
        state.push_diagnostic("Setup is required before starting a session. Restart with --setup.");
        return Ok(None);
    };
    let preparation_workspace = working_directory.to_path_buf();
    let preparation_prompt = prompt.clone();
    let active_model = runtime.model.clone();
    let supports_images = runtime.supports_images();
    let skill = invoked_skill(runtime, prompt.text()).cloned();
    let preparation = tokio::task::spawn_blocking(move || {
        prepare_submission(
            &preparation_workspace,
            &preparation_prompt,
            &active_model,
            supports_images,
        )
    })
    .await;
    let mut prepared = match preparation {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(error)) => {
            state.push_diagnostic(error.to_string());
            return Ok(None);
        }
        Err(error) => {
            state.push_diagnostic(format!("Image preparation task failed: {error}"));
            return Ok(None);
        }
    };
    if let Some(skill) = skill {
        prepared
            .turn
            .input
            .push(vibe_app_server::client::PublicContentBlock::Resource {
                resource: json!({
                    "resource": {
                        "uri": skill.path.as_deref().unwrap_or("skill://invoked"),
                        "name": skill.name,
                        "text": skill.body,
                    }
                }),
            });
    }
    Ok(Some(prepared))
}

pub(super) async fn start_prompt(
    context: PromptContext<'_>,
    prompt: &PromptDraft,
) -> Result<bool, CliError> {
    start_prompt_with_client_id(context, prompt, None, None).await
}

pub(super) async fn start_prompt_with_client_id(
    context: PromptContext<'_>,
    prompt: &PromptDraft,
    client_message_id: Option<&str>,
    prepared: Option<PreparedSubmission>,
) -> Result<bool, CliError> {
    let PromptContext {
        working_directory,
        runtime,
        active,
        state,
        controls,
        clipboard_images,
    } = context;
    let mut protected = state.prompt_queue.transient_images();
    protected.extend(prompt.transient_image_paths().cloned());
    clipboard_images
        .discard_unreferenced(&protected, state)
        .await;
    let mut prepared = match prepared {
        Some(prepared) => prepared,
        None => {
            let Some(prepared) =
                prepare_prompt_for_runtime(working_directory, prompt, runtime, state).await?
            else {
                return Ok(false);
            };
            prepared
        }
    };
    prepared.turn.client_user_message_id = client_message_id.map(ToOwned::to_owned);
    let Some(runtime) = runtime.as_mut() else {
        state.push_diagnostic("Setup is required before starting a session. Restart with --setup.");
        return Ok(false);
    };
    let reservation = runtime
        .service
        .reserve_prepared_prompt(
            &runtime.session_id,
            &prepared.turn,
            prepared.provider_images,
        )
        .await?;
    let protected = state.prompt_queue.transient_images();
    clipboard_images
        .consume(&prepared.cleanup_paths, &protected, state)
        .await;
    let started = match start_active_turn(
        &runtime.service,
        reservation,
        None,
        state.watermark,
        controls,
    ) {
        Ok(started) => started,
        Err(failure) => {
            let (reservation, error) = *failure;
            let failure = format!("Reserved turn could not start locally: {error}");
            let settled = settle_unstarted_reservation(runtime, state, &reservation, &failure);
            return Ok(settled || client_message_id.is_none());
        }
    };
    *active = Some(started);
    state.waiting = true;
    Ok(true)
}

pub(super) fn is_user_skill(runtime: Option<&InteractiveRuntime>, input: &str) -> bool {
    runtime
        .and_then(|runtime| invoked_skill(runtime, input))
        .is_some()
}

fn invoked_skill<'a>(runtime: &'a InteractiveRuntime, input: &str) -> Option<&'a RuntimeSkill> {
    let name = input
        .trim()
        .strip_prefix('/')?
        .split_whitespace()
        .next()?
        .to_ascii_lowercase();
    runtime.skills.get(&name)
}
