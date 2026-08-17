//! The `telemetry/send` extension notification an editor records through.

use serde_json::{Value, json};
use vibe_app_server::client::TurnDriver;
use vibe_core::observability::{LogLevel, log};

use crate::agent::AcpAgent;
use crate::protocol::{AcpError, AcpTelemetryNotification};
use crate::session::AcpHarness;

/// The two event names the reference routes off `telemetry/send`; every other
/// one is ignored with a warning.
const AT_MENTION_INSERTED_EVENT: &str = "vibe.at_mention_inserted";
const USER_RATING_FEEDBACK_EVENT: &str = "vibe.user_rating_feedback";

impl<D> AcpAgent<D>
where
    D: TurnDriver,
{
    /// Serves the `telemetry/send` extension notification.
    ///
    /// Reference `AcpAgent.ext_notification` (`vibe/acp/agent.py:1002-1031`):
    /// the payload is validated, a session the agent does not hold drops the
    /// event, two names are routed and every other one is ignored with a
    /// warning naming it. The rating carries the active model alias and
    /// correlates with the last request, which is what the reference reads off
    /// its own configuration and telemetry client.
    pub async fn telemetry_notification(&self, params: &Value) -> Result<(), AcpError> {
        let notification = serde_json::from_value::<AcpTelemetryNotification>(params.clone())
            .map_err(|error| {
                AcpError::InvalidParams(format!("invalid ACP telemetry notification: {error}"))
            })?;
        let Some(harness) = self.lock_state()?.active(&notification.session_id) else {
            return Ok(());
        };
        let (properties, correlate) = match notification.event.as_str() {
            AT_MENTION_INSERTED_EVENT => (notification.properties, false),
            USER_RATING_FEEDBACK_EVENT => {
                let rating = notification
                    .properties
                    .get("rating")
                    .cloned()
                    .unwrap_or_else(|| json!(0));
                let model = self
                    .active_model_alias(&harness, &notification.session_id)
                    .await;
                (
                    [
                        ("rating".to_owned(), rating),
                        ("model".to_owned(), json!(model)),
                    ]
                    .into_iter()
                    .collect(),
                    true,
                )
            }
            event => {
                log(
                    LogLevel::Warning,
                    &format!("Ignoring unsupported ACP telemetry event: {event}"),
                );
                return Ok(());
            }
        };
        harness.service.lock().await.public_call(
            "telemetry/record",
            json!({
                "sessionId": notification.session_id,
                "name": notification.event,
                "properties": properties,
                "correlateLastRequest": correlate,
            }),
        )?;
        Ok(())
    }

    /// The alias the session's configuration publishes for the model a turn
    /// would run on, which is what the reference reads as
    /// `config.current.active_model.alias`. A configuration that answers none
    /// reports the same `unknown` an absent label reports elsewhere.
    async fn active_model_alias(&self, harness: &AcpHarness<D>, session_id: &str) -> String {
        harness
            .service
            .lock()
            .await
            .public_call("config/read", json!({"sessionId": session_id}))
            .ok()
            .and_then(|result| {
                result
                    .get("config")?
                    .get("activeModel")?
                    .get("alias")?
                    .as_str()
                    .map(ToOwned::to_owned)
            })
            .filter(|alias| !alias.is_empty())
            .unwrap_or_else(|| "unknown".to_owned())
    }
}
