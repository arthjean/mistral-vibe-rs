use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::clipboard_images::ClipboardImageManager;
use super::interaction::{QueuedIntent, QueuedIntentKind};
use super::prompt::{PromptContext, prepare_prompt_for_runtime, start_prompt_with_client_id};
use super::state::TuiState;
use super::{CliError, InteractiveRuntime, start_shell};

enum BatchOutcome {
    Complete,
    Pause,
}

pub(super) async fn start_next_queued_prompt(
    mut context: PromptContext<'_>,
) -> Result<(), CliError> {
    if context.active.is_some()
        || context
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.shell.is_some())
        || context.controls.pending_callback().is_some()
    {
        return Ok(());
    }
    let Some(mut batch) = context.state.prompt_queue.take_next_batch() else {
        return Ok(());
    };

    let result = process_batch(&mut context, &mut batch).await;

    match result {
        Ok(BatchOutcome::Complete) => debug_assert!(batch.is_empty()),
        Ok(BatchOutcome::Pause) => context.state.prompt_queue.restore_batch_and_pause(batch),
        Err(error) => {
            context.state.prompt_queue.restore_batch_and_pause(batch);
            return Err(error);
        }
    }
    Ok(())
}

async fn process_batch(
    context: &mut PromptContext<'_>,
    batch: &mut Vec<QueuedIntent>,
) -> Result<BatchOutcome, CliError> {
    if batch[0].kind == QueuedIntentKind::Shell {
        if start_shell(batch[0].draft.text(), context.runtime, context.state).await? {
            batch.clear();
            return Ok(BatchOutcome::Complete);
        }
        return Ok(BatchOutcome::Pause);
    }
    if context
        .runtime
        .as_ref()
        .is_some_and(|runtime| !runtime.supports_images())
        && batch.iter().any(|item| {
            item.prepared
                .as_ref()
                .is_some_and(|prepared| !prepared.provider_images.as_slice().is_empty())
        })
    {
        context.state.push_diagnostic(
            "The active model does not support queued images; switch models and resume the queue",
        );
        return Ok(BatchOutcome::Pause);
    }

    let flush_before_shell =
        context.state.prompt_queue.next_kind() == Some(QueuedIntentKind::Shell);
    let injection_count = if flush_before_shell {
        batch.len()
    } else {
        batch.len().saturating_sub(1)
    };
    for _ in 0..injection_count {
        let protected = protected_images(context.state, batch, 1);
        if !inject_queued_prompt(
            context.working_directory,
            &batch[0],
            context.runtime,
            context.state,
            context.clipboard_images,
            &protected,
        )
        .await?
        {
            return Ok(BatchOutcome::Pause);
        }
        batch.remove(0);
    }
    if flush_before_shell {
        debug_assert!(batch.is_empty());
        return Ok(BatchOutcome::Complete);
    }

    let tail = &batch[0];
    if start_prompt_with_client_id(
        context.reborrow(),
        &tail.draft,
        Some(&tail.id),
        tail.prepared.clone(),
    )
    .await?
    {
        batch.clear();
        return Ok(BatchOutcome::Complete);
    }
    Ok(BatchOutcome::Pause)
}

fn protected_images(state: &TuiState, batch: &[QueuedIntent], skip: usize) -> HashSet<PathBuf> {
    let mut protected = state.prompt_queue.transient_images();
    protected.extend(
        batch[skip..]
            .iter()
            .flat_map(|item| item.draft.transient_image_paths())
            .cloned(),
    );
    protected
}

async fn inject_queued_prompt(
    working_directory: &Path,
    item: &QueuedIntent,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    clipboard_images: &mut ClipboardImageManager,
    protected: &HashSet<PathBuf>,
) -> Result<bool, CliError> {
    let prepared = if let Some(prepared) = item.prepared.clone() {
        prepared
    } else if let Some(prepared) =
        prepare_prompt_for_runtime(working_directory, &item.draft, runtime, state).await?
    {
        prepared
    } else {
        return Ok(false);
    };
    let Some(runtime) = runtime.as_mut() else {
        state.push_diagnostic("Setup is required before draining queued input");
        return Ok(false);
    };
    let injected_skill = prepared.turn.input.iter().any(|block| {
        matches!(
            block,
            vibe_app_server::client::PublicContentBlock::Resource { .. }
        )
    });
    match runtime
        .service
        .public_call_async(
            "session/context/inject",
            json!({
                "sessionId": runtime.session_id,
                "input": prepared.turn.input,
                "asMessage": true,
                "injectInvokedSkill": injected_skill,
                "clientUserMessageId": item.id,
                "mentionStats": prepared.turn.mention_stats,
            }),
        )
        .await
    {
        Ok(_) => {
            clipboard_images
                .consume(&prepared.cleanup_paths, protected, state)
                .await;
            Ok(true)
        }
        Err(error) => {
            state.push_diagnostic(format!("Queued prompt injection failed: {error}"));
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::attachments::PromptDraft;

    #[test]
    fn failed_batch_is_restored_before_the_queue_pauses() {
        let mut state = TuiState::new("session");
        state.prompt_queue.push(PromptDraft::text_only("first"));
        state.prompt_queue.push(PromptDraft::text_only("second"));
        let batch = state.prompt_queue.take_next_batch().expect("batch owned");

        state.prompt_queue.restore_batch_and_pause(batch);

        assert!(state.prompt_queue.is_paused());
        assert_eq!(state.prompt_queue.len(), 2);
        state.prompt_queue.resume();
        let restored = state
            .prompt_queue
            .take_next_batch()
            .expect("batch restored");
        assert_eq!(restored[0].draft.text(), "first");
        assert_eq!(restored[1].draft.text(), "second");
    }
}
