use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

mod attachment;
mod batch;

use attachment::apply_agent_profile_settings;
mod callbacks;
mod compaction;
mod connection;
mod connector_catalog;
mod manual_shell;
mod projection;
mod registry;
mod review;
mod runtime;
mod session_callbacks;
mod session_management;
mod titles;
mod turn_queue;
mod turns;
mod vibe_code_host;
mod wire;

pub(crate) use manual_shell::ShellOperation;
pub use runtime::SessionView;
pub(crate) use runtime::*;
pub use wire::SessionIntent;
pub(crate) use wire::*;

use turns::scheduled_loop_turn;

use batch::*;
use callbacks::*;
pub use connection::ServerConnection;
use projection::*;
use registry::SessionRegistry;
pub use vibe_code_host::{ClientLaunch, FrameSink};

use crate::client::{TurnReservation, public_turn_failure};
use crate::client_tools::ClientToolBridge;
use crate::harness::HarnessSelection;
use crate::host::{now_millis, vibe_home};
use crate::projects::{
    LoopFire, PROJECTS_METHODS, ProjectsDispatch, ProjectsService, ProjectsServiceError,
};
use crate::resources::{
    BACKEND_RESOURCE_METHODS, CoreResourceBackend, McpCatalogCall, McpCatalogError,
    McpCatalogNotify, McpCatalogOutcome, McpCatalogTarget, RESOURCE_METHODS, ResourceBackend,
    ResourceBackendCommand, ResourceBackendRequest, ResourceDispatch, ResourceError,
    ResourceService, ResourceSession, ResourceSignals,
};
use crate::workspace::{
    RuntimeAttachment, WORKSPACE_METHODS, WorkspaceService, WorkspaceServiceError,
};
use crate::worktrees::{self, WorktreeInput, WorktreeResolution};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use vibe_core::config::DotenvValues;
use vibe_core::engine::CompletionProvider;
use vibe_core::events::{
    CallbackDetail, CallbackKind as EngineCallbackKind, CallbackOutput, EffectDetail,
    EffectResultDisplay, LifecycleState, ModelMessage, NoticeDetail, ProjectionSnapshot,
    PublicCallbackState, PublicContentBlock, PublicEffectState, PublicEntryGenerationStatus,
    PublicEntryMetadata, PublicError, PublicHistoryEntry, PublicMessageRole, PublicMessageSource,
    PublicTurn, PublicTurnStatus, TurnErrorCode,
};
use vibe_core::extensions::{AgentApproval, AgentProfile};
use vibe_core::integrations::redact;
use vibe_core::matching::NameFilter;
use vibe_core::mcp::McpServerConfig;
use vibe_core::middleware::CompactionSettings;
use vibe_core::observability;
use vibe_core::observability::{FileLog, LogLevel, LogSettings};
pub use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionRequirement,
};
use vibe_core::policy::{PermissionRule, PermissionStore, ToolGuard, TrustDecision, TrustRootKind};
use vibe_core::scratchpad::{cleanup_scratchpad, init_scratchpad, scratchpad_path};
use vibe_core::storage::HydratedSession;
use vibe_core::telemetry::{ClientTelemetry, NoClientTelemetry};
pub use vibe_core::tools::builtins::{BuiltinTools, WebSearchAccess};
use vibe_core::tools::shell::ShellTools;
pub use vibe_core::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolError, ToolExecutionOutput, ToolInvocation,
    ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
};
use vibe_core::workspace::{ReviewManager, Workspace, WorkspaceTools};
use vibe_core::worktree::lifecycle::SessionWorktrees;
use vibe_core::worktree::{ManagedWorktree, PreparedWorktree, naming_model};
use vibe_protocol::{
    CallbackKind, ClientCapabilities, ClientEntrypoint, Envelope, ErrorResponse, InitializeParams,
    InitializeResponse, InvalidParamsData, InvalidParamsIssue, JsonRpcVersion, Notification,
    PathSegment, ProtocolError, ProtocolErrorCode, RequestId, ServerInfo, ServerRequest,
    SuccessResponse, TransportKind, decode_frame, encode_frame, is_server_method,
};

