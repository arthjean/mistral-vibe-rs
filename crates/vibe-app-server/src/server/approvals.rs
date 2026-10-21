//! The approval agents and session tool factories a server falls back on
//! when its host installs none.

use super::*;

pub(super) struct DenyApproval;

impl ApprovalAgent for DenyApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::Deny) })
    }
}

pub(super) struct ApproveOnce;

impl ApprovalAgent for ApproveOnce {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
    }

    /// Reference `bypass_tool_permissions`: an auto-approving session never
    /// consults the gate at all.
    fn attribution(&self, _tool: &str) -> vibe_core::tools::ToolApprovalSource {
        vibe_core::tools::ToolApprovalSource::Bypass
    }
}

pub trait ApprovalAgentFactory: Send + Sync {
    fn for_session(&self, session_id: &str, auto_approve: bool) -> Arc<dyn ApprovalAgent>;

    fn for_agent(
        &self,
        session_id: &str,
        approval: AgentApproval,
        auto_approve: bool,
    ) -> Arc<dyn ApprovalAgent> {
        let fallback = self.for_session(session_id, auto_approve || approval == AgentApproval::All);
        match approval {
            AgentApproval::Edits => Arc::new(ApproveEdits { fallback }),
            AgentApproval::Prompt | AgentApproval::All => fallback,
        }
    }
}

pub(super) struct ApproveEdits {
    fallback: Arc<dyn ApprovalAgent>,
}

impl ApprovalAgent for ApproveEdits {
    fn request<'a>(&'a self, request: ApprovalRequest) -> ApprovalFuture<'a> {
        if is_edit_tool(&request.tool) {
            Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
        } else {
            self.fallback.request(request)
        }
    }

    /// The reference's `accept-edits` profile sets the edit tools' permission
    /// to `always`, so an edit it lets through is the configuration's call.
    fn attribution(&self, tool: &str) -> vibe_core::tools::ToolApprovalSource {
        if is_edit_tool(tool) {
            vibe_core::tools::ToolApprovalSource::Config
        } else {
            self.fallback.attribution(tool)
        }
    }
}

fn is_edit_tool(tool: &str) -> bool {
    matches!(tool, "edit" | "write" | "write_file")
}

pub trait SessionToolFactory: Send + Sync {
    fn register(&self, session_id: &str, tools: &ToolRegistry) -> Result<(), String>;
}

pub(super) struct NoAdditionalTools;

impl SessionToolFactory for NoAdditionalTools {
    fn register(&self, _session_id: &str, _tools: &ToolRegistry) -> Result<(), String> {
        Ok(())
    }
}

pub(super) struct ChainedSessionToolFactory {
    pub(super) existing: Arc<dyn SessionToolFactory>,
    pub(super) additional: Arc<dyn SessionToolFactory>,
}

impl SessionToolFactory for ChainedSessionToolFactory {
    fn register(&self, session_id: &str, tools: &ToolRegistry) -> Result<(), String> {
        self.existing.register(session_id, tools)?;
        self.additional.register(session_id, tools)
    }
}

pub(super) struct DefaultApprovalFactory;

impl ApprovalAgentFactory for DefaultApprovalFactory {
    fn for_session(&self, _session_id: &str, auto_approve: bool) -> Arc<dyn ApprovalAgent> {
        if auto_approve {
            Arc::new(ApproveOnce)
        } else {
            Arc::new(DenyApproval)
        }
    }
}
