use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::provider::Usage;

/// A policy that answers whatever it was built with, and records every reset it
/// was told about, so a test can prove both what the pipeline resolved and what
/// it never polled.
struct Scripted {
    answer: MiddlewareResult,
    polls: AtomicUsize,
    resets: Mutex<Vec<ResetReason>>,
}

impl Scripted {
    fn new(answer: MiddlewareResult) -> Arc<Self> {
        Arc::new(Self {
            answer,
            polls: AtomicUsize::new(0),
            resets: Mutex::new(Vec::new()),
        })
    }

    fn polls(&self) -> usize {
        self.polls.load(Ordering::SeqCst)
    }

    fn resets(&self) -> Vec<ResetReason> {
        self.resets
            .lock()
            .expect("reset log is not poisoned")
            .clone()
    }
}

impl ConversationMiddleware for Scripted {
    fn before_turn(&self, _context: &ConversationContext<'_>) -> MiddlewareResult {
        self.polls.fetch_add(1, Ordering::SeqCst);
        self.answer.clone()
    }

    fn reset(&self, reset_reason: ResetReason) {
        self.resets
            .lock()
            .expect("reset log is not poisoned")
            .push(reset_reason);
    }
}

fn stats(steps: u32, input_tokens: u64, output_tokens: u64, context_tokens: u64) -> SessionStats {
    SessionStats {
        usage: Usage {
            input_tokens,
            output_tokens,
        },
        context_tokens,
        steps,
    }
}

fn context<'a>(
    stats: &'a SessionStats,
    price_micros: u64,
    compaction: &'a CompactionSettings,
) -> ConversationContext<'a> {
    ConversationContext {
        messages: &[],
        stats,
        price_micros,
        compaction,
    }
}

#[test]
fn the_vocabulary_declares_exactly_the_reference_variants() {
    // The action set and the reset-reason set are the contract every later
    // policy is written against, so their shape is pinned rather than implied.
    let actions = [
        MiddlewareAction::Continue,
        MiddlewareAction::Stop,
        MiddlewareAction::Compact,
        MiddlewareAction::InjectMessage,
    ];
    assert_eq!(actions.len(), 4);
    assert_eq!(MiddlewareAction::default(), MiddlewareAction::Continue);

    let reasons = [ResetReason::Stop, ResetReason::Compact];
    assert_eq!(reasons.len(), 2);
    assert_eq!(ResetReason::default(), ResetReason::Stop);

    let compaction = CompactionSettings {
        auto_compact_threshold: 120_000,
        ..CompactionSettings::default()
    };
    let carried = stats(2, 10, 5, 15);
    let context = ConversationContext {
        messages: &[ModelMessage::user("hello".to_owned())],
        stats: &carried,
        price_micros: 7,
        compaction: &compaction,
    };
    assert_eq!(context.messages.len(), 1);
    assert_eq!(context.stats.context_tokens, 15);
    assert_eq!(context.compaction.auto_compact_threshold, 120_000);

    let result = MiddlewareResult::stop("spent", TurnStopReason::PriceLimit);
    assert_eq!(result.action, MiddlewareAction::Stop);
    assert_eq!(result.message, None);
    assert_eq!(result.reason.as_deref(), Some("spent"));
    assert_eq!(result.stop_reason, Some(TurnStopReason::PriceLimit));
}

#[test]
fn a_stop_short_circuits_the_policies_after_it() {
    let first = Scripted::new(MiddlewareResult::proceed());
    let second = Scripted::new(MiddlewareResult::stop(
        "budget is spent",
        TurnStopReason::TokenLimit,
    ));
    let third = Scripted::new(MiddlewareResult::inject("never seen"));
    let mut pipeline = MiddlewarePipeline::new();
    pipeline
        .add(Arc::clone(&first) as Arc<dyn ConversationMiddleware>)
        .add(Arc::clone(&second) as Arc<dyn ConversationMiddleware>)
        .add(Arc::clone(&third) as Arc<dyn ConversationMiddleware>);

    let carried = stats(0, 0, 0, 0);
    let compaction = CompactionSettings::default();
    let result = pipeline.before_turn(&context(&carried, 0, &compaction));

    assert_eq!(result.action, MiddlewareAction::Stop);
    assert_eq!(result.reason.as_deref(), Some("budget is spent"));
    assert_eq!(result.stop_reason, Some(TurnStopReason::TokenLimit));
    assert_eq!(third.polls(), 0, "the policy after a stop is never polled");
}