const INITIALIZE_METHOD: &str = "initialize";
const INITIALIZED_NOTIFICATION: &str = "initialized";
const SHUTDOWN_METHOD: &str = "shutdown";
const EXIT_NOTIFICATION: &str = "exit";
/// What a tool-surface diagnostic is attributed to: the tool filters and the
/// availability conditions both come from the configuration the session loaded.
pub(crate) const CONFIG_FILE_LABEL: &str = "config.toml";
/// What a diagnostic about the session's checkpoint log names as its source.
pub(crate) const FILE_HISTORY_LABEL: &str = "file history";
const MAX_CALLBACK_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_CALLBACK_REQUEST_BYTES: usize = 64 * 1024;
const MAX_CALLBACK_ANSWERS: usize = 16;
const MAX_CALLBACK_OPTIONS: usize = 32;
const MAX_CALLBACK_TEXT_BYTES: usize = 8 * 1024;
/// Every notification name this build emits, sorted and unique.
///
/// [`notification_method`] gates each outbound name against this list, so it is
/// what the server actually sends rather than a hand-kept description of it. The
/// app-server surface replay reads it to report notification conformance, and
/// `docs/parity.md` records the names here that the reference does not declare.
pub const EMITTED_NOTIFICATIONS: &[&str] = &[
    "connector_catalog/authFailed",
    "connector_catalog/authRequired",
    "connector_catalog/authUrl",
    "error",
    "history/entryAdded",
    "history/entryUpdated",
    "mcp/authUrl",
    "mcp_catalog/authRequired",
    "mcp_catalog/authUrl",
    "runtime/updated",
    "session/compacted",
    "session/contextCleared",
    "session/snapshot",
    "session/statsUpdated",
    "session/updated",
    "turn/completed",
    "turn/queueUpdated",
    "turn/retrying",
    "turn/started",
    "vibeCode/teleport/event",
    "warning",
];

/// Which handoff notification a session rotation publishes, with the one field
/// that name adds to the shared handoff body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HandoffNotice {
    Compacted { summary_length: usize },
    ContextCleared { plan_file_path: Option<String> },
}

/// A brokered connector authorization: session, connector, accepted revision
/// and action.
type ConnectorAuthRequest = (String, String, String, String);

/// The parameter a routed connector call carries the connection's root in,
/// which no client can send: the parameters were validated before it is added.
pub(crate) const CONNECTION_ROOT_PARAM: &str = "\u{0}connectionRoot";

/// The parameter a routed narration call carries the client's launch context
/// in, on the same terms as [`CONNECTION_ROOT_PARAM`].
pub(crate) const CONNECTION_LAUNCH_PARAM: &str = "\u{0}connectionLaunch";

/// Every method this build routes, sorted and unique, whether or not the
/// reference declares it.
#[cfg(test)]
pub(crate) fn routed_methods() -> BTreeSet<&'static str> {
    IMPLEMENTED_METHODS
        .iter()
        .chain(WORKSPACE_METHODS)
        .chain(PROJECTS_METHODS)
        .chain(RESOURCE_METHODS)
        .copied()
        .collect()
}

/// Every reference method a stdio client reaches an answer for: the ones a
/// handler of this port serves, and the ones the reference routing answers by
/// itself, as the reference does (`connection::route`).
#[cfg(test)]
pub(crate) fn stdio_routed_methods() -> BTreeSet<&'static str> {
    let served = routed_methods();
    vibe_protocol::SERVER_METHODS
        .iter()
        .copied()
        .filter(|method| {
            served.contains(method)
                || !matches!(
                    connection::route(method),
                    connection::Route::Host
                        | connection::Route::SessionOptional
                        | connection::Route::Root
                        | connection::Route::Catalog
                )
        })
        .collect()
}

/// Gates a notification name on [`EMITTED_NOTIFICATIONS`] as it leaves.
///
/// Both notification funnels pass through here: the frames a dispatch batch
/// encodes and the frames the live projection builds. A name emitted without
/// being declared is a surface the conformance replay would never see.
pub(crate) fn notification_method(method: &str) -> &str {
    debug_assert!(
        EMITTED_NOTIFICATIONS.contains(&method),
        "{method} is emitted but absent from EMITTED_NOTIFICATIONS"
    );
    method
}

