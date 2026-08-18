//! What an agent profile decides about a session: which tools it approves
//! without asking, and which denials the client's request keeps.

use super::*;

#[tokio::test]
async fn accept_edits_profile_auto_approves_only_mutating_file_tools() {
    let factory = DefaultApprovalFactory;
    let approval = factory.for_agent("session", AgentApproval::Edits, false);
    let edit = approval
        .request(ApprovalRequest {
            tool: "edit".to_owned(),
            input: Value::Null,
            requirements: vec![PermissionRequirement::outside_directory("/workspace/*")],
            rationale: "edit file".to_owned(),
        })
        .await
        .expect("edit decision");
    let read = approval
        .request(ApprovalRequest {
            tool: "read_file".to_owned(),
            input: Value::Null,
            requirements: vec![PermissionRequirement::outside_directory("/workspace/*")],
            rationale: "read file".to_owned(),
        })
        .await
        .expect("read decision");

    assert_eq!(edit, ApprovalDecision::ApproveOnce);
    assert_eq!(read, ApprovalDecision::Deny);
}

#[test]
fn agent_profile_keeps_requested_denials_and_explicit_auto_approval() {
    let temporary = tempfile::tempdir().expect("temporary profile root");
    let profile = crate::builtin_agents::profiles(temporary.path())
        .into_iter()
        .find(|profile| profile.name == "plan")
        .expect("plan profile");
    let mut intent = SessionIntent {
        disabled_tools: vec!["shell".to_owned()],
        requested_disabled_tools: vec!["shell".to_owned()],
        auto_approve: true,
        requested_auto_approve: true,
        ..SessionIntent::default()
    };

    apply_agent_profile_settings(&mut intent, &profile);

    assert_eq!(intent.disabled_tools, ["shell"]);
    assert!(intent.agent_permission_rules.iter().any(|rule| {
        rule.tool == "edit"
            && rule.pattern.ends_with("/plans/*")
            && rule.mode == vibe_core::policy::PermissionMode::Always
    }));
    assert!(intent.auto_approve);
    assert_eq!(intent.mode.as_deref(), Some("plan"));
}