#[test]
fn a_compact_short_circuits_the_policies_after_it() {
    let first = Scripted::new(MiddlewareResult::proceed());
    let second = Scripted::new(MiddlewareResult::compact());
    let third = Scripted::new(MiddlewareResult::inject("never seen"));
    let mut pipeline = MiddlewarePipeline::new();
    pipeline
        .add(Arc::clone(&first) as Arc<dyn ConversationMiddleware>)
        .add(Arc::clone(&second) as Arc<dyn ConversationMiddleware>)
        .add(Arc::clone(&third) as Arc<dyn ConversationMiddleware>);

    let carried = stats(0, 0, 0, 0);
    let compaction = CompactionSettings::default();
    let result = pipeline.before_turn(&context(&carried, 0, &compaction));

    assert_eq!(result.action, MiddlewareAction::Compact);
    assert_eq!(result.message, None);
    assert_eq!(
        third.polls(),
        0,
        "the policy after a compact is never polled"
    );
}

#[test]
fn injections_join_in_registration_order_with_two_newlines() {
    let mut pipeline = MiddlewarePipeline::new();
    pipeline
        .add(Scripted::new(MiddlewareResult::inject("first")) as Arc<dyn ConversationMiddleware>)
        .add(Scripted::new(MiddlewareResult::proceed()) as Arc<dyn ConversationMiddleware>)
        .add(Scripted::new(MiddlewareResult::inject("third")) as Arc<dyn ConversationMiddleware>);

    let carried = stats(0, 0, 0, 0);
    let compaction = CompactionSettings::default();
    let result = pipeline.before_turn(&context(&carried, 0, &compaction));

    assert_eq!(result.action, MiddlewareAction::InjectMessage);
    assert_eq!(result.message.as_deref(), Some("first\n\nthird"));
    assert_eq!(result.stop_reason, None);
}

#[test]
fn an_all_continue_pipeline_resolves_to_continue_with_no_message() {
    let mut pipeline = MiddlewarePipeline::new();
    pipeline
        .add(Scripted::new(MiddlewareResult::proceed()) as Arc<dyn ConversationMiddleware>)
        .add(Scripted::new(MiddlewareResult::proceed()) as Arc<dyn ConversationMiddleware>);

    let carried = stats(0, 0, 0, 0);
    let compaction = CompactionSettings::default();
    let result = pipeline.before_turn(&context(&carried, 0, &compaction));

    assert_eq!(result.action, MiddlewareAction::Continue);
    assert_eq!(result.message, None);
    assert_eq!(result.reason, None);
}

#[test]
fn an_injection_carrying_no_message_neither_aggregates_nor_short_circuits() {
    let last = Scripted::new(MiddlewareResult::inject("spoken"));
    let mut pipeline = MiddlewarePipeline::new();
    pipeline
        .add(Scripted::new(MiddlewareResult {
            action: MiddlewareAction::InjectMessage,
            ..MiddlewareResult::default()
        }) as Arc<dyn ConversationMiddleware>)
        .add(Arc::clone(&last) as Arc<dyn ConversationMiddleware>);

    let carried = stats(0, 0, 0, 0);
    let compaction = CompactionSettings::default();
    let result = pipeline.before_turn(&context(&carried, 0, &compaction));

    assert_eq!(result.message.as_deref(), Some("spoken"));
    assert_eq!(last.polls(), 1);
}

#[test]
fn every_registered_policy_receives_the_reset_reason() {
    let first = Scripted::new(MiddlewareResult::proceed());
    let second = Scripted::new(MiddlewareResult::proceed());
    let mut pipeline = MiddlewarePipeline::new();
    pipeline
        .add(Arc::clone(&first) as Arc<dyn ConversationMiddleware>)
        .add(Arc::clone(&second) as Arc<dyn ConversationMiddleware>);

    pipeline.reset(ResetReason::Compact);
    pipeline.reset(ResetReason::Stop);

    assert_eq!(
        first.resets(),
        vec![ResetReason::Compact, ResetReason::Stop]
    );
    assert_eq!(
        second.resets(),
        vec![ResetReason::Compact, ResetReason::Stop]
    );

    pipeline.clear();
    assert!(pipeline.is_empty());
    pipeline.reset(ResetReason::Stop);
    assert_eq!(
        first.resets().len(),
        2,
        "a cleared pipeline no longer reaches the policy"
    );
}

