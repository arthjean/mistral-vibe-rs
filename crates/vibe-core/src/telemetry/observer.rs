//! Turning the engine's event stream into telemetry.
//!
//! The engine publishes what happened; the collector wants a much smaller set
//! of records, each carrying context the events only imply. So the observer
//! keeps a per-turn context and a table of tool calls still in flight, and
//! emits a record when the event that completes one arrives. Nothing it does
//! can fail the turn: a transport that refuses is a dropped record, not a
//! broken conversation.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::*;
use crate::events::{EngineEvent, EventEnvelope};

/// How the client reaches the configuration on every send. Reference
/// `TelemetryClient._config_getter`.
pub type TelemetryConfigGetter = Arc<dyn Fn() -> TelemetryConfig + Send + Sync>;

pub struct TelemetryClient<T> {
    config: TelemetryConfigGetter,
    pub(super) transport: T,
}

impl<T: TelemetryTransport> TelemetryClient<T> {
    #[must_use]
    pub fn new(config: TelemetryConfigGetter, transport: T) -> Self {
        Self { config, transport }
    }

    /// A client that never delivers, for a caller that has no configuration to
    /// read.
    #[must_use]
    pub fn disabled(transport: T) -> Self {
        Self::new(Arc::new(TelemetryConfig::disabled), transport)
    }

