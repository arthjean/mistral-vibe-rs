//! Agent profiles, skill discovery and the subagent manager.

use std::sync::Arc;

use serde_json::json;

use super::agents::{
    AgentKind, auto_approves_edits, canonical_tool_name, profile_permission_scope,
};
use super::subagents::{
    ChildContext, ChildLoggingPolicy, DelegationRequest, DelegationStatus, SubagentFuture,
    SubagentManager, SubagentRun, SubagentRunner,
};
use super::*;
use crate::engine::CancellationToken;
use crate::policy::{PermissionMode, PermissionScope};
use crate::storage::SessionStore;

fn builtin_agent(name: &str, kind: AgentKind) -> AgentProfile {
    AgentProfile {
        name: name.to_owned(),
        display_name: title_from_name(name),
        description: String::new(),
        kind,
        safety: "neutral".to_owned(),
        overrides: Table::new(),
        source: ExtensionSource::Builtin,
        path: None,
    }
}

/// The rename moved the profile vocabulary onto the reference names, and
/// the permission scope each one maps to has to survive that move.
///
/// US-105 replaced the invented scope strings with the four reference
/// scopes, so a file tool's path glob answers for `outside_directory` and
/// every other tool's entry for `command_pattern`.
#[test]
fn reference_tool_names_keep_the_permission_scope_the_invented_names_produced() {
    for tool in ["read_file", "grep", "edit", "write_file"] {
        assert_eq!(
            profile_permission_scope(tool),
            PermissionScope::OutsideDirectory,
            "`{tool}` allowlists paths"
        );
    }

    // Nothing rewrites the reference file-tool names any more.
    assert_eq!(canonical_tool_name("read_file"), "read_file");
    assert_eq!(canonical_tool_name("grep"), "grep");
    // `bash` is the published name now, so a profile naming it must reach
    // the tool the registry serves rather than the manual shell resource.
    assert_eq!(canonical_tool_name("bash"), "bash");
    assert_eq!(
        profile_permission_scope("bash"),
        PermissionScope::CommandPattern
    );
}

/// `_plan_overrides` and the accept-edits profile both name `write_file`
/// and `edit`, so auto-approval has to resolve against those names.
#[test]
fn auto_approval_resolves_against_the_reference_mutating_tool_names() {
    let always = |tool: &str| {
        Table::from_iter([(
            "tools".to_owned(),
            toml::Value::Table(Table::from_iter([(
                tool.to_owned(),
                toml::Value::Table(Table::from_iter([(
                    "permission".to_owned(),
                    toml::Value::String("always".to_owned()),
                )])),
            )])),
        )])
    };

    assert!(auto_approves_edits(&always("edit")));
    assert!(auto_approves_edits(&always("write_file")));
    assert!(!auto_approves_edits(&always("read_file")));
    assert!(!auto_approves_edits(&always("grep")));
}