#[test]
fn each_limit_policy_names_its_public_stop_reason() {
    let compaction = CompactionSettings::default();

    let at_the_step_limit = stats(3, 0, 0, 0);
    let turns =
        TurnLimitMiddleware::new(3).before_turn(&context(&at_the_step_limit, 0, &compaction));
    assert_eq!(turns.action, MiddlewareAction::Stop);
    assert_eq!(turns.stop_reason, Some(TurnStopReason::MaxSteps));
    let below = stats(2, 0, 0, 0);
    assert_eq!(
        TurnLimitMiddleware::new(3)
            .before_turn(&context(&below, 0, &compaction))
            .action,
        MiddlewareAction::Continue
    );

    let spent = stats(0, 3, 2, 0);
    let tokens = TokenLimitMiddleware::new(4).before_turn(&context(&spent, 0, &compaction));
    assert_eq!(tokens.action, MiddlewareAction::Stop);
    assert_eq!(tokens.stop_reason, Some(TurnStopReason::TokenLimit));
    assert_eq!(
        TokenLimitMiddleware::new(5)
            .before_turn(&context(&spent, 0, &compaction))
            .action,
        MiddlewareAction::Continue,
        "the token limit stops strictly above the allowance"
    );

    let idle = stats(0, 0, 0, 0);
    let price = PriceLimitMiddleware::new(4).before_turn(&context(&idle, 5, &compaction));
    assert_eq!(price.action, MiddlewareAction::Stop);
    assert_eq!(price.stop_reason, Some(TurnStopReason::PriceLimit));
    assert_eq!(
        PriceLimitMiddleware::new(5)
            .before_turn(&context(&idle, 5, &compaction))
            .action,
        MiddlewareAction::Continue,
        "the price limit stops strictly above the allowance"
    );
}

#[test]
fn the_turn_limit_precedes_compaction_in_registration_order() {
    // The reference registers the limits before automatic compaction, so a
    // conversation that reaches both in the same cycle stops.
    let mut pipeline = MiddlewarePipeline::from_limits(&EngineLimits {
        max_steps: 1,
        ..EngineLimits::default()
    });
    pipeline.add(Scripted::new(MiddlewareResult::compact()) as Arc<dyn ConversationMiddleware>);

    let at_the_limit = stats(1, 0, 0, 500_000);
    let compaction = CompactionSettings {
        auto_compact_threshold: 1,
        ..CompactionSettings::default()
    };
    let result = pipeline.before_turn(&context(&at_the_limit, 0, &compaction));

    assert_eq!(result.action, MiddlewareAction::Stop);
    assert_eq!(result.stop_reason, Some(TurnStopReason::MaxSteps));
}

#[test]
fn an_absent_price_limit_registers_no_policy() {
    let without = MiddlewarePipeline::from_limits(&EngineLimits::default());
    assert_eq!(
        without.len(),
        2,
        "the default price limit is absent, so only turns and tokens register"
    );

    let with = MiddlewarePipeline::from_limits(&EngineLimits {
        max_price_micros: 500,
        ..EngineLimits::default()
    });
    assert_eq!(with.len(), 3);

    let idle = stats(0, 0, 0, 0);
    let compaction = CompactionSettings::default();
    assert_eq!(
        without
            .before_turn(&context(&idle, u64::MAX, &compaction))
            .action,
        MiddlewareAction::Continue,
        "no registered price policy means no price stop at any cost"
    );
}

/// US-149: `AutoCompactMiddleware` compares the threshold the configuration
/// declared against the context size the last completion wrote.
#[test]
fn auto_compaction_fires_at_or_above_a_positive_threshold() {
    let settings = |auto_compact_threshold| CompactionSettings {
        auto_compact_threshold,
        ..CompactionSettings::default()
    };
    let policy = AutoCompactMiddleware;

    let at = settings(120_000);
    assert_eq!(
        policy
            .before_turn(&context(&stats(1, 0, 0, 120_000), 0, &at))
            .action,
        MiddlewareAction::Compact,
        "a context at the threshold compacts"
    );
    assert_eq!(
        policy
            .before_turn(&context(&stats(1, 0, 0, 130_000), 0, &at))
            .action,
        MiddlewareAction::Compact,
        "a context above the threshold compacts"
    );
    assert_eq!(
        policy
            .before_turn(&context(&stats(1, 0, 0, 119_999), 0, &at))
            .action,
        MiddlewareAction::Continue,
        "a context below the threshold proceeds"
    );

    // `auto_compact_threshold = 0` is how the reference disables automatic
    // compaction, whatever the context size.
    let disabled = settings(0);
    assert_eq!(
        policy
            .before_turn(&context(&stats(1, 0, 0, u64::MAX), 0, &disabled))
            .action,
        MiddlewareAction::Continue,
        "a threshold of zero disables the policy"
    );
}

