//! The conversation middleware a session latches across its turns: the
//! engine is rebuilt for every turn, so a policy that speaks once per session
//! lives on the driver and each turn's engine borrows it.

use super::*;

impl LiveTurnDriver {
    /// The warning policy this session speaks through, created on its first
    /// turn and kept afterward.
    ///
    /// `context_warnings` decides whether the policy is registered at all, the
    /// way the reference's `_setup_middleware` does, rather than registering a
    /// silent one: an unregistered policy cannot latch, so turning the key off
    /// mid-session leaves nothing behind.
    pub(super) fn context_warning(
        &self,
        session_id: &str,
        settings: &CompactionSettings,
    ) -> Option<Arc<ContextWarningMiddleware>> {
        if !settings.context_warnings {
            return None;
        }
        let mut warnings = lock(&self.context_warnings);
        Some(Arc::clone(
            warnings.entry(session_id.to_owned()).or_default(),
        ))
    }

    /// The plan-agent reminder of this session, told which agent the turn
    /// runs and what the reminder names.
    pub(super) fn plan_agent(
        &self,
        reservation: &TurnReservation,
        tools: &[vibe_core::provider::ToolDefinition],
    ) -> Arc<PlanAgentMiddleware> {
        let middleware = Arc::clone(
            lock(&self.plan_agents)
                .entry(reservation.session_id.clone())
                .or_default(),
        );
        let published = |name: &str| tools.iter().any(|tool| tool.name == name);
        let plan_file = self
            .plan_directory()
            .map(|directory| plan_file_path(&directory, &reservation.session_id))
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        middleware.set_reminder(vibe_core::middleware::plan_agent_reminder(
            &plan_file,
            published("ask_user_question"),
            published("exit_plan_mode"),
        ));
        middleware.set_active(runs_plan_agent(reservation));
        middleware
    }
}

/// Whether the turn runs the plan agent, or the port's own plan mode.
pub(super) fn runs_plan_agent(reservation: &TurnReservation) -> bool {
    reservation.intent.agent.as_deref() == Some(PLAN_AGENT)
        || reservation.intent.mode.as_deref() == Some("plan")
}
