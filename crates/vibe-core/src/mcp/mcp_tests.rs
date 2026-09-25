//! The registry's own lifecycle, against peers that do no I/O.
//!
//! What a server sees and what the model reads is measured against the
//! reference by the transport replay in `crates/vibe-app-server/tests`; these
//! cases cover the session state around it: toggles, retirement, the
//! descriptor caches and the invariants a tool call relies on.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::json;
use tokio::sync::Notify;

use super::*;
use crate::policy::{ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionMode};

struct AlwaysApprove;

impl ApprovalAgent for AlwaysApprove {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
    }
}

#[derive(Default)]
struct FakePeer {
    tools: Vec<RemoteTool>,
    fail_calls: bool,
    discoveries: AtomicUsize,
    closed: AtomicBool,
    hang: Option<Notify>,
}

impl FakePeer {
    fn with(tools: Vec<RemoteTool>) -> Self {
        Self {
            tools,
            ..Self::default()
        }
    }
}

impl McpPeer for FakePeer {
    fn discover<'a>(
        &'a self,
        _headers: &'a BTreeMap<String, String>,
    ) -> McpFuture<'a, Vec<RemoteTool>> {
        Box::pin(async move {
            self.discoveries.fetch_add(1, Ordering::AcqRel);
            Ok(self.tools.clone())
        })
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: Value,
        _headers: &'a BTreeMap<String, String>,
    ) -> McpFuture<'a, ToolExecutionOutput> {
        Box::pin(async move {
            if let Some(entered) = &self.hang {
                entered.notify_one();
                std::future::pending::<()>().await;
            }
            if self.fail_calls {
                return Err(McpError::Transport("the server went away".to_owned()));
            }
            Ok(ToolExecutionOutput::new(format!("{name} completed"))
                .typed(json!({"tool": name, "arguments": arguments})))
        })
    }

    fn close<'a>(&'a self) -> McpFuture<'a, ()> {
        Box::pin(async move {
            self.closed.store(true, Ordering::Release);
            Ok(())
        })
    }
}

struct FakeFactory {
    peers: BTreeMap<String, Arc<dyn McpPeer>>,
}

impl McpPeerFactory for FakeFactory {
    fn connect<'a>(&'a self, config: &'a McpServerConfig) -> McpFuture<'a, Arc<dyn McpPeer>> {
        Box::pin(async move {
            self.peers
                .get(&config.alias)
                .cloned()
                .ok_or_else(|| McpError::Transport("connection failed".to_owned()))
        })
    }
}

fn factory(peers: &[(&str, Arc<dyn McpPeer>)]) -> Arc<FakeFactory> {
    Arc::new(FakeFactory {
        peers: peers
            .iter()
            .map(|(alias, peer)| ((*alias).to_owned(), peer.clone()))
            .collect(),
    })
}

fn config(alias: &str) -> McpServerConfig {
    McpServerConfig {
        alias: alias.to_owned(),
        transport: McpTransportConfig::StreamableHttp {
            url: Url::parse(&format!("https://{alias}.example/mcp")).expect("url"),
            headers: BTreeMap::new(),
        },
        enabled: true,
        disabled_tools: BTreeSet::new(),
        startup_timeout_ms: DEFAULT_MCP_STARTUP_TIMEOUT_MS,
        tool_timeout_ms: DEFAULT_MCP_TOOL_TIMEOUT_MS,
        auth: McpAuthConfig::default(),
        prompt: None,
        sampling_enabled: true,
        declared: None,
    }
}

fn remote_tool() -> RemoteTool {
    RemoteTool {
        name: "search".to_owned(),
        description: Some("Search remote data".to_owned()),
        input_schema: json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"],
        }),
        output_schema: None,
        annotations: json!({"readOnlyHint": true}),
    }
}

fn invocation(call_id: &str) -> ToolInvocation {
    ToolInvocation {
        call_id: call_id.to_owned(),
        arguments: json!({"query": "rust"}),
    }
}

async fn discover(
    registry: &McpRegistry,
    configs: Vec<McpServerConfig>,
    factory: Arc<FakeFactory>,
    tools: &ToolRegistry,
) -> Vec<String> {
    registry
        .discover_all(
            configs,
            factory,
            tools,
            PermissionStore::default(),
            Arc::new(AlwaysApprove),
        )
        .await
}