/// US-149: the limits are registered first, so a cycle that reaches a limit and
/// the threshold at once stops rather than compacting.
#[test]
fn a_limit_and_the_threshold_in_one_cycle_resolve_to_stop() {
    let mut pipeline = MiddlewarePipeline::from_limits(&EngineLimits {
        max_steps: 1,
        ..EngineLimits::default()
    });
    pipeline.add(Arc::new(AutoCompactMiddleware));
    let compaction = CompactionSettings {
        auto_compact_threshold: 1,
        ..CompactionSettings::default()
    };

    let both = pipeline.before_turn(&context(&stats(1, 0, 0, 500_000), 0, &compaction));
    assert_eq!(both.action, MiddlewareAction::Stop);
    assert_eq!(both.stop_reason, Some(TurnStopReason::MaxSteps));

    // Under the limit, the same cycle compacts, which is what proves the stop
    // above came from precedence rather than from the policy never firing.
    let only_threshold = pipeline.before_turn(&context(&stats(0, 0, 0, 500_000), 0, &compaction));
    assert_eq!(only_threshold.action, MiddlewareAction::Compact);
}

/// US-157: the warning fires once, at half the window, and names the three
/// quantities the reference's message names.
#[test]
fn the_context_warning_fires_once_at_half_the_window() {
    let window = CompactionSettings {
        auto_compact_threshold: 200_000,
        context_warnings: true,
        ..CompactionSettings::default()
    };
    let policy = ContextWarningMiddleware::default();

    assert_eq!(
        policy
            .before_turn(&context(&stats(1, 0, 0, 99_999), 0, &window))
            .action,
        MiddlewareAction::Continue,
        "a context below half the window says nothing"
    );

    let warning = policy.before_turn(&context(&stats(1, 0, 0, 100_000), 0, &window));
    assert_eq!(warning.action, MiddlewareAction::InjectMessage);
    let message = warning.message.expect("the warning carries its message");
    assert!(
        message.starts_with("<vibe_warning>") && message.ends_with("</vibe_warning>"),
        "the message is wrapped in the reference's warning tag: {message}"
    );
    assert!(
        message.contains("50%") && message.contains("100000") && message.contains("200000"),
        "the message names the percentage, the current tokens and the window: {message}"
    );

    assert_eq!(
        policy
            .before_turn(&context(&stats(2, 0, 0, 180_000), 0, &window))
            .action,
        MiddlewareAction::Continue,
        "the latch holds, so a session is warned once"
    );
}

/// US-157: a window of zero silences the policy, and only the compaction reset
/// clears its latch, because this port also resets at the top of every turn.
#[test]
fn the_context_warning_is_silent_without_a_window_and_relatches_on_a_compaction() {
    let policy = ContextWarningMiddleware::default();
    let disabled = CompactionSettings {
        auto_compact_threshold: 0,
        context_warnings: true,
        ..CompactionSettings::default()
    };
    assert_eq!(
        policy
            .before_turn(&context(&stats(1, 0, 0, u64::MAX), 0, &disabled))
            .action,
        MiddlewareAction::Continue,
        "no window means nothing to warn about"
    );

    let window = CompactionSettings {
        auto_compact_threshold: 1_000,
        context_warnings: true,
        ..CompactionSettings::default()
    };
    assert_eq!(
        policy
            .before_turn(&context(&stats(1, 0, 0, 900), 0, &window))
            .action,
        MiddlewareAction::InjectMessage
    );

    policy.reset(ResetReason::Stop);
    assert_eq!(
        policy
            .before_turn(&context(&stats(2, 0, 0, 900), 0, &window))
            .action,
        MiddlewareAction::Continue,
        "a turn boundary does not re-arm the warning"
    );

    policy.reset(ResetReason::Compact);
    assert_eq!(
        policy
            .before_turn(&context(&stats(3, 0, 0, 900), 0, &window))
            .action,
        MiddlewareAction::InjectMessage,
        "a compaction re-arms the warning, because the window it measured was replaced"
    );
}

/// US-157: an injecting policy that fires alongside another leaves one
/// injection joined by two newlines, in registration order.
#[test]
fn the_context_warning_aggregates_with_another_injection() {
    let mut pipeline = MiddlewarePipeline::new();
    pipeline.add(Arc::new(ContextWarningMiddleware::default()));
    pipeline.add(Scripted::new(MiddlewareResult::inject("second")));
    let window = CompactionSettings {
        auto_compact_threshold: 100,
        context_warnings: true,
        ..CompactionSettings::default()
    };

    let combined = pipeline.before_turn(&context(&stats(1, 0, 0, 100), 0, &window));
    assert_eq!(combined.action, MiddlewareAction::InjectMessage);
    let message = combined.message.expect("the injection carries its message");
    let parts: Vec<&str> = message.split("\n\n").collect();
    assert_eq!(parts.len(), 2, "exactly two runs joined: {message}");
    assert!(parts[0].starts_with("<vibe_warning>"));
    assert_eq!(parts[1], "second");
}