#[test]
fn agent_runtime_settings_resolve_profile_policy_without_name_conventions() {
    let mut profile = builtin_agent("custom-reviewer", AgentKind::Agent);
    profile.overrides.insert(
        "enabled_tools".to_owned(),
        toml::Value::Array(vec![
            toml::Value::String("read_file".to_owned()),
            toml::Value::String("write_file".to_owned()),
        ]),
    );
    profile.overrides.insert(
        "disabled_tools".to_owned(),
        toml::Value::Array(vec![toml::Value::String("grep".to_owned())]),
    );
    profile.overrides.insert(
        "tools".to_owned(),
        toml::Value::Table(Table::from_iter([(
            "write_file".to_owned(),
            toml::Value::Table(Table::from_iter([(
                "permission".to_owned(),
                toml::Value::String("always".to_owned()),
            )])),
        )])),
    );
    profile.overrides.insert(
        "models".to_owned(),
        toml::Value::Array(vec![toml::Value::Table(Table::from_iter([
            ("alias".to_owned(), toml::Value::String("review".to_owned())),
            (
                "name".to_owned(),
                toml::Value::String("mistral-review".to_owned()),
            ),
            (
                "thinking".to_owned(),
                toml::Value::String("high".to_owned()),
            ),
        ]))]),
    );
    profile.overrides.insert(
        "active_model".to_owned(),
        toml::Value::String("review".to_owned()),
    );
    profile
        .overrides
        .insert("mode".to_owned(), toml::Value::String("plan".to_owned()));

    let settings = profile.runtime_settings();

    // `read_file` and `grep` are published verbatim now, so a profile
    // naming them resolves to itself rather than to an invented local name.
    assert_eq!(settings.enabled_tools, ["read_file", "edit"]);
    assert_eq!(settings.disabled_tools, ["grep"]);
    assert_eq!(settings.approval, AgentApproval::Edits);
    assert_eq!(settings.model.as_deref(), Some("mistral-review"));
    assert_eq!(settings.thinking, Some(true));
    assert_eq!(settings.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(settings.mode.as_deref(), Some("plan"));
    assert_eq!(settings.system_prompt_id, None);
}

#[test]
fn agent_runtime_settings_enforce_never_permissions_and_prompt_selection() {
    let mut profile = builtin_agent("planner", AgentKind::Agent);
    profile.overrides.insert(
        "tools".to_owned(),
        toml::Value::Table(Table::from_iter([(
            "write_file".to_owned(),
            toml::Value::Table(Table::from_iter([(
                "permission".to_owned(),
                toml::Value::String("never".to_owned()),
            )])),
        )])),
    );
    profile.overrides.insert(
        "system_prompt_id".to_owned(),
        toml::Value::String("plan".to_owned()),
    );

    let settings = profile.runtime_settings();

    assert_eq!(settings.disabled_tools, ["edit"]);
    assert_eq!(settings.system_prompt_id.as_deref(), Some("plan"));
}

#[test]
fn agent_runtime_settings_preserve_never_policy_allowlist_exceptions() {
    let mut profile = builtin_agent("planner", AgentKind::Agent);
    profile.overrides.insert(
        "tools".to_owned(),
        toml::Value::Table(Table::from_iter([(
            "write_file".to_owned(),
            toml::Value::Table(Table::from_iter([
                (
                    "permission".to_owned(),
                    toml::Value::String("never".to_owned()),
                ),
                (
                    "allowlist".to_owned(),
                    toml::Value::Array(vec![toml::Value::String("/workspace/plans/*".to_owned())]),
                ),
            ])),
        )])),
    );

    let settings = profile.runtime_settings();

    assert!(settings.disabled_tools.is_empty());
    assert!(settings.permission_rules.iter().any(|rule| {
        rule.tool == "edit" && rule.scope.is_none() && rule.mode == PermissionMode::Never
    }));
    assert!(settings.permission_rules.iter().any(|rule| {
        rule.tool == "edit"
            && rule.scope == Some(PermissionScope::OutsideDirectory)
            && rule.pattern == "/workspace/plans/*"
            && rule.mode == PermissionMode::Always
    }));
}

#[test]
fn allowlisted_edit_approval_does_not_expand_to_every_path() {
    let mut profile = builtin_agent("scoped-editor", AgentKind::Agent);
    profile.overrides.insert(
        "tools".to_owned(),
        toml::Value::Table(Table::from_iter([(
            "edit".to_owned(),
            toml::Value::Table(Table::from_iter([
                (
                    "permission".to_owned(),
                    toml::Value::String("always".to_owned()),
                ),
                (
                    "allowlist".to_owned(),
                    toml::Value::Array(vec![toml::Value::String(
                        "/workspace/generated/*".to_owned(),
                    )]),
                ),
            ])),
        )])),
    );

    let settings = profile.runtime_settings();

    assert_eq!(settings.approval, AgentApproval::Prompt);
    assert_eq!(settings.permission_rules.len(), 1);
    assert_eq!(
        settings.permission_rules[0].scope,
        Some(PermissionScope::OutsideDirectory)
    );
    assert_eq!(
        settings.permission_rules[0].pattern,
        "/workspace/generated/*"
    );
}

/// Reference `AgentRegistry._discover`: every directory searched takes a
/// builtin name over, so the last one wins, while a custom name keeps the
/// first file that declared it. The skill roots are their own list.
#[test]
fn a_builtin_name_goes_to_the_last_directory_and_a_custom_name_to_the_first() {
    let temporary = tempfile::tempdir().expect("temporary roots");
    let configured = temporary.path().join("configured");
    let user = temporary.path().join("user");
    for root in [&configured, &user] {
        fs::create_dir_all(root.join("agents")).expect("agent directory");
        fs::create_dir_all(root.join("skills/probe")).expect("skill directory");
    }
    let skill_roots = crate::skills::SkillDiscovery {
        roots: vec![
            (configured.join("skills"), crate::skills::SkillScope::Global),
            (user.join("skills"), crate::skills::SkillScope::Global),
        ],
        ..crate::skills::SkillDiscovery::default()
    };
    for (root, origin) in [(&configured, "configured"), (&user, "user")] {
        for name in ["default", "custom"] {
            fs::write(
                root.join(format!("agents/{name}.toml")),
                format!("description = \"{origin}\"\n"),
            )
            .expect("agent file");
        }
    }
    fs::write(
        configured.join("skills/probe/SKILL.md"),
        "---\nname: probe\ndescription: configured\n---\nconfigured body",
    )
    .expect("configured skill");
    fs::write(
        user.join("skills/probe/SKILL.md"),
        "---\nname: probe\ndescription: user\n---\nuser body",
    )
    .expect("user skill");
    let roots = DiscoveryRoots {
        agents: vec![
            (ExtensionSource::Configured, configured.join("agents")),
            (ExtensionSource::User, user.join("agents")),
        ],
        skills: skill_roots,
        ..DiscoveryRoots::default()
    };
    let catalog = discover_extensions(
        &roots,
        BTreeMap::from([(
            "default".to_owned(),
            builtin_agent("default", AgentKind::Agent),
        )]),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    assert_eq!(catalog.agents["default"].description, "user");
    assert_eq!(catalog.agents["custom"].description, "configured");
    assert_eq!(catalog.skills["probe"].body, "configured body");
}

/// A profile that does not load is skipped without an issue, as the
/// reference only logs it, and a legacy `base_disabled` list is folded
/// into `disabled_tools` on disk.
#[test]
fn unloadable_profiles_are_skipped_and_legacy_ones_rewritten() {
    let temporary = tempfile::tempdir().expect("temporary roots");
    let agents = temporary.path().join("agents");
    fs::create_dir_all(&agents).expect("agent directory");
    fs::write(agents.join("broken.toml"), "broken = [").expect("bad agent");
    fs::write(agents.join("reckless.toml"), "safety = \"reckless\"\n").expect("bad safety");
    fs::write(
        agents.join("legacy.toml"),
        "base_disabled = [\"bash\", \"grep\"]\ndisabled_tools = [\"grep\"]\n",
    )
    .expect("legacy agent");
    let catalog = discover_extensions(
        &DiscoveryRoots {
            agents: vec![(ExtensionSource::User, agents.clone())],
            ..DiscoveryRoots::default()
        },
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    assert_eq!(catalog.agents.keys().collect::<Vec<_>>(), ["legacy"]);
    assert!(catalog.issues.is_empty());
    let rewritten = fs::read_to_string(agents.join("legacy.toml"))
        .expect("rewritten")
        .parse::<Table>()
        .expect("still TOML");
    assert!(!rewritten.contains_key("base_disabled"));
    assert_eq!(
        rewritten["disabled_tools"],
        toml::Value::Array(vec!["grep".into(), "bash".into()])
    );
}

#[test]
fn a_display_name_defaults_to_the_title_cased_file_stem() {
    assert_eq!(title_from_name("code_review-v2x"), "Code_Review V2X");
    assert_eq!(title_from_name("my-AGENT"), "My Agent");
}

/// Writes the child's prompt to its transcript, which is what a running
/// child does first and what saves its session.
fn record_prompt(context: &ChildContext) {
    let store = &context.store;
    let mut metadata = store
        .open(&context.child_session_id)
        .expect("the child is held")
        .metadata;
    store
        .append_message(
            &mut metadata,
            &crate::events::ModelMessage::user(context.prompt.clone()),
            10,
        )
        .expect("the child prompt is saved");
}

struct FakeSubagent;

impl SubagentRunner for FakeSubagent {
    fn run<'a>(
        &'a self,
        context: ChildContext,
        _cancellation: CancellationToken,
    ) -> SubagentFuture<'a> {
        Box::pin(async move {
            record_prompt(&context);
            Ok(SubagentRun {
                response: format!("{}:{}", context.agent.name, context.prompt),
                turns_used: 1,
                completed: true,
            })
        })
    }
}

/// A subagent that opens the span its own turn would open, which is what
/// makes the delegation path itself measurable.
struct TracingSubagent;

impl SubagentRunner for TracingSubagent {
    fn run<'a>(
        &'a self,
        context: ChildContext,
        _cancellation: CancellationToken,
    ) -> SubagentFuture<'a> {
        Box::pin(async move {
            let outcome: Result<(), String> = crate::tracing::agent_span(
                crate::tracing::AgentSpan {
                    model: None,
                    session_id: Some(&context.child_session_id),
                },
                async { Ok(()) },
            )
            .await;
            outcome.map(|()| SubagentRun {
                response: "delegated".to_owned(),
                turns_used: 1,
                completed: true,
            })
        })
    }
}

