//! The conversation policy layer the engine consults before it builds a request.
//!
//! Every policy the reference expresses as a middleware answers the same
//! question at the same moment: given the transcript, the accumulated stats and
//! the compaction settings, may this cycle spend a request, and does anything
//! have to be said or done first. The pipeline resolves the answers of several
//! policies into one, and the resolution is load-bearing rather than cosmetic:
//! a conversation that reaches its turn limit and its compaction threshold in
//! the same cycle stops, because the limit is registered first.
//!
//! Reference: `vibe/core/middleware.py` at the pinned commit, and
//! `vibe/core/agent_loop/_loop.py::_setup_middleware` for the registration
//! order.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::engine::{EngineLimits, SessionStats, TurnStopReason};
use crate::events::ModelMessage;

#[cfg(test)]
mod middleware_tests;

/// What a policy asks the engine to do before the next request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MiddlewareAction {
    /// Nothing to do: the cycle proceeds.
    #[default]
    Continue,
    /// End the turn without building a request.
    Stop,
    /// Compact the transcript before building a request.
    Compact,
    /// Add a message to the transcript, then proceed.
    InjectMessage,
}

/// Why a pipeline's latched state is being cleared.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResetReason {
    /// A turn ended.
    #[default]
    Stop,
    /// The transcript was compacted, so anything measured against it is stale.
    Compact,
}

/// The identifier of the built-in compaction request, which is what
/// `compaction_prompt_id` defaults to.
pub const DEFAULT_COMPACTION_PROMPT_ID: &str = "compact";

/// The compaction policy a middleware reads.
///
/// It is carried on the conversation context rather than read from a global so
/// that a policy stays a total function of what it is handed. The five fields
/// are the five configuration keys the reference declares for compaction, read
/// once per session by [`crate::config::ConfigSnapshot::compaction_settings`].
///
/// [`Default`] is the "nothing configured" answer rather than the shipped
/// defaults: a stack composed without a configuration compacts on no threshold,
/// which is what the engine's own tests run under. The published defaults live
/// in the field registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionSettings {
    /// The context size at or above which compaction fires. Zero disables it.
    pub auto_compact_threshold: u64,
    /// The model a compaction request is sent to, already resolved the way
    /// `get_compaction_model` resolves it: the configured `compaction_model`,
    /// and otherwise the active model. `None` means no configuration named
    /// either, so the provider's own model answers.
    pub compaction_model: Option<String>,
    /// The identifier of the prompt the compaction request is built from.
    pub compaction_prompt_id: String,
    /// Whether the conversation is warned once it approaches the threshold.
    pub context_warnings: bool,
    /// Whether a compaction failure fails the turn instead of degrading. It
    /// also refuses the reactive recovery, which is what `_should_self_heal`
    /// consults.
    pub raise_on_compaction_failure: bool,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            auto_compact_threshold: 0,
            compaction_model: None,
            compaction_prompt_id: DEFAULT_COMPACTION_PROMPT_ID.to_owned(),
            context_warnings: false,
            raise_on_compaction_failure: false,
        }
    }
}

/// Everything a policy is allowed to read.
#[derive(Debug, Clone, Copy)]
pub struct ConversationContext<'a> {
    pub messages: &'a [ModelMessage],
    pub stats: &'a SessionStats,
    /// The session cost so far. The port accounts price outside [`SessionStats`],
    /// where the reference keeps it on its own stats object.
    pub price_micros: u64,
    pub compaction: &'a CompactionSettings,
}

/// One policy's answer.
///
/// `stop_reason` is the typed counterpart of the reference's untyped `metadata`
/// dictionary: this port publishes [`TurnStopReason`] on the wire, so the
/// mapping cannot be recovered from the free-text `reason` after the fact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MiddlewareResult {
    pub action: MiddlewareAction,
    pub message: Option<String>,
    pub reason: Option<String>,
    pub stop_reason: Option<TurnStopReason>,
}

impl MiddlewareResult {
    /// The answer of a policy that has nothing to say.
    #[must_use]
    pub fn proceed() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn stop(reason: impl Into<String>, stop_reason: TurnStopReason) -> Self {
        Self {
            action: MiddlewareAction::Stop,
            message: None,
            reason: Some(reason.into()),
            stop_reason: Some(stop_reason),
        }
    }