    pub async fn record(
        &self,
        envelope: &TelemetryEnvelope,
    ) -> Result<TelemetryOutcome, TelemetryError> {
        let config = (self.config)();
        if !config.enabled {
            return Ok(TelemetryOutcome::Disabled);
        }
        let (Some(target), Some(credential)) = (config.target.as_ref(), config.credential.as_ref())
        else {
            return Ok(TelemetryOutcome::NoEligibleCredential);
        };
        self.transport
            .send(&target.endpoint, &target.user_agent, credential, envelope)
            .await?;
        Ok(TelemetryOutcome::Sent)
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        (self.config)().is_active()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryOutcome {
    Disabled,
    NoEligibleCredential,
    Sent,
}

/// What the turn's own events reported about the request they belong to.
///
/// The reference reads the model, the profile and the current message
/// identifier off its agent loop where it builds each event. This port projects
/// events from a stream, so the same facts are remembered here as they pass:
/// [`EngineEvent::RequestSent`] sets them and the tool events of that request
/// read them back.
#[derive(Debug, Default)]
struct TurnContext {
    model: String,
    agent_profile: String,
    message_id: Option<String>,
    /// What a rating correlates with. Reference `last_correlation_id`, which
    /// its loop fills from the identifier the backend answered with; this port
    /// has no such header, so the turn that made the request is what a rating
    /// of that request points at.
    last_correlation_id: Option<String>,
    /// Tool calls the model declared and that have not answered yet, by
    /// identifier. The reference reads the same pair off its `ResolvedToolCall`.
    calls: BTreeMap<String, PendingToolCall>,
}

/// What a tool event reports for a fact no request named yet. The reference
/// always has an active model and an active profile; a stream that answers a
/// tool call before any request has none, and reports the same `unknown` its
/// absent entrypoint reports rather than an empty label.
fn known(value: &str) -> String {
    if value.is_empty() {
        return "unknown".to_owned();
    }
    value.to_owned()
}

#[derive(Debug)]
struct PendingToolCall {
    name: String,
    arguments: Value,
}

/// Where an event a client authored is handed to, so a wire dispatch can ship
/// one without holding a transport of its own.
///
/// Reference `TelemetryResource.record`, whose `telemetry/record` request the
/// app server hands to the agent loop's own telemetry client
/// (`vibe/app_server/_resources.py:488-499`). The name and the properties are
/// the client's, so neither is rewritten here: only the census is merged
/// underneath them, and the caller's keys win. The reference validates nothing
/// on this path either, which is why the label validators this port applies to
/// its own events are absent from it, asserted by
/// `authored_labels_are_validated_and_client_properties_are_not`.
pub trait ClientTelemetry: Send + Sync {
    fn record_client_event(
        &self,
        name: &str,
        properties: Map<String, Value>,
        session_id: Option<&str>,
        correlate_last_request: bool,
    );
}

/// The sink a server with no telemetry client installed answers with, which
/// keeps `telemetry/record` answering empty rather than failing.
pub struct NoClientTelemetry;

impl ClientTelemetry for NoClientTelemetry {
    fn record_client_event(
        &self,
        _name: &str,
        _properties: Map<String, Value>,
        _session_id: Option<&str>,
        _correlate_last_request: bool,
    ) {
    }
}

pub struct TelemetryEventObserver<T> {
    pub(super) client: Arc<TelemetryClient<T>>,
    context: TelemetryContext,
    pending: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    turn: Mutex<TurnContext>,
}

impl<T> TelemetryEventObserver<T>
where
    T: TelemetryTransport + 'static,
{
    #[must_use]
    pub fn new(client: TelemetryClient<T>, context: TelemetryContext) -> Self {
        Self {
            client: Arc::new(client),
            context,
            pending: Mutex::new(Vec::new()),
            turn: Mutex::new(TurnContext::default()),
        }
    }

    /// Queues one event raised outside the engine stream, which is how every
    /// client surface reports: the rating prompt, the slash commands, the voice
    /// toggle, the two audio managers and the teleport tracker.
    pub fn record(
        &self,
        record: &TelemetryRecord,
        session_id: Option<&str>,
    ) -> Result<(), TelemetryError> {
        let correlation_id = self.correlation(record.correlates_last_request());
        self.queue(record, session_id, correlation_id)
    }

    /// Builds the envelope and hands the delivery to a task, so neither the
    /// configuration read nor the request touches the caller's path.
    pub(super) fn queue(
        &self,
        record: &TelemetryRecord,
        session_id: Option<&str>,
        correlation_id: Option<String>,
    ) -> Result<(), TelemetryError> {
        if let Some(correlation_id) = correlation_id.as_deref() {
            validate_safe_identifier(correlation_id)?;
        }
        let census = match record.call_type() {
            Some(call_type) => self
                .context
                .request_metadata(session_id, call_type, None)
                .properties(),
            None => self.context.base_metadata(session_id).properties(),
        };
        let attributes = record.attributes(self.context.launch.as_ref())?;
        let envelope = TelemetryEnvelope::new(
            record.event().event_name(),
            merge_properties(census, attributes.into_properties()),
            correlation_id,
        );
        self.deliver(envelope);
        Ok(())
    }

    /// Hands one envelope to a task and remembers it, so [`Self::flush`] still
    /// awaits a delivery the caller never sees.
    fn deliver(&self, envelope: TelemetryEnvelope) {
        // Telemetry never decides whether a caller runs: an observer reached
        // from outside a runtime drops the delivery rather than failing the
        // path that produced the event.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = Arc::clone(&self.client);
        let task = runtime.spawn(async move {
            let _ = client.record(&envelope).await;
        });
        if let Ok(mut pending) = self.pending.lock() {
            pending.retain(|task| !task.is_finished());
            pending.push(task);
        }
    }

    pub async fn flush(&self) {
        let pending = self
            .pending
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default();
        for task in pending {
            let _ = task.await;
        }
    }

    /// What a request that asked to correlate points at: the turn that made the
    /// last backend request, which is this port's `last_correlation_id`.
    fn correlation(&self, correlate_last_request: bool) -> Option<String> {
        if !correlate_last_request {
            return None;
        }
        self.turn
            .lock()
            .ok()
            .and_then(|turn| turn.last_correlation_id.clone())
    }

    /// The records one engine event reports, with the turn context updated by
    /// the events that carry it.
    ///
    /// Most events report one or none; a compaction reports two when its
    /// summarization failed with a classified reason, because the reference
    /// sends the auto-compaction record from the loop and the failure record
    /// from the manager, for the same compaction.
    pub(super) fn project(&self, event: &EventEnvelope) -> Vec<TelemetryRecord> {
        let Ok(mut turn) = self.turn.lock() else {
            return Vec::new();
        };
        match &event.event {
            EngineEvent::RequestSent {
                model,
                agent_profile,
                nb_context_chars,
                nb_context_messages,
                nb_prompt_chars,
                nb_images,
                supports_images,
                message_id,
            } => {
                turn.model.clone_from(model);
                turn.agent_profile.clone_from(agent_profile);
                turn.message_id.clone_from(message_id);
                turn.last_correlation_id.clone_from(&event.turn_id);
                vec![TelemetryRecord::RequestSent(records::RequestSent {
                    model: model.clone(),
                    nb_context_chars: *nb_context_chars,
                    nb_context_messages: *nb_context_messages,
                    nb_prompt_chars: *nb_prompt_chars,
                    // Every request this port makes is the turn's own. The
                    // summarization request is the reference's only secondary
                    // call, and `docs/parity.md` records that it carries no
                    // census here.
                    call_type: TelemetryCallType::MainCall,
                    message_id: message_id.clone(),
                    attachment_counts: attachment_counts(*nb_images as usize, *supports_images),
                })]
            }
            EngineEvent::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                turn.calls.insert(
                    call_id.clone(),
                    PendingToolCall {
                        name: name.clone(),
                        arguments: serde_json::from_str(arguments).unwrap_or(Value::Null),
                    },
                );
                Vec::new()
            }
            EngineEvent::ToolResult {
                call_id,
                content,
                typed_result,
                is_error,
                ..
            } => {
                let Some(call) = turn.calls.remove(call_id) else {
                    return Vec::new();
                };
                // Reference `_ask_approval`: a refusal is a `skip` verdict on an
                // `ask` permission, and the event it produces is `skipped`
                // rather than a failure. The refusal crosses the tool boundary
                // as the message the policy denied with, which is the only
                // thing that tells it from a tool that failed on its own.
                let declined = *is_error && content.starts_with(crate::policy::DENIAL_PREFIX);
                let status = match (declined, *is_error) {
                    (true, _) => records::TelemetryToolStatus::Skipped,
                    (false, true) => records::TelemetryToolStatus::Failure,
                    (false, false) => records::TelemetryToolStatus::Success,
                };
                let decision = declined.then_some(records::ToolDecision {
                    verdict: records::TelemetryToolVerdict::Skip,
                    approval_type: records::TelemetryApprovalType::Ask,
                });
                vec![TelemetryRecord::ToolCallFinished(
                    records::ToolCallFinished::new(records::ToolCallReport {
                        tool_name: &call.name,
                        status,
                        arguments: &call.arguments,
                        result: (!*is_error).then_some(typed_result),
                        decision,
                        agent_profile_name: &known(&turn.agent_profile),
                        model: &known(&turn.model),
                        message_id: turn.message_id.clone(),
                    }),
                )]
            }
            EngineEvent::CompactionOutcome {
                status,
                context_tokens_before,
                threshold,
                reason,
                ..
            } => {
                let compaction = TelemetryRecord::AutoCompactTriggered {
                    nb_context_tokens_before: *context_tokens_before,
                    auto_compact_threshold: *threshold,
                    status: status.label(),
                };
                match reason {
                    // The reference's failure record carries the classified
                    // reason and nothing else: no prompt, no transcript, no
                    // summary text.
                    Some(reason) => vec![
                        compaction,
                        TelemetryRecord::CompactionFailed {
                            reason: reason.label(),
                        },
                    ],
                    None => vec![compaction],
                }
            }
            // The variant this port emitted before the boundary pair existed.
            // Nothing emits it any more; a transcript that carries one still
            // reports the compaction it recorded, without a status it never
            // held.
            EngineEvent::Compaction { .. } => vec![TelemetryRecord::AutoCompactTriggered {
                nb_context_tokens_before: 0,
                auto_compact_threshold: 0,
                status: CompactionStatus::Success.label(),
            }],
            _ => Vec::new(),
        }
    }
}

impl<T> ClientTelemetry for TelemetryEventObserver<T>
where
    T: TelemetryTransport + 'static,
{
    /// Reference `TelemetryClient.send_telemetry_event`: the census first, the
    /// client's own properties second, and a correlation id only when the
    /// caller asked for one and a request has already been made.
    fn record_client_event(
        &self,
        name: &str,
        properties: Map<String, Value>,
        session_id: Option<&str>,
        correlate_last_request: bool,
    ) {
        let census = self.context.base_metadata(session_id).properties();
        self.deliver(TelemetryEnvelope::new(
            name,
            merge_properties(census, properties),
            self.correlation(correlate_last_request),
        ));
    }
}

impl<T> EventObserver for TelemetryEventObserver<T>
where
    T: TelemetryTransport + 'static,
{
    /// A projection failure never reaches the turn: an event whose label the
    /// validators refuse is dropped, on the same terms as a delivery that
    /// fails, because telemetry decides nothing about whether a caller runs.
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        for record in self.project(event) {
            let _ = self.queue(&record, Some(&event.session_id), None);
        }
        Ok(())
    }
}