/// The add commands decide whether a plaintext URL on another host may be
/// stored; once it is, the session connects to it as the reference does.
#[test]
fn a_configured_plaintext_server_url_is_accepted_on_any_host() {
    let mut lan = config("lan");
    lan.transport = McpTransportConfig::StreamableHttp {
        url: Url::parse("http://lan.example/mcp").expect("url"),
        headers: BTreeMap::new(),
    };
    assert!(validate_config(&lan).is_ok());
    lan.transport = McpTransportConfig::StreamableHttp {
        url: Url::parse("http://lan.example/mcp#fragment").expect("url"),
        headers: BTreeMap::new(),
    };
    assert!(validate_config(&lan).is_err());
}

#[tokio::test]
async fn partial_failure_keeps_healthy_server_and_policy_guards_tool() {
    let good: Arc<dyn McpPeer> = Arc::new(FakePeer::with(vec![remote_tool()]));
    let registry = McpRegistry::default();
    let tools = ToolRegistry::default();
    let policy = PermissionStore::default();
    policy.set_tool_permission("good_search", PermissionMode::Always);
    let diagnostics = registry
        .discover_all(
            vec![config("good"), config("failed")],
            factory(&[("good", good)]),
            &tools,
            policy,
            Arc::new(AlwaysApprove),
        )
        .await;
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    let views = registry.read().await;
    assert_eq!(views[0].status, McpServerStatus::Failed);
    assert_eq!(views[1].status, McpServerStatus::Healthy);
    let output = tools
        .invoke("good_search", invocation("call-1"))
        .await
        .expect("invoke");
    assert_eq!(output.typed_result["tool"], "search");

    let disabled = registry.toggle("good", false).await.expect("disable");
    assert_eq!(disabled.status, McpServerStatus::Disabled);
    assert!(matches!(
        tools.invoke("good_search", invocation("call-2")).await,
        Err(ToolError::Unavailable(_))
    ));
    let enabled = registry.toggle("good", true).await.expect("reconnect");
    assert_eq!(enabled.status, McpServerStatus::Healthy);
}

/// Reference `create_mcp_http_proxy_tool_class`: the server's name in
/// brackets, the tool's description or the sentence naming its origin, and
/// the server's hint.
#[tokio::test]
async fn a_published_description_names_its_server_and_carries_the_hint() {
    let mut bare = remote_tool();
    bare.name = "bare".to_owned();
    bare.description = Some(String::new());
    let peer: Arc<dyn McpPeer> = Arc::new(FakePeer::with(vec![remote_tool(), bare]));
    let mut configured = config("docs");
    configured.prompt = Some("Search the handbook first".to_owned());
    let tools = ToolRegistry::default();
    discover(
        &McpRegistry::default(),
        vec![configured],
        factory(&[("docs", peer)]),
        &tools,
    )
    .await;

    let described = tools
        .list()
        .expect("the registry lists what it registered")
        .into_iter()
        .map(|spec| (spec.name, spec.description))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        described["docs_search"],
        "[docs] Search remote data\nHint: Search the handbook first"
    );
    assert_eq!(
        described["docs_bare"],
        "[docs] MCP tool 'bare' from https://docs.example/mcp\nHint: Search the handbook first"
    );
}

/// A call that fails leaves the server and its tools where they were: the
/// reference raises the failure to the model and keeps publishing the tool.
#[tokio::test]
async fn a_failed_call_keeps_the_server_published() {
    let peer: Arc<dyn McpPeer> = Arc::new(FakePeer {
        tools: vec![remote_tool()],
        fail_calls: true,
        ..FakePeer::default()
    });
    let registry = McpRegistry::default();
    let tools = ToolRegistry::default();
    discover(
        &registry,
        vec![config("flaky")],
        factory(&[("flaky", peer)]),
        &tools,
    )
    .await;

    assert!(matches!(
        tools.invoke("flaky_search", invocation("first")).await,
        Err(ToolError::Approved { source, .. }) if matches!(*source, ToolError::Execution(_))
    ));
    assert_eq!(registry.read().await[0].status, McpServerStatus::Healthy);
    assert!(matches!(
        tools.invoke("flaky_search", invocation("second")).await,
        Err(ToolError::Approved { source, .. }) if matches!(*source, ToolError::Execution(_))
    ));
}

