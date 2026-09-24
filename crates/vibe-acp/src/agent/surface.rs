//! What the client is told about a session besides its transcript: the modes
//! and options it can pick from, whether its workspace is trusted, and how
//! much of the context it has used.
//!
//! Reference `vibe/acp/utils.py` (`build_mode_state`, `build_model_config`,
//! `make_thinking_response`) and `VibeAcpAgent._send_usage_update`.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};
use vibe_app_server::client::TurnDriver;

use crate::agent::AcpAgent;
use crate::protocol::AcpError;
use crate::session::AcpHarness;

/// The thinking levels a model can run at, in the order they are offered.
const THINKING_LEVELS: [(&str, &str); 5] = [
    ("off", "Off"),
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("max", "Max"),
];

/// How long a new session waits before announcing its commands. Reference
/// `INITIAL_AVAILABLE_COMMANDS_DELAY_SECONDS`.
pub(crate) const INITIAL_COMMANDS_DELAY: std::time::Duration =
    std::time::Duration::from_millis(100);

impl<D> AcpAgent<D>
where
    D: TurnDriver + 'static,
{
    /// Calls an app-server method about `harness`, naming the session it
    /// runs under unless the parameters already name one.
    pub(crate) async fn call(
        &self,
        harness: &AcpHarness<D>,
        method: &str,
        mut params: Value,
    ) -> Result<BTreeMap<String, Value>, AcpError> {
        if let Some(object) = params.as_object_mut() {
            object
                .entry("sessionId")
                .or_insert_with(|| json!(harness.canonical_id()));
        }
        Ok(harness.service.lock().await.public_call(method, params)?)
    }

    /// [`Self::call`] for the methods the app server answers asynchronously,
    /// with the notifications they raised.
    pub(crate) async fn call_async(
        &self,
        harness: &AcpHarness<D>,
        method: &str,
        mut params: Value,
    ) -> Result<vibe_app_server::client::PublicDispatch, AcpError> {
        if let Some(object) = params.as_object_mut() {
            object
                .entry("sessionId")
                .or_insert_with(|| json!(harness.canonical_id()));
        }
        Ok(harness
            .service
            .lock()
            .await
            .public_call_async(method, params)
            .await?)
    }

    /// The modes a session offers and the matching selector. Reference
    /// `build_mode_state`: the primary agents, plus the active one when a
    /// rollout gate hides it.
    pub(crate) async fn mode_state(
        &self,
        harness: &AcpHarness<D>,
    ) -> Result<(Value, Value), AcpError> {
        let listing = self.call(harness, "agents/list", json!({})).await?;
        let active = listing.get("active").cloned().unwrap_or(Value::Null);
        let mut primary = listing
            .get("agents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|agent| agent.get("agentType").and_then(Value::as_str) == Some("agent"))
            .cloned()
            .collect::<Vec<_>>();
        let active_name = active.get("name").cloned().unwrap_or(Value::Null);
        if primary
            .iter()
            .all(|agent| agent.get("name") != Some(&active_name))
            && !active.is_null()
        {
            primary.push(active);
        }
        let field = |agent: &Value, key: &str| agent.get(key).cloned().unwrap_or(Value::Null);
        let modes = primary
            .iter()
            .map(|agent| {
                json!({
                    "id": field(agent, "name"),
                    "name": field(agent, "displayName"),
                    "description": field(agent, "description"),
                })
            })
            .collect::<Vec<_>>();
        let options = primary
            .iter()
            .map(|agent| {
                json!({
                    "value": field(agent, "name"),
                    "name": field(agent, "displayName"),
                    "description": field(agent, "description"),
                })
            })
            .collect::<Vec<_>>();
        Ok((
            json!({"currentModeId": active_name, "availableModes": modes}),
            json!({
                "id": "mode",
                "name": "Session mode",
                "currentValue": active_name,
                "category": "mode",
                "type": "select",
                "options": options,
            }),
        ))
    }

    /// Whether `mode_id` names one of the modes the session offers.
    pub(crate) async fn is_primary_mode(
        &self,
        harness: &AcpHarness<D>,
        mode_id: &str,
    ) -> Result<bool, AcpError> {
        let (modes, _) = self.mode_state(harness).await?;
        Ok(modes
            .get("availableModes")
            .and_then(Value::as_array)
            .is_some_and(|modes| {
                modes
                    .iter()
                    .any(|mode| mode.get("id").and_then(Value::as_str) == Some(mode_id))
            }))
    }

    /// The effective configuration view the session runs under.
    pub(crate) async fn config_view(&self, harness: &AcpHarness<D>) -> Result<Value, AcpError> {
        Ok(self
            .call(harness, "config/read", json!({}))
            .await?
            .remove("config")
            .unwrap_or(Value::Null))
    }

    /// The three selectors a session publishes: mode, model, and thinking.
    pub(crate) async fn config_options(
        &self,
        harness: &AcpHarness<D>,
    ) -> Result<Vec<Value>, AcpError> {
        let (_, mode) = self.mode_state(harness).await?;
        let config = self.config_view(harness).await?;
        let active = config.get("activeModel").cloned().unwrap_or(Value::Null);
        let field = |value: &Value, key: &str| value.get(key).cloned().unwrap_or(Value::Null);
        let models = config
            .get("models")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|model| {
                json!({
                    "value": field(model, "alias"),
                    "name": field(model, "displayName"),
                    "description": field(model, "name"),
                })
            })
            .collect::<Vec<_>>();
        let model = json!({
            "id": "model",
            "name": "Model",
            "currentValue": field(&active, "alias"),
            "category": "model",
            "type": "select",
            "options": models,
        });
        let thinking = json!({
            "id": "thinking",
            "name": "Thinking",
            "currentValue": field(&active, "thinking"),
            "category": "thinking",
            "type": "select",
            "options": THINKING_LEVELS
                .iter()
                .map(|(value, name)| json!({"value": value, "name": name}))
                .collect::<Vec<_>>(),
        });
        Ok(vec![mode, model, thinking])
    }

    /// The `_meta` a new or loaded session carries: its workspace trust.
    pub(crate) async fn trust_meta(
        &self,
        harness: &AcpHarness<D>,
        cwd: &str,
    ) -> Result<Value, AcpError> {
        let status = self
            .call(harness, "workspace/trust/status", json!({"cwd": cwd}))
            .await?;
        Ok(json!({
            "workspace_trust": {
                "status": status.get("status").cloned().unwrap_or(Value::Null),
                "details": status.get("details").cloned().unwrap_or(Value::Null),
            }
        }))
    }

    /// The token usage a prompt response reports: the session's totals.
    pub(crate) async fn usage(&self, harness: &AcpHarness<D>) -> Value {
        let stats = self.stats(harness).await.0;
        let number = |key: &str| stats.get(key).and_then(Value::as_u64).unwrap_or(0);
        let input = number("sessionPromptTokens");
        let output = number("sessionCompletionTokens");
        json!({
            "inputTokens": input,
            "outputTokens": output,
            "totalTokens": input.saturating_add(output),
        })
    }

    async fn stats(&self, harness: &AcpHarness<D>) -> (Value, u64) {
        match self.call(harness, "stats/read", json!({})).await {
            Ok(mut result) => (
                result.remove("stats").unwrap_or(Value::Null),
                result
                    .get("contextWindow")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            ),
            Err(_) => (Value::Null, 0),
        }
    }

    /// The `usage_update` the session's statistics read as now.
    pub(crate) async fn usage_update(&self, harness: &AcpHarness<D>) -> Value {
        let (stats, context_window) = self.stats(harness).await;
        let number = |key: &str| stats.get(key).and_then(Value::as_u64).unwrap_or(0);
        let float = |key: &str| stats.get(key).and_then(Value::as_f64).unwrap_or(0.0);
        let prompt = number("sessionPromptTokens");
        let completion = number("sessionCompletionTokens");
        let cached = number("sessionCachedTokens");
        let cost = session_cost(
            prompt,
            completion,
            cached,
            float("inputPricePerMillion"),
            float("outputPricePerMillion"),
            stats
                .get("cachedInputPricePerMillion")
                .and_then(Value::as_f64),
        );
        let mut update = json!({
            "sessionUpdate": "usage_update",
            "used": number("contextTokens"),
            "size": context_window,
            "_meta": {
                "steps": number("steps"),
                "promptTokens": prompt,
                "completionTokens": completion,
                "cachedTokens": cached,
                "totalTokens": prompt.saturating_add(completion),
                "tokensPerSecond": stats.get("tokensPerSecond").cloned().unwrap_or(json!(0.0)),
                "lastTurnDuration": stats.get("lastTurnDuration").cloned().unwrap_or(json!(0.0)),
                "lastTurnTotalTokens": number("lastTurnPromptTokens")
                    .saturating_add(number("lastTurnCompletionTokens")),
            },
        });
        if cost > 0.0 {
            update["cost"] = json!({"amount": cost, "currency": "USD"});
        }
        update
    }

    /// Announces the session's usage from a task of its own, the way the
    /// reference spawns it.
    pub(crate) fn send_usage_update(self: &Arc<Self>, harness: &Arc<AcpHarness<D>>) {
        let agent = Arc::clone(self);
        let session = Arc::clone(harness);
        let task = tokio::spawn(async move {
            let update = agent.usage_update(&session).await;
            agent.session_update(&session.session_id, update);
        });
        harness.track(task.abort_handle());
    }

    /// Re-announces the selectors after something the client did not ask for
    /// changed them.
    pub(crate) async fn send_config_options(
        &self,
        harness: &AcpHarness<D>,
    ) -> Result<(), AcpError> {
        let options = self.config_options(harness).await?;
        self.session_update(
            &harness.session_id,
            json!({"sessionUpdate": "config_option_update", "configOptions": options}),
        );
        Ok(())
    }
}

/// Reference `session_token_cost`: cached tokens, clamped to the prompt, are
/// billed at the cached price when the model declares one and at the input
/// price otherwise.
fn session_cost(
    prompt: u64,
    completion: u64,
    cached: u64,
    input_price: f64,
    output_price: f64,
    cached_price: Option<f64>,
) -> f64 {
    let as_float = |value: u64| u32::try_from(value).map_or(f64::from(u32::MAX), f64::from);
    let cached = cached.min(prompt);
    let cached_price = cached_price.unwrap_or(input_price);
    let input_cost = as_float(prompt - cached) * input_price + as_float(cached) * cached_price;
    let output_cost = as_float(completion) * output_price;
    (input_cost + output_cost) / 1_000_000.0
}