    #[must_use]
    pub fn compact() -> Self {
        Self {
            action: MiddlewareAction::Compact,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn inject(message: impl Into<String>) -> Self {
        Self {
            action: MiddlewareAction::InjectMessage,
            message: Some(message.into()),
            reason: None,
            stop_reason: None,
        }
    }
}

/// A conversation policy.
///
/// `before_turn` takes `&self` because a pipeline outlives the turns it runs
/// against: a policy that latches, such as a once-per-session warning, keeps
/// its latch until [`ConversationMiddleware::reset`] clears it.
pub trait ConversationMiddleware: Send + Sync {
    fn before_turn(&self, context: &ConversationContext<'_>) -> MiddlewareResult;

    /// Clears whatever the policy latched. A stateless policy ignores it.
    fn reset(&self, _reset_reason: ResetReason) {}
}

/// Stops the turn once the step budget is spent.
///
/// The comparison is this port's, not the reference's: `steps >= max_turns`
/// where the reference writes `steps - 1 >= max_turns`, because the port counts
/// a step on completion and the reference counts it on entry. The observable
/// stop reason is the same, and the existing engine tests pin it.
#[derive(Debug, Clone, Copy)]
pub struct TurnLimitMiddleware {
    max_turns: u32,
}

impl TurnLimitMiddleware {
    #[must_use]
    pub fn new(max_turns: u32) -> Self {
        Self { max_turns }
    }
}

impl ConversationMiddleware for TurnLimitMiddleware {
    fn before_turn(&self, context: &ConversationContext<'_>) -> MiddlewareResult {
        if context.stats.steps >= self.max_turns {
            return MiddlewareResult::stop(
                format!("turn limit of {} reached", self.max_turns),
                TurnStopReason::MaxSteps,
            );
        }
        MiddlewareResult::proceed()
    }
}

/// Stops the turn once the session costs more than the operator allowed.
#[derive(Debug, Clone, Copy)]
pub struct PriceLimitMiddleware {
    max_price_micros: u64,
}

impl PriceLimitMiddleware {
    #[must_use]
    pub fn new(max_price_micros: u64) -> Self {
        Self { max_price_micros }
    }
}

impl ConversationMiddleware for PriceLimitMiddleware {
    fn before_turn(&self, context: &ConversationContext<'_>) -> MiddlewareResult {
        if context.price_micros > self.max_price_micros {
            return MiddlewareResult::stop(
                format!(
                    "price limit exceeded: {} > {} micros",
                    context.price_micros, self.max_price_micros
                ),
                TurnStopReason::PriceLimit,
            );
        }
        MiddlewareResult::proceed()
    }
}

/// Stops the turn once the session spent more tokens than the operator allowed.
#[derive(Debug, Clone, Copy)]
pub struct TokenLimitMiddleware {
    max_total_tokens: u64,
}

impl TokenLimitMiddleware {
    #[must_use]
    pub fn new(max_total_tokens: u64) -> Self {
        Self { max_total_tokens }
    }
}

impl ConversationMiddleware for TokenLimitMiddleware {
    fn before_turn(&self, context: &ConversationContext<'_>) -> MiddlewareResult {
        let spent = context
            .stats
            .usage
            .input_tokens
            .saturating_add(context.stats.usage.output_tokens);
        if spent > self.max_total_tokens {
            return MiddlewareResult::stop(
                format!("token limit exceeded: {spent} > {}", self.max_total_tokens),
                TurnStopReason::TokenLimit,
            );
        }
        MiddlewareResult::proceed()
    }
}

/// Compacts the transcript once it reaches the configured threshold.
///
/// The policy is stateless: it reads the same context size the engine wrote
/// from the last completion's usage, so the answer is a function of the ledger
/// rather than of anything the policy latched. A threshold of zero disables it,
/// which is the only way the reference turns automatic compaction off; the
/// reference tests `threshold > 0` on a signed integer, and a negative value
/// reaches this port as zero because the configuration reader cannot represent
/// one.
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoCompactMiddleware;

impl ConversationMiddleware for AutoCompactMiddleware {
    fn before_turn(&self, context: &ConversationContext<'_>) -> MiddlewareResult {
        let threshold = context.compaction.auto_compact_threshold;
        if threshold > 0 && context.stats.context_tokens >= threshold {
            return MiddlewareResult::compact();
        }
        MiddlewareResult::proceed()
    }
}

/// The registered policies, in the order the engine polls them.
#[derive(Clone, Default)]
pub struct MiddlewarePipeline {
    middlewares: Vec<Arc<dyn ConversationMiddleware>>,
}

impl std::fmt::Debug for MiddlewarePipeline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MiddlewarePipeline")
            .field("middlewares", &self.middlewares.len())
            .finish()
    }
}

impl MiddlewarePipeline {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the budget policies declared by `limits`, in the reference's
    /// registration order: turns, then price, then tokens.
    ///
    /// A policy is registered when its limit exists, which is what the
    /// reference's `is not None` tests. This port collapses an absent price
    /// limit to `u64::MAX`, so that value, and only that value, means absent;
    /// the step and token limits have no absent representation and a zero there
    /// is a real limit that stops the turn at once.
    #[must_use]
    pub fn from_limits(limits: &EngineLimits) -> Self {
        let mut pipeline = Self::new();
        pipeline.add(Arc::new(TurnLimitMiddleware::new(limits.max_steps)));
        if limits.max_price_micros != u64::MAX {
            pipeline.add(Arc::new(PriceLimitMiddleware::new(limits.max_price_micros)));
        }
        pipeline.add(Arc::new(TokenLimitMiddleware::new(limits.max_total_tokens)));
        pipeline
    }

    pub fn add(&mut self, middleware: Arc<dyn ConversationMiddleware>) -> &mut Self {
        self.middlewares.push(middleware);
        self
    }

    pub fn clear(&mut self) {
        self.middlewares.clear();
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.middlewares.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.middlewares.is_empty()
    }

    /// Clears every registered policy's latched state.
    pub fn reset(&self, reset_reason: ResetReason) {
        for middleware in &self.middlewares {
            middleware.reset(reset_reason);
        }
    }

    /// Resolves every policy's answer into one.
    ///
    /// The first `Stop` or `Compact` wins and the policies after it are never
    /// polled, which is what makes registration order a precedence order. Every
    /// message injected along the way is joined by exactly two newlines, in
    /// registration order. An injection carrying no message is a policy that
    /// declined to speak, so it neither aggregates nor short-circuits.
    #[must_use]
    pub fn before_turn(&self, context: &ConversationContext<'_>) -> MiddlewareResult {
        let mut injected: Vec<String> = Vec::new();
        for middleware in &self.middlewares {
            let result = middleware.before_turn(context);
            match result.action {
                MiddlewareAction::InjectMessage => {
                    if let Some(message) = result.message.filter(|message| !message.is_empty()) {
                        injected.push(message);
                    }
                }
                MiddlewareAction::Stop | MiddlewareAction::Compact => return result,
                MiddlewareAction::Continue => {}
            }
        }
        if injected.is_empty() {
            return MiddlewareResult::proceed();
        }
        MiddlewareResult::inject(injected.join("\n\n"))
    }
}