/// Reference `_memory_hit`: a server discovered in this session answers the
/// next discovery from memory, until something forces it to be reached.
#[tokio::test]
async fn a_rediscovery_answers_from_memory_until_refreshed() {
    let peer = Arc::new(FakePeer::with(vec![remote_tool()]));
    let registry = McpRegistry::default();
    let tools = ToolRegistry::default();
    let factory = factory(&[("docs", peer.clone() as Arc<dyn McpPeer>)]);
    discover(&registry, vec![config("docs")], factory.clone(), &tools).await;
    discover(&registry, vec![config("docs")], factory, &tools).await;
    assert_eq!(peer.discoveries.load(Ordering::Acquire), 1);
    assert_eq!(registry.published().await, ["docs_search".to_owned()]);

    registry.refresh("docs").await.expect("refresh");
    assert_eq!(peer.discoveries.load(Ordering::Acquire), 2);
}

/// A discovery written to the persistent cache is served to a new session
/// without reaching the server.
#[tokio::test]
async fn a_new_session_reads_the_persistent_descriptor_cache() {
    let root = tempfile::tempdir().expect("cache root");
    let cache = Arc::new(McpDescriptorCache::new(
        root.path().join("legacy"),
        86_400.0,
    ));
    let first = Arc::new(FakePeer::with(vec![remote_tool()]));
    let registry = McpRegistry::default();
    registry
        .configure_descriptor_cache(Some(cache.clone()))
        .await;
    discover(
        &registry,
        vec![config("docs")],
        factory(&[("docs", first.clone() as Arc<dyn McpPeer>)]),
        &ToolRegistry::default(),
    )
    .await;

    let second = Arc::new(FakePeer::with(Vec::new()));
    let restarted = McpRegistry::default();
    restarted.configure_descriptor_cache(Some(cache)).await;
    let tools = ToolRegistry::default();
    discover(
        &restarted,
        vec![config("docs")],
        factory(&[("docs", second.clone() as Arc<dyn McpPeer>)]),
        &tools,
    )
    .await;
    assert_eq!(second.discoveries.load(Ordering::Acquire), 0);
    assert_eq!(restarted.published().await, ["docs_search".to_owned()]);
}

#[tokio::test]
async fn rediscovery_retires_aliases_missing_from_the_new_configuration() {
    let first_b = Arc::new(FakePeer::with(vec![remote_tool()]));
    let registry = McpRegistry::default();
    let tools = ToolRegistry::default();
    discover(
        &registry,
        vec![config("a"), config("b")],
        factory(&[
            (
                "a",
                Arc::new(FakePeer::with(vec![remote_tool()])) as Arc<dyn McpPeer>,
            ),
            ("b", first_b.clone() as Arc<dyn McpPeer>),
        ]),
        &tools,
    )
    .await;
    discover(
        &registry,
        vec![config("a")],
        factory(&[(
            "a",
            Arc::new(FakePeer::with(vec![remote_tool()])) as Arc<dyn McpPeer>,
        )]),
        &tools,
    )
    .await;

    assert!(first_b.closed.load(Ordering::Acquire));
    assert_eq!(
        registry
            .read()
            .await
            .into_iter()
            .map(|view| view.alias)
            .collect::<Vec<_>>(),
        vec!["a"]
    );
    assert!(matches!(
        tools.invoke("b_search", invocation("retired")).await,
        Err(ToolError::Unavailable(_))
    ));
}

#[tokio::test]
async fn disabled_tool_preferences_survive_disabled_startup_and_reconnect() {
    let peer: Arc<dyn McpPeer> = Arc::new(FakePeer::with(vec![remote_tool()]));
    let mut disabled = config("disabled");
    disabled.enabled = false;
    disabled.disabled_tools = BTreeSet::from(["search".to_owned()]);
    let registry = McpRegistry::default();
    let tools = ToolRegistry::default();
    discover(
        &registry,
        vec![disabled],
        factory(&[("disabled", peer)]),
        &tools,
    )
    .await;

    let initial = registry.read().await.remove(0);
    assert_eq!(
        initial.disabled_tools,
        BTreeSet::from(["search".to_owned()])
    );
    let enabled = registry.toggle("disabled", true).await.expect("reconnect");
    assert_eq!(
        enabled.disabled_tools,
        BTreeSet::from(["disabled_search".to_owned()])
    );
    assert!(matches!(
        tools.invoke("disabled_search", invocation("call")).await,
        Err(ToolError::Unavailable(_))
    ));
}