/// US-016: a delegation is awaited inside the tool span that asked for it,
/// so the child's own agent span hangs off that span rather than opening a
/// second trace, and it publishes the child conversation id the way
/// reference `_loop.py` does, one `agent_span` per loop.
#[tokio::test]
async fn a_delegated_turn_hangs_off_the_tool_span_that_asked_for_it() {
    let _exclusive = crate::tracing::harness::exclusive();
    let harness = crate::tracing::harness::Harness::install();
    let temporary = tempfile::tempdir().expect("temporary sessions");
    let store = SessionStore::new(temporary.path());
    store
        .create("parent", "/workspace", None, 1)
        .expect("parent session");
    let manager = SubagentManager::new(store, Arc::new(TracingSubagent));
    let effect = crate::tracing::tool_span(
        crate::tracing::ToolSpan {
            tool_name: "task",
            call_id: "call",
            arguments: "{}",
        },
        async {
            manager
                .delegate(
                    DelegationRequest {
                        parent_session_id: "parent".to_owned(),
                        agent: builtin_agent("explore", AgentKind::Subagent),
                        prompt: "inspect".to_owned(),
                        logging: ChildLoggingPolicy::SummaryOnly,
                        tool_call_id: "call-1".to_owned(),
                        signal: None,
                    },
                    10,
                )
                .await
        },
    )
    .await
    .expect("delegation completes");
    let spans = harness.drain();
    drop(harness);
    let tool = spans
        .iter()
        .find(|span| span.name == "execute_tool task")
        .expect("the delegating tool span was exported");
    let child = spans
        .iter()
        .find(|span| span.name == "invoke_agent mistral-vibe")
        .expect("the delegated turn opened its own agent span");
    assert_eq!(
        child.span_context.trace_id(),
        tool.span_context.trace_id(),
        "the delegated turn stays inside the trace that asked for it"
    );
    assert_eq!(child.parent_span_id, tool.span_context.span_id());
    assert_eq!(
        child
            .attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == "gen_ai.conversation.id")
            .map(|attribute| attribute.value.to_string()),
        Some(effect.child_session_id),
        "the child publishes its own conversation id, as the reference does"
    );
}

