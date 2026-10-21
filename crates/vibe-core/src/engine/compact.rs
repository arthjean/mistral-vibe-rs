//! Compacting the transcript in the middle of a turn, and the requests the
//! summarization reports.

use std::sync::{Arc, Mutex};

use super::controls::CancellationToken;
use super::ledger::{TurnLedger, TurnRecorder, new_compaction_id};
use super::streaming::{request_sent, stamp_census};
use super::{
    Compactor, CompletionProvider, ConversationEngine, EngineError, ToolExecutor, TranscriptSink,
};
use crate::compaction::CompactionStatus;
use crate::events::{EngineEvent, ModelMessage, SessionHandoffCause};
use crate::middleware::{MiddlewarePipeline, ResetReason};
use crate::provider::ProviderInput;
use crate::sync;

/// The summarization requests one compaction made: each is stamped with the
/// census as a secondary call and kept as the `RequestSent` it reports.
///
/// Reference `AgentLoop._complete` with `call_type="secondary_call"`, which
/// sends `vibe.request_sent` before every summarization call, the fallback and
/// the overflow retries included, under the current user message.
pub struct SummaryRequestRecorder {
    census: Option<crate::telemetry::TelemetryContext>,
    session_id: String,
    message_id: Option<String>,
    agent_profile: String,
    /// The label every request reports as its model, the summarizing model's
    /// alias.
    model: Option<String>,
    sent: Mutex<Vec<EngineEvent>>,
}

impl SummaryRequestRecorder {
    #[must_use]
    pub fn new(
        census: Option<crate::telemetry::TelemetryContext>,
        session_id: String,
        message_id: Option<String>,
        agent_profile: String,
        model: Option<String>,
    ) -> Self {
        Self {
            census,
            session_id,
            message_id,
            agent_profile,
            model,
            sent: Mutex::new(Vec::new()),
        }
    }

    /// The reports of every request made so far, in order.
    pub fn take(&self) -> Vec<EngineEvent> {
        std::mem::take(&mut *sync::lock(&self.sent))
    }
}

impl crate::compaction::manager::SummaryRequestHook for SummaryRequestRecorder {
    fn before_request(&self, input: &mut ProviderInput) {
        let call_type = crate::telemetry::TelemetryCallType::SecondaryCall;
        stamp_census(
            self.census.as_ref(),
            input,
            &self.session_id,
            call_type,
            &self.message_id,
        );
        let model = self
            .model
            .clone()
            .or_else(|| input.model_override.clone())
            .unwrap_or_default();
        let report = request_sent(
            input,
            model,
            self.agent_profile.clone(),
            self.message_id.clone(),
            call_type,
        );
        sync::lock(&self.sent).push(report);
    }
}

impl<P, T, C, S> ConversationEngine<P, T, C, S>
where
    P: CompletionProvider,
    T: ToolExecutor,
    C: Compactor,
    S: TranscriptSink,
{
    /// Compacts the transcript, whether a policy asked for it or an overflow
    /// forced it.
    ///
    /// Both entries clear the pipeline's latched state here rather than at
    /// their own call site, because a policy measured against the transcript is
    /// stale either way and the reference resets from the one function both
    /// paths share.
    ///
    /// `None` means the turn was cancelled while compaction was running.
    ///
    /// The boundary events bracket the model call the way the reference's
    /// `_run_compaction` does: the start event is emitted before the compactor
    /// is asked for anything, the completed event only once there is a summary
    /// and a new identifier, and the outcome event on every path, because a
    /// compaction that failed or was cancelled still reports its status.
    pub(super) async fn compact(
        &self,
        recorder: &mut TurnRecorder<'_>,
        messages: &mut Vec<ModelMessage>,
        message_id: Option<String>,
        pipeline: &MiddlewarePipeline,
        ledger: &mut TurnLedger,
        cancellation: &CancellationToken,
    ) -> Result<Option<()>, EngineError> {
        let context_tokens = ledger.context_tokens;
        let compaction_id = new_compaction_id();
        let threshold = self.settings.compaction.auto_compact_threshold;
        let old_session_id = recorder.state().session_id.clone();
        recorder.emit(EngineEvent::CompactionStarted {
            compaction_id: compaction_id.clone(),
            current_context_tokens: context_tokens,
            threshold,
        })?;
        let outcome = |status, reason| EngineEvent::CompactionOutcome {
            compaction_id: compaction_id.clone(),
            status,
            context_tokens_before: context_tokens,
            threshold,
            reason,
        };
        let requests = Arc::new(SummaryRequestRecorder::new(
            self.settings.request_census.clone(),
            old_session_id.clone(),
            message_id,
            self.settings.agent_profile.clone(),
            self.settings
                .compaction
                .compaction_model_alias
                .clone()
                .or_else(|| self.provider.model().map(ToOwned::to_owned)),
        ));
        let compaction = tokio::select! {
            result = self.compactor.compact_reporting(
                &old_session_id,
                messages,
                crate::compaction::manager::SummaryRequests(requests.clone()),
            ) => Some(result),
            () = cancellation.cancelled() => None,
        };
        // Every request made was reported, a cancelled compaction's included.
        for report in requests.take() {
            recorder.emit(report)?;
        }
        let Some(compaction) = compaction else {
            recorder.emit(outcome(CompactionStatus::Cancelled, None))?;
            return Ok(None);
        };
        let compaction = match compaction {
            Ok(compaction) => compaction,
            Err(failure) => {
                // A failed compaction still made the calls it made, and the
                // transcript it read is left exactly as it was.
                ledger.record_compaction(&failure.usage, &self.settings.limits, false);
                recorder.emit(outcome(CompactionStatus::Failure, failure.reason))?;
                return Err(EngineError::Compaction(failure.message));
            }
        };
        ledger.record_compaction(&compaction.usage, &self.settings.limits, true);
        let summary_length = u64::try_from(compaction.summary.chars().count()).unwrap_or(u64::MAX);
        let new_session_id = compaction.new_session_id;
        // The completed event precedes the handoff because the handoff is what
        // raises `session/compacted` here, and the reference raises that
        // notification from the end event itself: the summary length has to be
        // known by the time the session rotates.
        recorder.emit(EngineEvent::CompactionCompleted {
            compaction_id: compaction_id.clone(),
            summary_length,
            old_session_id: old_session_id.clone(),
            new_session_id: new_session_id.clone(),
        })?;
        // A compactor that keeps the identifier, which is what the reference
        // does since it stopped rotating on compaction, hands nothing off.
        if new_session_id != old_session_id {
            recorder.emit(EngineEvent::SessionHandoff {
                from_session_id: old_session_id,
                to_session_id: new_session_id,
                cause: SessionHandoffCause::Compaction,
            })?;
        }
        // A compaction that degraded to the placeholder still succeeded: the
        // conversation is compacted. It reports the reason it degraded from, so
        // the failure record the reference sends alongside the success is
        // written here too.
        recorder.emit(outcome(CompactionStatus::Success, compaction.failure))?;
        *messages = compaction.messages;
        pipeline.reset(ResetReason::Compact);
        Ok(Some(()))
    }
}