/// Reference `_apply_per_source_filtering` keys the denylist by source, so two
/// servers exposing the same remote name are filtered independently.
#[tokio::test]
async fn a_per_source_denylist_only_withholds_that_server_tools() {
    let mut muted = config("muted");
    muted.disabled_tools = BTreeSet::from(["search".to_owned()]);
    let tools = ToolRegistry::default();
    discover(
        &McpRegistry::default(),
        vec![muted, config("loud")],
        factory(&[
            (
                "muted",
                Arc::new(FakePeer::with(vec![remote_tool()])) as Arc<dyn McpPeer>,
            ),
            (
                "loud",
                Arc::new(FakePeer::with(vec![remote_tool()])) as Arc<dyn McpPeer>,
            ),
        ]),
        &tools,
    )
    .await;

    let published = tools
        .available(None, &crate::matching::NameFilter::default())
        .expect("available")
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    assert_eq!(published, ["loud_search".to_owned()]);
}

#[tokio::test]
async fn per_tool_toggle_survives_a_refresh_and_a_logout_withdraws_the_tool() {
    let peer: Arc<dyn McpPeer> = Arc::new(FakePeer::with(vec![remote_tool()]));
    let registry = McpRegistry::default();
    let tools = ToolRegistry::default();
    discover(
        &registry,
        vec![config("tools")],
        factory(&[("tools", peer)]),
        &tools,
    )
    .await;

    let disabled = registry
        .toggle_tool("tools", "tools_search", false)
        .await
        .expect("tool disables");
    assert!(disabled.disabled_tools.contains("tools_search"));
    let refreshed = registry.refresh("tools").await.expect("server refreshes");
    assert!(refreshed.disabled_tools.contains("tools_search"));
    assert!(matches!(
        tools
            .invoke("tools_search", invocation("still-disabled"))
            .await,
        Err(ToolError::Unavailable(_))
    ));

    let enabled = registry
        .toggle_tool("tools", "tools_search", true)
        .await
        .expect("tool re-enables");
    assert!(enabled.disabled_tools.is_empty());
    tools
        .invoke("tools_search", invocation("enabled"))
        .await
        .expect("enabled tool invokes");

    let cleared = registry.clear_auth("tools").await.expect("auth clears");
    assert_eq!(cleared.status, McpServerStatus::AuthRequired);
    assert!(matches!(
        tools.invoke("tools_search", invocation("logged-out")).await,
        Err(ToolError::Unavailable(_))
    ));
}

#[tokio::test]
async fn disabling_a_server_cancels_its_running_call() {
    let peer = Arc::new(FakePeer {
        tools: vec![remote_tool()],
        hang: Some(Notify::new()),
        ..FakePeer::default()
    });
    let registry = McpRegistry::default();
    let tools = ToolRegistry::default();
    discover(
        &registry,
        vec![config("slow")],
        factory(&[("slow", peer.clone() as Arc<dyn McpPeer>)]),
        &tools,
    )
    .await;

    let running = {
        let tools = tools.clone();
        tokio::spawn(async move { tools.invoke("slow_search", invocation("hung")).await })
    };
    if let Some(entered) = &peer.hang {
        entered.notified().await;
    }
    registry.toggle("slow", false).await.expect("disable");
    let outcome = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the call ends once its server is disabled")
        .expect("the call task joins");
    assert!(matches!(
        outcome,
        Err(ToolError::Approved { source, .. }) if matches!(*source, ToolError::Unavailable(_))
    ));
    assert!(peer.closed.load(Ordering::Acquire));
}

/// An entry that names an OAuth block and no authentication service has
/// nothing to connect with, and says so rather than connecting anonymously.
#[tokio::test]
async fn an_oauth_entry_without_a_service_requires_authorization() {
    let peer = Arc::new(FakePeer::with(vec![remote_tool()]));
    let mut oauth = config("secure");
    oauth.auth = McpAuthConfig::Oauth(McpOAuthConfig::default());
    let registry = McpRegistry::default();
    discover(
        &registry,
        vec![oauth],
        factory(&[("secure", peer.clone() as Arc<dyn McpPeer>)]),
        &ToolRegistry::default(),
    )
    .await;
    assert_eq!(peer.discoveries.load(Ordering::Acquire), 0);
    assert_eq!(
        registry.read().await[0].status,
        McpServerStatus::AuthRequired
    );
    assert_eq!(
        registry.needs_auth().await,
        BTreeSet::from(["secure".to_owned()])
    );
}