struct HangingSubagent {
    entered: Arc<tokio::sync::Notify>,
}

impl SubagentRunner for HangingSubagent {
    fn run<'a>(
        &'a self,
        context: ChildContext,
        _cancellation: CancellationToken,
    ) -> SubagentFuture<'a> {
        Box::pin(async move {
            record_prompt(&context);
            self.entered.notify_one();
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn delegation_uses_independent_child_sessions_and_bounded_depth() {
    let temporary = tempfile::tempdir().expect("temporary sessions");
    let store = SessionStore::new(temporary.path());
    let mut parent = store
        .create("parent", "/workspace", None, 1)
        .expect("parent session");
    parent.config.insert("model".to_owned(), json!("child"));
    store.update_metadata(&parent).expect("parent config");
    let manager = SubagentManager::new(store.clone(), Arc::new(FakeSubagent));
    let agent = builtin_agent("explore", AgentKind::Subagent);
    let effect = manager
        .delegate(
            DelegationRequest {
                parent_session_id: "parent".to_owned(),
                agent: agent.clone(),
                prompt: "inspect".to_owned(),
                logging: ChildLoggingPolicy::SummaryOnly,
                tool_call_id: "call-1".to_owned(),
                signal: None,
            },
            10,
        )
        .await
        .expect("delegation completes");
    assert_eq!(effect.status, DelegationStatus::Completed);
    assert_ne!(effect.child_session_id, effect.parent_session_id);
    let parent_record = store.open("parent").expect("parent loads").metadata;
    let children = store.child_store(&parent_record, "explore");
    let child = children
        .open(&effect.child_session_id)
        .expect("child persisted beneath its parent");
    assert_eq!(child.metadata.parent_session_id.as_deref(), Some("parent"));
    assert_eq!(child.metadata.config["model"], "child");
    assert_eq!(
        SubagentManager::activity(&effect, "tool").root_session_id,
        "parent"
    );
    let second = manager
        .delegate(
            DelegationRequest {
                parent_session_id: "parent".to_owned(),
                agent: agent.clone(),
                prompt: "inspect again".to_owned(),
                logging: ChildLoggingPolicy::SummaryOnly,
                tool_call_id: "call-1".to_owned(),
                signal: None,
            },
            10,
        )
        .await
        .expect("same-millisecond delegation completes");
    assert_ne!(effect.child_session_id, second.child_session_id);
    // Both children are linked into the parent's record, and neither is a
    // session of its own in the parent's store.
    assert_eq!(
        store
            .open("parent")
            .expect("parent loads")
            .metadata
            .child_sessions
            .len(),
        1,
        "the second delegation reused the first call's identifier"
    );
    assert!(
        store
            .sessions(None)
            .expect("sessions list")
            .iter()
            .all(|session| session.parent_session_id.is_none())
    );

    let mut parent = store.open("parent").expect("parent loads").metadata;
    parent.agent_profile = Some(json!({"depth": MAX_DELEGATION_DEPTH}));
    store.update_metadata(&parent).expect("parent depth");
    assert!(matches!(
        manager
            .delegate(
                DelegationRequest {
                    parent_session_id: "parent".to_owned(),
                    agent,
                    prompt: "recursive".to_owned(),
                    logging: ChildLoggingPolicy::Disabled,
                    tool_call_id: "call-3".to_owned(),
                    signal: None,
                },
                20,
            )
            .await,
        Err(ExtensionError::DelegationDepth { .. })
    ));
}

#[tokio::test]
async fn parent_cancellation_forces_child_finalization() {
    let temporary = tempfile::tempdir().expect("temporary sessions");
    let store = SessionStore::new(temporary.path());
    store
        .create("parent", "/workspace", None, 1)
        .expect("parent session");
    let entered = Arc::new(tokio::sync::Notify::new());
    let manager = SubagentManager::new(
        store.clone(),
        Arc::new(HangingSubagent {
            entered: entered.clone(),
        }),
    );
    let delegation = tokio::spawn({
        let manager = manager.clone();
        async move {
            manager
                .delegate(
                    DelegationRequest {
                        parent_session_id: "parent".to_owned(),
                        agent: builtin_agent("explore", AgentKind::Subagent),
                        prompt: "wait".to_owned(),
                        logging: ChildLoggingPolicy::SummaryOnly,
                        tool_call_id: "call-1".to_owned(),
                        signal: None,
                    },
                    10,
                )
                .await
        }
    });
    entered.notified().await;
    manager.cancel_parent("parent").await;
    let effect = tokio::time::timeout(std::time::Duration::from_millis(250), delegation)
        .await
        .expect("delegation cancellation")
        .expect("delegation task")
        .expect("delegation effect");
    assert_eq!(effect.status, DelegationStatus::Cancelled);
    let parent_record = store.open("parent").expect("parent loads").metadata;
    assert!(
        store
            .child_store(&parent_record, "explore")
            .open(&effect.child_session_id)
            .expect("child remains auditable")
            .metadata
            .end_time
            .is_some()
    );
    assert!(manager.active.lock().await.is_empty());
}