/// The namespace this port's own in-process clients address the session
/// shapes they predate the reference surface with (`workspace/internal.rs`).
///
/// A method under it is answered only over [`TransportKind::InProcess`]: it is
/// neither declared nor advertised, and a stdio client naming one is refused
/// as for any unknown method.
pub const INTERNAL_METHOD_PREFIX: &str = "internal/";

/// The methods [`INTERNAL_METHOD_PREFIX`] reaches, by the name the workspace
/// service dispatches them under.
const INTERNAL_METHODS: &[&str] = &[
    "history/list",
    "session/continue",
    "session/delete",
    "session/fork",
    "session/history/clear",
    "session/list",
    "session/log/read",
    "session/resume",
    "session/title/update",
];

/// The workspace method an in-process `method` addresses, if it names one.
pub(crate) fn internal_method(transport: TransportKind, method: &str) -> Option<&str> {
    let method = method.strip_prefix(INTERNAL_METHOD_PREFIX)?;
    (transport == TransportKind::InProcess && INTERNAL_METHODS.contains(&method)).then_some(method)
}

/// The methods this port's own handlers answer, beside the service lists.
#[cfg(test)]
const IMPLEMENTED_METHODS: &[&str] = &[
    "account/read",
    "callback/respond",
    "callback/result",
    "connector_catalog/auth/request",
    "connector_catalog/read",
    "connector_catalog/refresh",
    "connector_catalog/toggle",
    "connectors/auth/read",
    "connectors/read",
    "connectors/refresh",
    "diagnostics/list",
    "diagnostics/logs/read",
    "feedback/record",
    "feedback/shouldShow",
    "mcp/add",
    "mcp/login",
    "mcp/logout",
    "mcp/read",
    "mcp/refresh",
    "mcp/toggle",
    "mcp_catalog/add",
    "mcp_catalog/login",
    "mcp_catalog/logout",
    "mcp_catalog/read",
    "mcp_catalog/refresh",
    "mcp_catalog/remove",
    "mcp_catalog/toggle",
    "narration/summarize",
    "review/approve",
    "review/baseline",
    "review/hunks",
    "review/revert",
    "review/state",
    "review/turnDiff",
    "runtime/read",
    "session/close",
    "session/compact",
    "session/compact/start",
    "session/context/inject",
    "session/continue",
    "session/delete",
    "session/fork",
    "session/history/clear",
    "session/history/get",
    "session/history/list",
    "session/list",
    "session/log/read",
    "session/overrides/write",
    "session/pin",
    "session/read",
    "session/ready/read",
    "session/ready/wait",
    "session/relocate",
    "session/rename",
    "session/resume",
    "session/rewind",
    "session/rewind/read",
    "session/settings/update",
    "session/shellCommand",
    "session/start",
    "session/stop",
    "session/title/update",
    "session/turn/enqueue",
    "session/turn/queue/read",
    "session/turn/queue/remove",
    "session/turn/queue/replace",
    "session/turn/queue/resume",
    "session/turns/list",
    "shell/interrupt",
    "shell/run",
    "skills/catalog",
    "skills/convertLocal",
    "skills/detail",
    "skills/import",
    "skills/installed",
    "skills/remove",
    "skills/setAlias",
    "skills/setEnabled",
    "skills/setLatest",
    "skills/setVersion",
    "skills/updates",
    "skills/versions",
    "stats/read",
    "telemetry/record",
    "tools/list",
    "turn/interrupt",
    "turn/start",
    "turn/steer",
    "workspace/trust/decision",
    "workspace/trust/status",
    "workspace/trust/untrustedConfig",
];

struct DenyApproval;

impl ApprovalAgent for DenyApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::Deny) })
    }
}

struct ApproveOnce;

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

