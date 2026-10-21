//! The policy in front of a tool: what a family registers its handlers
//! behind, so a call is decided by the session's store before it runs.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use super::store::granted_without_asking;
use super::{ApprovalAgent, PermissionContext, PermissionStore, PolicyError};
use crate::tools::config::ToolConfigResolver;
use crate::tools::{
    ToolApproval, ToolError, ToolExecutionOutput, ToolHandler, ToolInvocation, ToolOutputSink,
    ToolSkip,
};

/// What a tool family publishes its tools behind: the session's permission
/// store, the agent that answers an approval, and the configuration each tool
/// reads at every call.
///
/// The three travel together through every registration, and the resolver is
/// the store's own, so a family and the policy in front of it always read one
/// composition rather than two.
#[derive(Clone)]
pub struct ToolGuard {
    pub policy: PermissionStore,
    pub approval: Arc<dyn ApprovalAgent>,
    pub config: ToolConfigResolver,
    /// The session's own scratchpad, which every file tool grants outright.
    /// [`None`] for a session whose scratchpad could not be opened, where the
    /// chain simply never takes that branch.
    pub scratchpad: Option<PathBuf>,
}

impl ToolGuard {
    #[must_use]
    pub fn new(policy: PermissionStore, approval: Arc<dyn ApprovalAgent>) -> Self {
        let config = policy.tool_config();
        Self {
            policy,
            approval,
            config,
            scratchpad: None,
        }
    }

    /// The same guard whose file tools may always reach `scratchpad`.
    #[must_use]
    pub fn with_scratchpad(mut self, scratchpad: Option<PathBuf>) -> Self {
        self.scratchpad = scratchpad;
        self
    }
}

pub type RequirementResolver =
    dyn Fn(&ToolInvocation) -> Result<PermissionContext, ToolError> + Send + Sync;

pub struct PolicyGuardedTool {
    name: String,
    store: PermissionStore,
    approval: Arc<dyn ApprovalAgent>,
    requirements: Arc<RequirementResolver>,
    inner: Arc<dyn ToolHandler>,
}

impl PolicyGuardedTool {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        store: PermissionStore,
        approval: Arc<dyn ApprovalAgent>,
        requirements: Arc<RequirementResolver>,
        inner: Arc<dyn ToolHandler>,
    ) -> Self {
        Self {
            name: name.into(),
            store,
            approval,
            requirements,
            inner,
        }
    }

    /// The provenance of a call its own policy allows outright, which never
    /// reaches the store: the bypass when the agent bypasses the gate, and the
    /// configured permission otherwise (reference `_should_execute_tool`).
    #[must_use]
    pub fn allowed_outright(&self) -> ToolApproval {
        granted_without_asking(false, self.approval.attribution(&self.name))
    }
}

impl ToolHandler for PolicyGuardedTool {
    fn resolve_permission<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
    ) -> Option<crate::tools::PermissionFuture<'a>> {
        Some(Box::pin(async move {
            let context = (self.requirements)(invocation)
                .map_err(|error| PolicyError::Approval(error.to_string()))?;
            self.store.outcome(&self.name, &context).await
        }))
    }

    fn invoke<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
        output: ToolOutputSink,
    ) -> crate::tools::ToolHandlerFuture<'a> {
        let invocation = invocation.clone();
        let name = self.name.clone();
        let store = self.store.clone();
        let approval = self.approval.clone();
        let requirements = self.requirements.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            if let Some(approval) = crate::tools::gated_approval() {
                crate::tools::mark_call_started(approval);
                return inner.invoke(&invocation, output).await.map(|mut result| {
                    result.approval.get_or_insert(approval);
                    result
                });
            }
            let mut context = requirements(&invocation)?;
            context.call_id = Some(invocation.call_id.clone()).filter(|id| !id.is_empty());
            let lease = match store
                .authorize(
                    &name,
                    invocation.arguments.clone(),
                    context,
                    approval.as_ref(),
                )
                .await
            {
                Ok(lease) => lease,
                Err(PolicyError::Skipped { reason, cancelled }) => {
                    let mut output = ToolExecutionOutput::text(reason);
                    output.typed_result = Value::Null;
                    output.skip = Some(ToolSkip { cancelled });
                    output.approval = Some(ToolApproval::declined());
                    return Ok(output);
                }
                Err(PolicyError::TurnFailed(message)) => {
                    return Err(ToolError::TurnFailed(message));
                }
                Err(error) => return Err(ToolError::Execution(error.to_string())),
            };
            // The read guard is held across the side effect on purpose: it is
            // what makes revocation atomic. `revoke_trust` needs the write lock,
            // so it cannot land between this revalidation and the effect it
            // authorizes, and any waiting revocation applies to the next call.
            let _frozen = lease
                .revalidated()
                .await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            let approval = lease.approval();
            crate::tools::mark_call_started(approval);
            match inner.invoke(&invocation, output).await {
                Ok(mut result) => {
                    result.approval.get_or_insert(approval);
                    Ok(result)
                }
                Err(source) => Err(ToolError::Approved {
                    approval,
                    source: Box::new(source),
                }),
            }
        })
    }
}
