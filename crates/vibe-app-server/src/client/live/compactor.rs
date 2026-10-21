//! The provider-bound half of compaction, which a turn summarizes under and
//! a manual compaction reuses.

use super::*;

/// The provider-bound half of compaction: it binds the core manager to this
/// session's provider and plan. A compacted session keeps its identifier, as
/// it does upstream since the envelope is appended rather than substituted.
///
/// The summarization itself lives one layer down, in
/// [`vibe_core::compaction::manager`], because it is provider-neutral: the call
/// shape, the failure taxonomy, the fallback and the retry ladder are the same
/// whichever backend answers, and keeping them there is what lets the compaction
/// corpus drive them with a scripted provider.
#[derive(Clone)]
pub(in crate::client) struct ProviderSessionCompactor {
    provider: Arc<dyn CompletionProvider>,
    /// The prompts, the model, the tools and the strict flag this session
    /// summarizes under.
    plan: Arc<CompactionPlan>,
}

impl ProviderSessionCompactor {
    pub(in crate::client) fn new(provider: Arc<dyn CompletionProvider>) -> Self {
        Self {
            provider,
            plan: Arc::new(CompactionPlan::default()),
        }
    }

    /// The same compactor, summarizing under `plan`.
    pub(in crate::client) fn with_plan(&self, plan: CompactionPlan) -> Self {
        Self {
            provider: Arc::clone(&self.provider),
            plan: Arc::new(plan),
        }
    }

    pub(super) async fn compact_with_instructions(
        &self,
        current_session_id: &str,
        messages: &[ModelMessage],
        extra_instructions: &str,
        requests: Option<compaction_manager::SummaryRequests>,
    ) -> Result<CompactionResult, CompactionFailure> {
        let plan = CompactionPlan {
            session_id: Some(current_session_id.to_owned()),
            requests,
            ..(*self.plan).clone()
        };
        let summarized = compaction_manager::compact(
            self.provider.as_ref(),
            &plan,
            messages,
            extra_instructions.trim(),
        )
        .await?;
        Ok(CompactionResult {
            new_session_id: current_session_id.to_owned(),
            summary: summarized.summary,
            messages: summarized.messages,
            usage: summarized.usage,
            failure: summarized.failure,
        })
    }

    /// The plan one session summarizes under: this process's resolved prompts,
    /// with the model, the strict flag and the live tool surface the session
    /// itself carries.
    pub(super) fn session_plan(
        &self,
        settings: &CompactionSettings,
        tools: Vec<ToolDefinition>,
        tool_choice: Option<ToolChoice>,
        thinking: bool,
    ) -> CompactionPlan {
        CompactionPlan {
            prompts: self.plan.prompts.clone(),
            model: settings.compaction_model.clone(),
            thinking,
            tools,
            tool_choice,
            strict: settings.raise_on_compaction_failure,
            ..CompactionPlan::default()
        }
    }
}

impl Compactor for ProviderSessionCompactor {
    fn compact<'a>(
        &'a self,
        current_session_id: &'a str,
        messages: &'a [ModelMessage],
    ) -> vibe_core::engine::CompactionFuture<'a> {
        Box::pin(async move {
            self.compact_with_instructions(current_session_id, messages, "", None)
                .await
        })
    }

    fn compact_reporting<'a>(
        &'a self,
        current_session_id: &'a str,
        messages: &'a [ModelMessage],
        requests: compaction_manager::SummaryRequests,
    ) -> vibe_core::engine::CompactionFuture<'a> {
        Box::pin(async move {
            self.compact_with_instructions(current_session_id, messages, "", Some(requests))
                .await
        })
    }

    fn cleared_session_id(&self, current_session_id: &str) -> Result<String, String> {
        Ok(rotate_session_id(current_session_id))
    }
}