struct ApproveEdits {
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

struct NoAdditionalTools;

impl SessionToolFactory for NoAdditionalTools {
    fn register(&self, _session_id: &str, _tools: &ToolRegistry) -> Result<(), String> {
        Ok(())
    }
}

struct ChainedSessionToolFactory {
    existing: Arc<dyn SessionToolFactory>,
    additional: Arc<dyn SessionToolFactory>,
}

impl SessionToolFactory for ChainedSessionToolFactory {
    fn register(&self, session_id: &str, tools: &ToolRegistry) -> Result<(), String> {
        self.existing.register(session_id, tools)?;
        self.additional.register(session_id, tools)
    }
}

struct DefaultApprovalFactory;

impl ApprovalAgentFactory for DefaultApprovalFactory {
    fn for_session(&self, _session_id: &str, auto_approve: bool) -> Arc<dyn ApprovalAgent> {
        if auto_approve {
            Arc::new(ApproveOnce)
        } else {
            Arc::new(DenyApproval)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    New,
    AwaitingInitialized,
    Ready,
    ShuttingDown,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferredWork {
    RunTurn {
        session_id: String,
        turn_id: String,
        prompt: String,
        input: Vec<PublicContentBlock>,
        injected: bool,
        client_user_message_id: Option<String>,
        auto_title: Option<String>,
        user_display_content: Option<Value>,
        mention_stats: Option<Value>,
    },
    InterruptTurn {
        session_id: String,
        turn_id: String,
    },
    SteerTurn {
        session_id: String,
        turn_id: String,
        content: String,
        inject_invoked_skill: bool,
    },
    InjectContext {
        session_id: String,
        content: String,
        as_message: bool,
        inject_invoked_skill: bool,
    },
    ResolveCallback {
        session_id: String,
        turn_id: String,
        callback_id: String,
        accepted: bool,
        value: Option<String>,
    },
    ResourceRequest {
        request_id: RequestId,
        session_id: String,
        command: ResourceBackendCommand,
    },
    CloudRequest {
        request_id: RequestId,
        method: String,
        params: BTreeMap<String, Value>,
    },
    ConfigureMcp {
        session_id: String,
        configs: Vec<McpServerConfig>,
    },
    /// One MCP catalog call. `publish` says whether a notification it raises
    /// mid-flight reaches the client: the reference publishes nothing to a
    /// connection no session is attached to.
    McpCatalog {
        request_id: RequestId,
        call: McpCatalogCall,
        target: McpCatalogTarget,
        publish: bool,
    },
    /// One `vibeCode/*` call, run against the session's controller. Frames it
    /// publishes after answering go out as they happen.
    VibeCode {
        request_id: RequestId,
        session_id: String,
        method: String,
        params: BTreeMap<String, Value>,
        launch: vibe_code_host::ClientLaunch,
    },
    CompactSession {
        request_id: RequestId,
        session_id: String,
        /// Where the session is written when it has not been yet.
        working_directory: String,
        extra_instructions: String,
    },
    /// A manual `!` command the connection reserved on its session, which
    /// runs outside the request loop and answers once it settles.
    ShellCommand {
        request_id: RequestId,
        session_id: String,
        operation_id: String,
        command: String,
        cwd: String,
        timeout_ms: u64,
    },
    /// An interrupt of a running manual command, answered once the command
    /// has settled, as the reference's waits for the process to exit.
    ShellInterrupt {
        request_id: RequestId,
        session_id: String,
        operation_id: String,
    },
    CloseResources {
        session_id: String,
        generation: u64,
    },
}

/// Where a catalog call's mid-flight notifications go: framed and handed to
/// `deliver` when `publish` holds, dropped otherwise.
pub fn mcp_catalog_notifier(
    publish: bool,
    deliver: impl Fn(Vec<u8>) + Send + Sync + 'static,
) -> McpCatalogNotify {
    Arc::new(move |method, params| {
        if publish {
            deliver(encode_notification(
                notification_method(method),
                params.into_iter().collect(),
            ));
        }
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchBatch {
    pub outbound: Vec<Vec<u8>>,
    pub deferred: Vec<DeferredWork>,
    pub close_after_flush: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledLoopWork {
    pub fire: LoopFire,
    pub work: DeferredWork,
}

impl DispatchBatch {
    fn empty() -> Self {
        Self {
            outbound: Vec::new(),
            deferred: Vec::new(),
            close_after_flush: false,
        }
    }
}

#[derive(Clone)]
pub struct AppServer {
    sessions: Arc<Mutex<SessionRegistry>>,
    resources: Arc<Mutex<ResourceService>>,
    resource_backend: Option<Arc<dyn ResourceBackend>>,
    workspace: Arc<WorkspaceService>,
    projects: Arc<ProjectsService>,
    approval_factory: Arc<dyn ApprovalAgentFactory>,
    session_tool_factory: Arc<dyn SessionToolFactory>,
    builtin_tools: Arc<BuiltinTools>,
    shell_tools: Arc<ShellTools>,
    /// The `clientTool/*` delegation this server's connection offers. Empty
    /// until a client declares a capability, which is what keeps a client that
    /// hosts nothing on the server's own filesystem and terminals.
    pub(crate) client_tools: Arc<ClientToolBridge>,
    /// Where a client-authored event reaches the datalake. The reference hands
    /// it to the agent loop's own telemetry client; the adapter that owns one
    /// installs it here, and a server built without one keeps the event on
    /// `diagnostics/logs/read` alone.
    client_telemetry: Arc<dyn ClientTelemetry>,
    /// The model an `auto` worktree request asks for a name. Without one, the
    /// name comes from the prompt alone, as it does upstream when no key
    /// resolves (`vibe/core/llm/utility_completion.py:22-76`).
    utility_provider: Option<Arc<dyn CompletionProvider>>,
    /// The model a Teleport run summarizes the session with, on the session's
    /// compaction model. Without one, a run with history to summarize fails
    /// the way a failed summarization does.
    secondary_provider: Option<Arc<dyn CompletionProvider>>,
    /// The harness this process resolved, which `config/read` reports.
    harness: Arc<HarnessSelection>,
    next_session: Arc<AtomicU64>,
    next_turn: Arc<AtomicU64>,
    next_callback: Arc<AtomicU64>,
    /// The sessions this process holds open, each under the lease that keeps a
    /// second process from opening it too (reference `SessionLease`).
    leases: Arc<Mutex<BTreeMap<String, vibe_core::storage::lease::SessionLease>>>,
    /// The connector catalogs this process fetched or read back, by provider
    /// fingerprint, with when each was stored (reference
    /// `ConnectorCatalogService._memory`).
    connector_catalogs:
        Arc<Mutex<BTreeMap<String, (crate::connector_catalog::ResolvedCatalog, i64)>>>,
    /// The connector authorizations being brokered, by session, connector,
    /// accepted revision and action (reference `_auth_requests_seen`).
    connector_auth_requests: Arc<Mutex<BTreeSet<ConnectorAuthRequest>>>,
}

impl Default for AppServer {
    fn default() -> Self {
        let home = vibe_home();
        Self {
            sessions: Arc::new(Mutex::new(SessionRegistry::default())),
            // Reference `_resources.py` builds its `LogReader` over `LOG_FILE`
            // at construction. A ceiling the environment spells wrong leaves
            // the reader on the same file with the shipped defaults: reading a
            // log never depends on what writing one resolved.
            resources: Arc::new(Mutex::new(ResourceService::default().logging_to(
                FileLog::in_home(&home, LogSettings::from_environment().unwrap_or_default()),
            ))),
            resource_backend: Some(Arc::new(CoreResourceBackend::default())),
            workspace: Arc::new(WorkspaceService::default()),
            projects: Arc::new(ProjectsService::default()),
            approval_factory: Arc::new(DefaultApprovalFactory),
            session_tool_factory: Arc::new(NoAdditionalTools),
            // The reference resolves the web-search key from the environment or
            // the OS keyring. Only the environment branch is reachable from
            // here; a client holding a keyring credential installs it with
            // [`AppServer::using_web_search_access`]. The environment includes
            // `{vibe_home}/.env`, which the reference startup has folded into
            // the process by this point.
            builtin_tools: Arc::new(BuiltinTools::new(
                home.clone(),
                WebSearchAccess::from_environment(&DotenvValues::global(&home), "MISTRAL_API_KEY"),
            )),
            // The rollout is not resolved here: `register` reads
            // `managed_shell_tools_enabled` off the session's configuration
            // resolver, which follows every load, so a value written after
            // startup still selects the family.
            shell_tools: Arc::new(ShellTools::new(home)),
            client_tools: Arc::new(ClientToolBridge::default()),
            client_telemetry: Arc::new(NoClientTelemetry),
            utility_provider: None,
            secondary_provider: None,
            harness: Arc::new(HarnessSelection::default()),
            next_session: Arc::new(AtomicU64::new(1)),
            next_turn: Arc::new(AtomicU64::new(1)),
            next_callback: Arc::new(AtomicU64::new(1)),
            leases: Arc::new(Mutex::new(BTreeMap::new())),
            connector_catalogs: Arc::new(Mutex::new(BTreeMap::new())),
            connector_auth_requests: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }
}

impl AppServer {
    #[must_use]
    pub fn with_resource_backend(backend: Arc<dyn ResourceBackend>) -> Self {
        Self {
            resource_backend: Some(backend),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_workspace_service(service: WorkspaceService) -> Self {
        Self::default().using_workspace_service(service)
    }

    #[must_use]
    pub fn with_projects_service(service: ProjectsService) -> Self {
        Self {
            projects: Arc::new(service),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn using_workspace_service(self, service: WorkspaceService) -> Self {
        // The log file follows the home the service reads under, so a server
        // built for another home neither writes to nor answers from the
        // operator's.
        let log = FileLog::in_home(
            service.vibe_home(),
            LogSettings::from_environment().unwrap_or_default(),
        );
        // Upstream loads its trust store when the module is imported, which
        // writes an empty trust file into a home that has none; a host opens
        // it here for the same reason, before any client asks.
        let _ = vibe_core::trust::TrustStore::for_vibe_home(service.vibe_home());
        let mut server = self.logging_to(log);
        server.workspace = Arc::new(service);
        server
    }

    /// The file this server records to and answers `diagnostics/logs/read`
    /// from. Reference builds one `LogReader` over `LOG_FILE` per resource set;
    /// naming the file here is what lets a host, and a test, keep the operator's
    /// out of it.
    #[must_use]
    pub fn logging_to(self, log: FileLog) -> Self {
        if let Ok(mut resources) = self.resources.lock() {
            resources.set_log(log);
        }
        self
    }

    #[must_use]
    pub fn using_projects_service(mut self, service: ProjectsService) -> Self {
        self.projects = Arc::new(service);
        self
    }

    /// Installs where `telemetry/record` ships a client-authored event.
    ///
    /// The reference reaches the agent loop's telemetry client from the same
    /// resource set that serves the method
    /// (`vibe/app_server/_resources.py:488-499`). Nothing here decides whether
    /// the event travels: the sink re-reads `enable_telemetry` and the Mistral
    /// provider on every send, exactly as an event the turn itself produced
    /// does.
    #[must_use]
    pub fn using_client_telemetry(mut self, telemetry: Arc<dyn ClientTelemetry>) -> Self {
        self.client_telemetry = telemetry;
        self
    }

    /// Installs the harness decision the launch resolved from its flags.
    #[must_use]
    pub fn using_harness_selection(mut self, harness: HarnessSelection) -> Self {
        self.harness = Arc::new(harness);
        self
    }

    /// The harness decision `config/read` reports.
    #[must_use]
    pub fn harness_selection(&self) -> &HarnessSelection {
        &self.harness
    }

    /// Installs the model utility completions run on, which names the
    /// worktrees `auto` requests raise.
    #[must_use]
    pub fn using_utility_provider(mut self, provider: Option<Arc<dyn CompletionProvider>>) -> Self {
        self.utility_provider = provider;
        self
    }

    /// The provider a Teleport run's summarization calls.
    #[must_use]
    pub fn using_secondary_provider(
        mut self,
        provider: Option<Arc<dyn CompletionProvider>>,
    ) -> Self {
        self.secondary_provider = provider;
        self
    }

    /// [`Self::using_secondary_provider`], unless one is installed already.
    #[must_use]
    pub(crate) fn defaulting_secondary_provider(
        self,
        provider: Option<Arc<dyn CompletionProvider>>,
    ) -> Self {
        if self.secondary_provider.is_some() {
            return self;
        }
        self.using_secondary_provider(provider)
    }

    /// Installs what answers the approvals a session's tools ask for.
    #[must_use]
    pub(crate) fn using_approval_factory(
        mut self,
        approval_factory: Arc<dyn ApprovalAgentFactory>,
    ) -> Self {
        self.approval_factory = approval_factory;
        self
    }

    #[must_use]
    pub fn using_session_tool_factory(
        mut self,
        session_tool_factory: Arc<dyn SessionToolFactory>,
    ) -> Self {
        self.session_tool_factory = Arc::new(ChainedSessionToolFactory {
            existing: self.session_tool_factory,
            additional: session_tool_factory,
        });
        self
    }

    /// Installs the credential `web_search` reaches the endpoint with, or
    /// withholds the tool when `None`.
    ///
    /// The reference publishes `web_search` only when a Mistral key resolves,
    /// and a client that read its key from the OS keyring is the only party
    /// that can hand it down.
    #[must_use]
    pub fn using_web_search_access(mut self, access: Option<WebSearchAccess>) -> Self {
        self.builtin_tools = Arc::new(self.builtin_tools.as_ref().clone().with_web_search(access));
        self
    }

    #[must_use]
    pub fn using_surface_extension(
        mut self,
        approval_factory: Arc<dyn ApprovalAgentFactory>,
        session_tool_factory: Arc<dyn SessionToolFactory>,
    ) -> Self {
        self.approval_factory = approval_factory;
        self.session_tool_factory = Arc::new(ChainedSessionToolFactory {
            existing: self.session_tool_factory,
            additional: session_tool_factory,
        });
        self
    }

    #[must_use]
    /// The configuration and session service this server composes over.
    ///
    /// Handed back so an adapter can attach a runtime that has to read and
    /// write the *same* configuration the server's own sessions read, which is
    /// what a resolved rollout depends on: a second service would compose a
    /// second document.
    pub fn workspace_service(&self) -> WorkspaceService {
        self.workspace.as_ref().clone()
    }

    pub fn connect(&self, transport: TransportKind) -> ServerConnection {
        ServerConnection {
            server: self.clone(),
            state: ConnectionState::New,
            transport,
            capabilities: ClientCapabilities::default(),
            attached_sessions: BTreeSet::new(),
            root: None,
            entrypoint: ClientEntrypoint::Unknown,
            client_name: None,
            launch: None,
            pending_server_requests: HashMap::new(),
        }
    }

    pub fn session(&self, session_id: &str) -> Result<SessionView, ServerError> {
        let sessions = self.lock_sessions()?;
        sessions
            .get(session_id)
            .map(SessionView::from)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))
    }

    pub(crate) fn tool_registry(&self, session_id: &str) -> Result<ToolRegistry, ServerError> {
        let sessions = self.lock_sessions()?;
        sessions
            .get(session_id)
            .map(|session| session.tools.clone())
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))
    }

    fn lock_sessions(&self) -> Result<std::sync::MutexGuard<'_, SessionRegistry>, ServerError> {
        self.sessions.lock().map_err(|_| ServerError::StatePoisoned)
    }
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("connection is not initialized")]
    NotInitialized,
    #[error("session `{0}` was not found")]
    SessionNotFound(String),
    #[error("session `{0}` already exists")]
    SessionConflict(String),
    #[error("projects workflow failed: {0}")]
    Projects(String),
    #[error("turn `{0}` is stale")]
    StaleTurn(String),
    #[error("deferred work reserved no turn")]
    MissingTurnReservation,
    #[error("turn completion is not terminal: {0:?}")]
    NonTerminalCompletion(LifecycleState),
    #[error("another callback is already pending")]
    CallbackConflict,
    #[error("callback kind `{0:?}` is not supported")]
    UnsupportedCallbackKind(EngineCallbackKind),
    #[error("client does not support callback kind `{0:?}`")]
    UnsupportedClientCallbackKind(EngineCallbackKind),
    #[error("callback request did not encode as a server request")]
    InvalidCallbackRequest,
    #[error("callback detail is invalid: {0}")]
    InvalidCallbackDetail(String),
    #[error("server state lock is poisoned")]
    StatePoisoned,
    #[error("tool execution failed: {0}")]
    Tool(String),
    #[error("resource cleanup failed: {0}")]
    Resource(String),
    #[error(transparent)]
    Json(serde_json::Error),
    #[error(transparent)]
    Protocol(#[from] vibe_protocol::ProtocolValidationError),
}

/// Why a request's parameters were rejected, with the path to the value that
/// caused it.
///
/// Serde stops at the first violation, so `issues` carries one entry; the shape
/// is a list because the contract is a list and a client reads `errorCount`
/// rather than assuming.
pub(crate) struct ParamsRejection {
    message: String,
    issues: Vec<InvalidParamsIssue>,
}

impl ParamsRejection {
    /// A rejection a dispatcher raised by hand rather than through a
    /// deserializer, so there is no traversal to read a path from and the issue
    /// sits at the parameter object itself.
    ///
    /// The detail is still structured: a client reads `errorCount` and `issues`
    /// on every `invalid_params`, and an empty path says the failure is about
    /// the object rather than about a value inside it.
    /// A rejection reporting every violation the reference's model reports,
    /// under the message its request validation raises.
    fn with_issues(issues: Vec<InvalidParamsIssue>) -> Self {
        Self {
            message: "Invalid request parameters".to_owned(),
            issues,
        }
    }

    fn at_root(message: String) -> Self {
        Self {
            issues: vec![InvalidParamsIssue {
                path: Vec::new(),
                message: message.clone(),
            }],
            message,
        }
    }
}

fn from_params<T: for<'de> Deserialize<'de>>(
    params: &BTreeMap<String, Value>,
) -> Result<T, ParamsRejection> {
    let value = Value::Object(
        params
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    serde_path_to_error::deserialize(value).map_err(|error| {
        let path = error
            .path()
            .iter()
            .filter_map(|segment| match segment {
                serde_path_to_error::Segment::Seq { index } => Some(PathSegment::Index(*index)),
                serde_path_to_error::Segment::Map { key }
                | serde_path_to_error::Segment::Enum { variant: key } => {
                    Some(PathSegment::Field(key.clone()))
                }
                serde_path_to_error::Segment::Unknown => None,
            })
            .collect();
        let message = error.into_inner().to_string();
        ParamsRejection {
            issues: vec![InvalidParamsIssue {
                path,
                message: message.clone(),
            }],
            message,
        }
    })
}

fn result_map<const N: usize>(entries: [(&str, Value); N]) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn next_event_id(session: &mut SessionRuntime) -> u64 {
    session.event_watermark = session.event_watermark.saturating_add(1);
    session.event_watermark
}

/// Publishes the session's status as a sequenced `session/updated`.
///
/// Every server-side status transition mints one. The client's projection reads
/// the per-session `eventId` as a contiguous counter, so a transition that
/// advanced the watermark without publishing anything would read to it as a gap
/// rather than as silence.
fn session_updated_frame(session: &mut SessionRuntime) -> Vec<u8> {
    let status = public_session_status(session);
    let updated_at = session.updated_at;
    let event_id = next_event_id(session);
    encode_notification(
        "session/updated",
        result_map([
            ("eventId", json!(event_id)),
            ("sessionId", json!(session.id)),
            (
                "patch",
                json!([
                    {"op": "replace", "path": "/status", "value": status},
                    {"op": "replace", "path": "/updatedAt", "value": updated_at},
                ]),
            ),
            ("emittedAt", json!(now_millis())),
        ]),
    )
}

/// Publishes the whole session as a sequenced `session/snapshot`.
///
/// The watermark is advanced before the state is projected so the embedded
/// `state.eventId` equals the notification's own, which the client's projection
/// asserts before it replaces its state.
fn session_snapshot_frame(session: &mut SessionRuntime) -> Vec<u8> {
    let event_id = next_event_id(session);
    let state = public_session_state(session);
    encode_notification(
        "session/snapshot",
        result_map([
            ("eventId", json!(event_id)),
            ("sessionId", json!(session.id)),
            ("state", state),
            ("emittedAt", json!(now_millis())),
        ]),
    )
}

/// The outbound frames a rejected callback delivery produces.
///
/// [`AppServer::reject_callback`] answers with an empty frame when no callback
/// was pending, because there was no transition to publish.
fn rejection_frames(status: Vec<u8>) -> Vec<Vec<u8>> {
    if status.is_empty() {
        Vec::new()
    } else {
        vec![status]
    }
}

fn object(value: Value) -> BTreeMap<String, Value> {
    value
        .as_object()
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Reference `uuid4()`: a new session is named by a random UUID, whose first
/// eight characters also name its directory.
fn generated_session_id(_sequence: u64) -> String {
    vibe_core::session_id::uuid_v4()
}

#[cfg(test)]
mod server_tests;
