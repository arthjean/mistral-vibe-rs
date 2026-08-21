//! The reference tools that need no shell runtime.
//!
//! `todo`, `web_fetch`, `web_search` and `skill` are published by the reference
//! for every session and need nothing from the workspace, so they register
//! together here rather than alongside the file tools. Each one carries a
//! reference-conformant schema built through [`crate::schema`] and a handler
//! that executes: the registry never publishes a name it cannot serve.
//!
//! `web_search` is the one conditional member. The reference withholds it when
//! no Mistral API key resolves, so a [`BuiltinTools`] built without
//! [`WebSearchAccess`] simply does not register it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::DotenvValues;
use crate::extensions::DiscoveryRoots;
use crate::policy::{
    PermissionContext, PermissionMode, PermissionRequirement, PolicyGuardedTool, ToolGuard,
};
use crate::skills::SkillDiscovery;
use crate::tools::config::{SharedToolConfig, TodoConfig, ToolConfigResolver, declared_document};
use crate::tools::{
    OwnedToolHandlerFuture, RegistrationOutcome, ToolError, ToolHandler, ToolInvocation,
    ToolOutputSink, ToolRegistry, reference_text,
};

/// The redirect budget the security NFR sets for `web_fetch`.
mod skill;
mod todo;
mod web_fetch;
mod web_search;

use skill::{SkillInvocationResolver, run_skill, skill_spec};
#[cfg(test)]
use skill::{render_skill, skill_files};
use todo::{run_todo, todo_spec};
#[cfg(test)]
use web_fetch::{
    FETCH_ACCEPT, FETCH_ACCEPT_LANGUAGE, HONEST_USER_AGENT, fetch_timeout, html_to_text,
};
use web_fetch::{fetch_url, web_fetch_handler, web_fetch_spec};
#[cfg(test)]
use web_search::parse_search_response;
use web_search::{web_search_handler, web_search_spec};

/// How many hops `web_fetch` follows before it refuses the chain.
///
/// The reference leaves its HTTP client at the default, so a smaller budget
/// here would refuse a page the reference reads. The bound still exists: a
/// redirect loop terminates rather than running until the timeout.
const MAX_FETCH_REDIRECTS: usize = 20;
/// How many skill names an unknown-skill error lists before it truncates.
const MAX_LISTED_SKILLS: usize = 40;
/// Reference `skill.py:_MAX_LISTED_FILES`.
const MAX_LISTED_SKILL_FILES: usize = 10;

/// The agent `web_search` presents itself with, keeping the SDK prefix the
/// reference sends ahead of this port's own product name.
const SEARCH_USER_AGENT: &str = concat!(
    "mistral-client-python/mistral-vibe-rs/",
    env!("CARGO_PKG_VERSION")
);

/// What `web_search` needs to reach the Mistral conversations endpoint.
///
/// The reference resolves its key from the environment or the OS keyring, both
/// of which live above this crate, so the caller resolves it and hands the
/// result down. `None` at the [`BuiltinTools`] level means the reference
/// availability rule withholds the tool.
#[derive(Clone)]
pub struct WebSearchAccess {
    /// The API base, for example `https://api.mistral.ai`.
    pub endpoint: String,
    pub api_key: SecretString,
}

impl std::fmt::Debug for WebSearchAccess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebSearchAccess")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl WebSearchAccess {
    /// Reference `DEFAULT_MISTRAL_API_ENV_KEY`.
    pub const DEFAULT_ENDPOINT: &'static str = "https://api.mistral.ai";

    /// The access the ambient key grants, or `None` when the variable is unset
    /// or empty, matching the reference `resolve_api_key` environment branch.
    ///
    /// `dotenv` carries the global dotenv file, which the reference has already
    /// loaded into the process environment by the time it resolves this key; a
    /// key kept there therefore enables `web_search` here too.
    #[must_use]
    pub fn from_environment(dotenv: &DotenvValues, variable: &str) -> Option<Self> {
        let key = dotenv.variable(variable).filter(|key| !key.is_empty())?;
        Some(Self {
            endpoint: Self::DEFAULT_ENDPOINT.to_owned(),
            api_key: SecretString::from(key),
        })
    }
}

/// One entry of the model-facing todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    #[serde(default = "TodoItem::default_status")]
    pub status: String,
    #[serde(default = "TodoItem::default_priority")]
    pub priority: String,
}

impl TodoItem {
    /// Reference `TodoStatus` members, in declaration order.
    const STATUSES: [&'static str; 4] = ["pending", "in_progress", "completed", "cancelled"];
    /// Reference `TodoPriority` members, in declaration order.
    const PRIORITIES: [&'static str; 3] = ["low", "medium", "high"];

    fn default_status() -> String {
        "pending".to_owned()
    }

    fn default_priority() -> String {
        "medium".to_owned()
    }

    /// The entry as one dictionary of the model-facing rendering.
    fn rendered_fields(&self) -> Vec<(&'static str, String)> {
        vec![
            ("id", reference_text::string_repr(&self.id)),
            ("content", reference_text::string_repr(&self.content)),
            ("status", reference_text::string_repr(&self.status)),
            ("priority", reference_text::string_repr(&self.priority)),
        ]
    }
}

/// The universal builtin tools, and the session state they keep.
///
/// One instance serves every session: the todo list is keyed by session id, so
/// re-registering a session's tools after an agent switch finds the list it
/// left behind rather than an empty one.
#[derive(Clone)]
pub struct BuiltinTools {
    vibe_home: PathBuf,
    web_search: Option<WebSearchAccess>,
    todos: Arc<Mutex<BTreeMap<String, Vec<TodoItem>>>>,
    /// Which skills each session has already loaded, so a second request for
    /// one is acknowledged instead of rendered again.
    loaded_skills: Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>,
}

impl std::fmt::Debug for BuiltinTools {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BuiltinTools")
            .field("vibe_home", &self.vibe_home)
            .field("web_search", &self.web_search.is_some())
            .finish_non_exhaustive()
    }
}

impl BuiltinTools {
    #[must_use]
    pub fn new(vibe_home: impl Into<PathBuf>, web_search: Option<WebSearchAccess>) -> Self {
        Self {
            vibe_home: vibe_home.into(),
            web_search,
            todos: Arc::new(Mutex::new(BTreeMap::new())),
            loaded_skills: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// The same tools reaching the endpoint with another credential, or none.
    ///
    /// The todo state is shared rather than reset: swapping the credential is
    /// a configuration change, not a new session.
    #[must_use]
    pub fn with_web_search(mut self, access: Option<WebSearchAccess>) -> Self {
        self.web_search = access;
        self
    }

    /// Publishes the universal tools for one session.
    ///
    /// `skills` reaches the skill catalog, which is discovered per session
    /// because a project may ship its own skills and because the roots and the
    /// filters both come from the merged configuration the session opened with.
    pub fn register(
        &self,
        session_id: &str,
        skills: SkillDiscovery,
        registry: &ToolRegistry,
        guard: &ToolGuard,
    ) -> Result<Vec<RegistrationOutcome>, ToolError> {
        let ToolGuard {
            policy,
            approval,
            config,
            scratchpad: _,
        } = guard;
        let mut outcomes = vec![
            // `todo` and `skill` are configured `always` upstream, and the
            // guard is what reads that: a session or an operator moving either
            // one to `ask` is obeyed without the handler knowing about policy.
            registry.register(
                todo_spec(),
                Arc::new(PolicyGuardedTool::new(
                    "todo",
                    policy.clone(),
                    approval.clone(),
                    Arc::new(|_invocation| Ok(PermissionContext::deferred())),
                    self.todo_handler(session_id, config),
                )),
            )?,
            registry.register(
                skill_spec(),
                Arc::new(PolicyGuardedTool::new(
                    "skill",
                    policy.clone(),
                    approval.clone(),
                    // Reference `SkillTool.resolve_permission` grants outright
                    // rather than deferring, so a session that moved `skill` to
                    // `ask` still loads a skill without a prompt.
                    Arc::new(|_invocation| Ok(PermissionContext::settled(PermissionMode::Always))),
                    self.skill_handler(session_id, skills.clone()),
                )),
            )?,
            registry.register(
                web_fetch_spec(),
                Arc::new(PolicyGuardedTool::new(
                    "web_fetch",
                    policy.clone(),
                    approval.clone(),
                    // Reference `WebFetchTool.resolve_permission`: a configured
                    // `always` or `never` settles the call, and everything else
                    // asks for the host the fetch reaches.
                    {
                        let settings = config.clone();
                        Arc::new(move |invocation: &ToolInvocation| {
                            let configured = settings.view::<SharedToolConfig>("web_fetch");
                            if configured.permission != PermissionMode::Ask {
                                return Ok(PermissionContext::settled(configured.permission));
                            }
                            let url = fetch_url(&invocation.arguments)?;
                            let Some(domain) = url.host_str() else {
                                return Ok(PermissionContext::deferred());
                            };
                            Ok(PermissionContext::asking(vec![
                                PermissionRequirement::url_domain(domain),
                            ]))
                        })
                    },
                    web_fetch_handler(config.clone()),
                )),
            )?,
        ];
        if let Some(access) = &self.web_search {
            let access = access.clone();
            // The endpoint is still parsed here rather than at the first call:
            // a malformed one is a registration failure, not a turn failure.
            Url::parse(&access.endpoint).map_err(|error| {
                ToolError::Execution(format!(
                    "the web search endpoint `{}` is not a URL: {error}",
                    access.endpoint
                ))
            })?;
            outcomes.push(registry.register(
                web_search_spec(),
                Arc::new(PolicyGuardedTool::new(
                    "web_search",
                    policy.clone(),
                    approval.clone(),
                    // The reference declares no `resolve_permission` for
                    // `web_search`, so the configured permission is the whole
                    // decision and an approval for the session grants the tool
                    // rather than one host.
                    Arc::new(|_invocation| Ok(PermissionContext::deferred())),
                    web_search_handler(access, config.clone()),
                )),
            )?);
        }
        // The synthetic pair a slash invocation appends resolves against the
        // same discovery and the same loaded ledger the `skill` tool answers
        // from, so the two paths cannot disagree about what exists or about
        // what is already loaded.
        registry.set_invoked_skills(Arc::new(SkillInvocationResolver {
            roots: DiscoveryRoots {
                skills,
                ..DiscoveryRoots::default()
            },
            loaded: self.loaded_skills.clone(),
            session_id: session_id.to_owned(),
        }));
        Ok(outcomes)
    }

    fn todo_handler(&self, session_id: &str, config: &ToolConfigResolver) -> Arc<dyn ToolHandler> {
        let todos = self.todos.clone();
        let session_id = session_id.to_owned();
        let config = config.clone();
        Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let todos = todos.clone();
                let session_id = session_id.clone();
                let settings: TodoConfig = config.view("todo");
                let arguments = invocation.arguments.clone();
                Box::pin(async move { run_todo(&todos, &session_id, &arguments, &settings) })
            },
        )
    }

    fn skill_handler(&self, session_id: &str, skills: SkillDiscovery) -> Arc<dyn ToolHandler> {
        let roots = DiscoveryRoots {
            skills,
            ..DiscoveryRoots::default()
        };
        let loaded = self.loaded_skills.clone();
        let session_id = session_id.to_owned();
        Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let roots = roots.clone();
                let loaded = loaded.clone();
                let session_id = session_id.clone();
                let name = invocation.arguments["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                Box::pin(async move { run_skill(&roots, &loaded, &session_id, &name) })
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use secrecy::ExposeSecret as _;
    use serde_json::{Value, json};

    use crate::tools::config::{WebFetchConfig, WebSearchConfig};

    use crate::extensions::SkillDefinition;
    use crate::policy::{ApprovalAgent, PermissionStore};
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    use tempfile::tempdir;

    use super::*;
    use crate::policy::{
        ApprovalDecision, ApprovalFuture, ApprovalRequest, TrustDecision, TrustRootKind,
    };
    use crate::tools::ToolRegistry;

    struct RejectApproval;

    impl ApprovalAgent for RejectApproval {
        fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
            Box::pin(async { Ok(ApprovalDecision::Deny) })
        }
    }

    /// Stands in for the operator granting a network reach, which the policy
    /// asks for on every `web_fetch` and `web_search` call.
    struct AllowApproval;

    impl ApprovalAgent for AllowApproval {
        fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
            Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
        }
    }

    /// A trusted root with the universal tools registered and every approval
    /// refused, so anything reaching the approval path fails loudly.
    async fn registered(root: &Path, access: Option<WebSearchAccess>) -> ToolRegistry {
        registered_with(root, access, Arc::new(RejectApproval)).await
    }

    async fn registered_with(
        root: &Path,
        access: Option<WebSearchAccess>,
        approval: Arc<dyn ApprovalAgent>,
    ) -> ToolRegistry {
        let policy = PermissionStore::default();
        policy
            .set_trust(root, TrustDecision::Trusted, TrustRootKind::Workspace)
            .await
            .expect("trust");
        let registry = ToolRegistry::default();
        BuiltinTools::new(root, access)
            .register(
                "session-1",
                trusted_skills(root),
                &registry,
                &ToolGuard::new(policy, approval),
            )
            .expect("register");
        registry
    }

    /// The skill discovery a trusted session opened at `root` resolves, which
    /// is the wiring `AppServer::register_session_tools` hands `register`.
    fn trusted_skills(root: &Path) -> SkillDiscovery {
        let projects = vec![root.to_path_buf()];
        let vibe_home = root.join(".vibe");
        SkillDiscovery {
            roots: crate::skills::search_paths(&crate::skills::SearchInputs {
                configured: &[],
                projects: &projects,
                vibe_home: &vibe_home,
                user_home: None,
                working_directory: root,
            }),
            ..SkillDiscovery::default()
        }
    }

    /// The declared `todo` configuration, which is what the handler resolves
    /// when nothing overrides it.
    fn todo_settings() -> TodoConfig {
        ToolConfigResolver::new().view("todo")
    }

    fn names(registry: &ToolRegistry) -> Vec<String> {
        registry
            .list()
            .expect("list")
            .into_iter()
            .map(|spec| spec.name)
            .collect()
    }

    fn probe_access(endpoint: String) -> WebSearchAccess {
        WebSearchAccess {
            endpoint,
            api_key: SecretString::from("probe-key"),
        }
    }

    /// Serves one canned HTTP response per accepted connection and stops.
    fn serve_once(responses: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        let address = listener.local_addr().expect("listener address").to_string();
        std::thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buffer = [0_u8; 4096];
                let _ = stream.read(&mut buffer);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{address}")
    }

    /// The same fixture, handing back what the client sent so a test can assert
    /// on the request rather than only on the answer.
    fn serve_once_recording(response: String) -> (String, std::sync::mpsc::Receiver<String>) {
        serve_recording(vec![response])
    }

    /// One canned response per accepted connection, every request recorded in
    /// the order it arrived, so a test can read a retry as well as a first try.
    fn serve_recording(responses: Vec<String>) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        let address = listener.local_addr().expect("listener address").to_string();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buffer = [0_u8; 8192];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let _ = sender.send(String::from_utf8_lossy(&buffer[..read]).into_owned());
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://{address}"), receiver)
    }

    /// How long a test waits before concluding that no further request is
    /// coming. The fixture thread stays parked on `accept` when a canned
    /// response goes unused, so a plain `recv` would never return.
    const SETTLE: Duration = Duration::from_millis(250);

    /// A 403 that carries the challenge marker, which the reference answers by
    /// retrying once under an agent that names itself.
    fn challenge_response() -> String {
        "HTTP/1.1 403 Forbidden\r\ncf-mitigated: challenge\r\nContent-Length: 0\r\nConnection: \
         close\r\n\r\n"
            .to_owned()
    }

    fn http_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: \
             close\r\n\r\n{body}",
            body.len()
        )
    }

    fn redirect_response(location: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: \
             close\r\n\r\n"
        )
    }

    /// The reference withholds `web_search` when no Mistral key resolves and
    /// publishes the other three unconditionally.
    #[tokio::test]
    async fn web_search_is_published_only_when_a_credential_resolves() {
        let directory = tempdir().expect("tempdir");
        let without = registered(directory.path(), None).await;
        assert_eq!(names(&without), ["skill", "todo", "web_fetch"]);

        let with = registered(
            directory.path(),
            Some(probe_access(WebSearchAccess::DEFAULT_ENDPOINT.to_owned())),
        )
        .await;
        assert_eq!(names(&with), ["skill", "todo", "web_fetch", "web_search"]);
    }

    /// A key kept in `{vibe_home}/.env` resolves like an exported one, which is
    /// what the reference sees after its startup folds the file into the
    /// process environment.
    #[tokio::test]
    async fn a_dotenv_key_grants_web_search_access() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(
            directory.path().join(".env"),
            "VIBE_WEB_SEARCH_FIXTURE_KEY=from-file\n",
        )
        .expect("dotenv fixture");
        let dotenv = DotenvValues::global(directory.path());

        let access = WebSearchAccess::from_environment(&dotenv, "VIBE_WEB_SEARCH_FIXTURE_KEY")
            .expect("the file's key resolves");
        assert_eq!(access.api_key.expose_secret(), "from-file");
        assert_eq!(access.endpoint, WebSearchAccess::DEFAULT_ENDPOINT);

        assert!(
            WebSearchAccess::from_environment(&dotenv, "VIBE_WEB_SEARCH_ABSENT_KEY").is_none(),
            "a variable neither the process nor the file sets grants nothing"
        );

        let registry = registered(directory.path(), Some(access)).await;
        assert!(names(&registry).contains(&"web_search".to_owned()));
    }

    /// The status and priority members and their order, which the model reads
    /// straight out of the published schema.
    #[tokio::test]
    async fn the_todo_enums_carry_the_reference_members_in_order() {
        let directory = tempdir().expect("tempdir");
        let registry = registered(directory.path(), None).await;
        let schema = registry
            .list()
            .expect("list")
            .into_iter()
            .find(|spec| spec.name == "todo")
            .expect("todo is published")
            .input_schema;
        assert_eq!(
            schema["$defs"]["TodoStatus"]["enum"],
            json!(["pending", "in_progress", "completed", "cancelled"])
        );
        assert_eq!(
            schema["$defs"]["TodoPriority"]["enum"],
            json!(["low", "medium", "high"])
        );
    }

    /// The todo round trip through the registry, which is where schema
    /// defaults are applied before the handler sees the arguments.
    #[tokio::test]
    async fn the_registered_todo_tool_round_trips_a_written_list() {
        let directory = tempdir().expect("tempdir");
        let registry = registered(directory.path(), None).await;
        registry
            .invoke(
                "todo",
                ToolInvocation {
                    call_id: "todo-1".to_owned(),
                    arguments: json!({
                        "action": "write",
                        "todos": [{"id": "a", "content": "ship", "status": "in_progress"}],
                    }),
                },
            )
            .await
            .expect("write");
        let read = registry
            .invoke(
                "todo",
                ToolInvocation {
                    call_id: "todo-2".to_owned(),
                    arguments: json!({"action": "read"}),
                },
            )
            .await
            .expect("read");
        // The transcript renders the todo widget from the typed result.
        assert_eq!(
            read.typed_result["todos"],
            json!([{"id": "a", "content": "ship", "status": "in_progress", "priority": "medium"}])
        );
        // The model reads the result the way the reference renders one: a
        // `field: value` line per declared field, the list included.
        assert_eq!(read.typed_result["message"], "Retrieved 1 todos");
        assert_eq!(
            read.model_text,
            "verb: Retrieved\ntodos: [{'id': 'a', 'content': 'ship', 'status': 'in_progress', \
             'priority': 'medium'}]\ntotal_count: 1\nmessage: Retrieved 1 todos"
        );
    }

    /// An unknown skill answers with the names that do exist, so a model that
    /// guessed can correct itself without another round trip.
    #[tokio::test]
    async fn an_unknown_skill_reports_the_names_that_exist() {
        let directory = tempdir().expect("tempdir");
        let skill_directory = directory.path().join(".vibe/skills/probe");
        std::fs::create_dir_all(&skill_directory).expect("skill directory");
        std::fs::write(
            skill_directory.join("SKILL.md"),
            "---\nname: probe\ndescription: a probe\n---\nDo the probing.\n",
        )
        .expect("skill file");
        std::fs::write(skill_directory.join("helper.py"), "pass\n").expect("helper");
        let registry = registered(directory.path(), None).await;

        let loaded = registry
            .invoke(
                "skill",
                ToolInvocation {
                    call_id: "skill-1".to_owned(),
                    arguments: json!({"name": "probe"}),
                },
            )
            .await
            .expect("a discovered skill loads");
        assert!(loaded.model_text.contains("Do the probing."), "{loaded:?}");
        assert!(
            loaded.model_text.contains("<file>helper.py</file>"),
            "{loaded:?}"
        );

        let missing = registry
            .invoke(
                "skill",
                ToolInvocation {
                    call_id: "skill-2".to_owned(),
                    arguments: json!({"name": "absent"}),
                },
            )
            .await
            .expect_err("an unknown skill is refused");
        assert!(missing.to_string().contains("absent"), "{missing}");
        assert!(missing.to_string().contains("probe"), "{missing}");
    }

    /// US-167: the filter runs inside the same catalog build the tool reads, so
    /// a skill `disabled_skills` withholds is not found by the model either.
    /// Filtering and lookup cannot disagree.
    #[tokio::test]
    async fn a_filtered_skill_is_invisible_to_the_skill_tool() {
        let directory = tempdir().expect("tempdir");
        let skills = directory.path().join(".vibe/skills");
        for name in ["probe", "withheld"] {
            std::fs::create_dir_all(skills.join(name)).expect("skill directory");
            std::fs::write(
                skills.join(name).join("SKILL.md"),
                format!("---\nname: {name}\ndescription: a {name}\n---\nDo the {name}ing.\n"),
            )
            .expect("skill file");
        }
        let policy = PermissionStore::default();
        policy
            .set_trust(
                directory.path(),
                TrustDecision::Trusted,
                TrustRootKind::Workspace,
            )
            .await
            .expect("trust");
        let registry = ToolRegistry::default();
        let mut discovery = trusted_skills(directory.path());
        discovery.disabled = vec!["with*".to_owned()];
        BuiltinTools::new(directory.path(), None)
            .register(
                "session-1",
                discovery,
                &registry,
                &ToolGuard::new(policy, Arc::new(RejectApproval)),
            )
            .expect("register");

        registry
            .invoke(
                "skill",
                ToolInvocation {
                    call_id: "skill-1".to_owned(),
                    arguments: json!({"name": "probe"}),
                },
            )
            .await
            .expect("the published skill still loads");
        let refused = registry
            .invoke(
                "skill",
                ToolInvocation {
                    call_id: "skill-2".to_owned(),
                    arguments: json!({"name": "withheld"}),
                },
            )
            .await
            .expect_err("a withheld skill is not found");
        assert!(!refused.to_string().contains("withheld,"), "{refused}");
        assert!(refused.to_string().contains("probe"), "{refused}");
    }

    /// US-168: the catalog's issue list reaches the model rather than being
    /// discarded, so a skill missing because its own file will not parse is
    /// distinguishable from one that was never written.
    #[tokio::test]
    async fn an_unloadable_skill_is_named_when_a_lookup_misses() {
        let directory = tempdir().expect("tempdir");
        let broken = directory.path().join(".vibe/skills/broken");
        std::fs::create_dir_all(&broken).expect("skill directory");
        std::fs::write(broken.join("SKILL.md"), "no frontmatter here\n").expect("skill file");
        let registry = registered(directory.path(), None).await;

        let missing = registry
            .invoke(
                "skill",
                ToolInvocation {
                    call_id: "skill-1".to_owned(),
                    arguments: json!({"name": "broken"}),
                },
            )
            .await
            .expect_err("the unloadable skill is not in the catalog");

        let message = missing.to_string();
        assert!(message.contains("could not be loaded"), "{message}");
        assert!(message.contains("broken/SKILL.md"), "{message}");
    }

    /// US-115: the second request for a skill already in the conversation is
    /// acknowledged rather than rendered again, and it still names the
    /// directory so a relative path in the instructions still resolves.
    #[tokio::test]
    async fn a_skill_loaded_twice_is_acknowledged_rather_than_rendered_again() {
        let directory = tempdir().expect("tempdir");
        let skill_directory = directory.path().join(".vibe/skills/probe");
        std::fs::create_dir_all(skill_directory.join("references")).expect("skill directory");
        std::fs::write(
            skill_directory.join("SKILL.md"),
            "---\nname: probe\ndescription: a probe\n---\nDo the probing.\n",
        )
        .expect("skill file");
        std::fs::write(skill_directory.join("references/api.md"), "api\n").expect("nested");
        let registry = registered(directory.path(), None).await;

        let first = registry
            .invoke(
                "skill",
                ToolInvocation {
                    call_id: "skill-1".to_owned(),
                    arguments: json!({"name": "probe"}),
                },
            )
            .await
            .expect("the first load renders the body");
        assert!(first.model_text.contains("Do the probing."), "{first:?}");
        // The walk is recursive and the names are relative to the base.
        assert!(
            first.model_text.contains("<file>references/api.md</file>"),
            "{first:?}"
        );
        assert!(
            !first.model_text.contains("SKILL.md"),
            "the skill's own file is not one of its attachments: {first:?}"
        );

        let second = registry
            .invoke(
                "skill",
                ToolInvocation {
                    call_id: "skill-2".to_owned(),
                    arguments: json!({"name": "probe"}),
                },
            )
            .await
            .expect("the second load is acknowledged");
        assert!(!second.model_text.contains("Do the probing."), "{second:?}");
        assert!(second.model_text.contains("already loaded"), "{second:?}");
        assert_eq!(
            second.typed_result["skill_dir"],
            json!(skill_directory.to_string_lossy().replace('\\', "/"))
        );
    }

    /// US-169: the seeded builtins are loadable by name with nothing on disk,
    /// and a builtin has no directory, so the base-directory lines and the
    /// `skill_dir` field are omitted rather than naming an empty path.
    #[tokio::test]
    async fn a_builtin_skill_loads_with_no_base_directory() {
        let directory = tempdir().expect("tempdir");
        let registry = registered(directory.path(), None).await;

        let loaded = registry
            .invoke(
                "skill",
                ToolInvocation {
                    call_id: "skill-1".to_owned(),
                    arguments: json!({"name": "skill-creator"}),
                },
            )
            .await
            .expect("the builtin loads with no skill root walked");
        assert!(loaded.model_text.contains("# Skill Creator"), "{loaded:?}");
        assert!(
            !loaded.model_text.contains("Base directory for this skill"),
            "{loaded:?}"
        );
        assert_eq!(loaded.typed_result["skill_dir"], serde_json::Value::Null);
    }

    /// US-169: a disk skill carrying a builtin name is skipped, so the body the
    /// tool renders is the seeded one rather than the impostor's.
    #[tokio::test]
    async fn a_builtin_name_cannot_be_taken_by_a_disk_skill() {
        let directory = tempdir().expect("tempdir");
        let impostor = directory.path().join(".vibe/skills/vibe");
        std::fs::create_dir_all(&impostor).expect("skill directory");
        std::fs::write(
            impostor.join("SKILL.md"),
            "---\nname: vibe\ndescription: impostor\n---\nImpostor body.\n",
        )
        .expect("skill file");
        let registry = registered(directory.path(), None).await;

        let loaded = registry
            .invoke(
                "skill",
                ToolInvocation {
                    call_id: "skill-1".to_owned(),
                    arguments: json!({"name": "vibe"}),
                },
            )
            .await
            .expect("the builtin answers under its reserved name");
        assert!(!loaded.model_text.contains("Impostor body."), "{loaded:?}");
        assert!(
            loaded.model_text.contains("source of truth"),
            "the seeded body is the one rendered: {loaded:?}"
        );
    }

    /// US-172: the invoked-skill resolver installed with the `skill` tool
    /// answers the reference's `parse_skill_command` vocabulary: the first
    /// word past the `/` names the skill case-insensitively, trailing text
    /// stays the operator's message, and a name that is unknown or not user
    /// invocable resolves to nothing.
    #[tokio::test]
    async fn a_slash_invocation_resolves_against_the_published_catalog() {
        let directory = tempdir().expect("tempdir");
        let skill_directory = directory.path().join(".vibe/skills/probe");
        std::fs::create_dir_all(&skill_directory).expect("skill directory");
        std::fs::write(
            skill_directory.join("SKILL.md"),
            "---\nname: probe\ndescription: a probe\n---\nDo the probing.\n",
        )
        .expect("skill file");
        let registry = registered(directory.path(), None).await;
        let resolver = registry
            .invoked_skills()
            .expect("registering the skill tool installs the resolver");

        let invoked = resolver
            .resolve("/probe extra instructions here")
            .expect("the first word resolves the skill");
        assert_eq!(invoked.name, "probe");
        assert!(
            invoked.loaded.model_text.contains("Do the probing."),
            "{invoked:?}"
        );
        assert!(
            invoked
                .loaded
                .model_text
                .contains(&crate::skills::skill_content_marker("probe")),
            "the rendering opens with the dedup marker: {invoked:?}"
        );
        assert!(
            invoked.already_loaded.model_text.contains("already loaded"),
            "{invoked:?}"
        );

        assert!(
            resolver.resolve("/PROBE").is_some(),
            "the lookup is case-insensitive"
        );
        assert!(
            resolver.resolve("/unknown").is_none(),
            "a slash word naming no skill is an ordinary prompt"
        );
        assert!(
            resolver.resolve("/vibe").is_none(),
            "a skill that is not user invocable cannot be slash-invoked"
        );
        assert!(
            resolver.resolve("probe").is_none(),
            "a prompt without the slash is never an invocation"
        );
    }

    /// US-172: a slash invocation records the load in the same ledger the
    /// `skill` tool reads, so the model calling `skill` afterward is
    /// acknowledged instead of paying for the body twice.
    #[tokio::test]
    async fn a_slash_invocation_marks_the_skill_loaded_for_the_tool() {
        let directory = tempdir().expect("tempdir");
        let skill_directory = directory.path().join(".vibe/skills/probe");
        std::fs::create_dir_all(&skill_directory).expect("skill directory");
        std::fs::write(
            skill_directory.join("SKILL.md"),
            "---\nname: probe\ndescription: a probe\n---\nDo the probing.\n",
        )
        .expect("skill file");
        let registry = registered(directory.path(), None).await;
        registry
            .invoked_skills()
            .expect("resolver installed")
            .resolve("/probe")
            .expect("the skill resolves");

        let loaded = registry
            .invoke(
                "skill",
                ToolInvocation {
                    call_id: "skill-1".to_owned(),
                    arguments: json!({"name": "probe"}),
                },
            )
            .await
            .expect("the tool still answers");
        assert!(
            loaded.model_text.contains("already loaded"),
            "the slash invocation counted as the first load: {loaded:?}"
        );
    }

    /// US-115: the file list stops at ten entries, so a skill shipping a
    /// hundred files cannot flood the conversation with their names.
    #[test]
    fn a_skill_file_list_stops_at_the_published_cap() {
        let directory = tempdir().expect("tempdir");
        for index in 0..25 {
            std::fs::write(directory.path().join(format!("file-{index:02}.md")), "x\n")
                .expect("seed");
        }
        std::fs::write(directory.path().join("SKILL.md"), "skill\n").expect("own file");

        let files = skill_files(directory.path());

        assert_eq!(files.len(), MAX_LISTED_SKILL_FILES);
        assert_eq!(files[0], "file-00.md");
        assert!(!files.iter().any(|file| file == "SKILL.md"));
    }

    /// US-115: a skill with no directory on disk renders without the two lines
    /// that would otherwise name an empty path.
    #[test]
    fn a_skill_with_no_directory_omits_the_base_directory_lines() {
        let skill = SkillDefinition {
            name: "inline".to_owned(),
            description: "declared, not filed".to_owned(),
            license: None,
            compatibility: None,
            metadata: BTreeMap::new(),
            allowed_tools: Vec::new(),
            user_invocable: false,
            body: "Do the thing.".to_owned(),
            source: crate::skills::SkillSource::Builtin,
            scope: crate::skills::SkillScope::Builtin,
            path: None,
        };

        let rendered = render_skill(&skill, None);

        assert!(!rendered.contains("Base directory"), "{rendered}");
        assert!(rendered.contains("Do the thing."), "{rendered}");
        assert!(
            rendered.contains("<skill_files>\n\n</skill_files>"),
            "{rendered}"
        );
    }

    /// US-103: `todo` reads its maximum from the configuration, and the failure
    /// names the configured limit rather than a compiled-in one.
    #[tokio::test]
    async fn a_configured_todo_maximum_replaces_the_declared_one() {
        let directory = tempdir().expect("tempdir");
        let policy = PermissionStore::default();
        policy
            .set_trust(
                directory.path(),
                TrustDecision::Trusted,
                TrustRootKind::Workspace,
            )
            .await
            .expect("trust");
        let config = policy.tool_config();
        config.update(
            "[todo]\nmax_todos = 2\n"
                .parse::<toml::Table>()
                .expect("settings parse"),
        );
        let registry = ToolRegistry::default();
        BuiltinTools::new(directory.path(), None)
            .register(
                "session-1",
                trusted_skills(directory.path()),
                &registry,
                &ToolGuard {
                    policy,
                    approval: Arc::new(AllowApproval),
                    config: config.clone(),
                    scratchpad: None,
                },
            )
            .expect("register");

        let todos = |count: usize| {
            json!({
                "action": "write",
                "todos": (0..count)
                    .map(|index| json!({"id": index.to_string(), "content": "work"}))
                    .collect::<Vec<_>>(),
            })
        };
        registry
            .invoke(
                "todo",
                ToolInvocation {
                    call_id: "todo-1".to_owned(),
                    arguments: todos(2),
                },
            )
            .await
            .expect("a list at the configured maximum is accepted");
        let refused = registry
            .invoke(
                "todo",
                ToolInvocation {
                    call_id: "todo-2".to_owned(),
                    arguments: todos(3),
                },
            )
            .await
            .expect_err("a list past the configured maximum is refused");
        assert!(refused.to_string().contains("2-item limit"), "{refused}");
    }

    /// US-103: `web_search` sends the configured model, and waits the configured
    /// timeout for the answer.
    #[tokio::test]
    async fn web_search_sends_the_configured_model() {
        let directory = tempdir().expect("tempdir");
        let answer = "{\"outputs\": [{\"type\": \"message.output\", \"content\": \"answered\"}]}";
        let (endpoint, requests) = serve_once_recording(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: \
             close\r\n\r\n{answer}",
            answer.len()
        ));
        let policy = PermissionStore::default();
        let config = policy.tool_config();
        config.update(
            "[web_search]\nmodel = \"configured-model\"\ntimeout = 9\n"
                .parse::<toml::Table>()
                .expect("settings parse"),
        );
        let registry = ToolRegistry::default();
        BuiltinTools::new(
            directory.path(),
            Some(WebSearchAccess {
                endpoint,
                api_key: SecretString::from("probe"),
            }),
        )
        .register(
            "session-1",
            trusted_skills(directory.path()),
            &registry,
            &ToolGuard {
                policy,
                approval: Arc::new(AllowApproval),
                config: config.clone(),
                scratchpad: None,
            },
        )
        .expect("register");

        registry
            .invoke(
                "web_search",
                ToolInvocation {
                    call_id: "search-1".to_owned(),
                    arguments: json!({"query": "who ships parity"}),
                },
            )
            .await
            .expect("the fixture answers");
        let request = requests.recv().expect("the fixture recorded the request");
        assert!(request.contains("configured-model"), "{request}");
        assert!(
            !request.contains("mistral-vibe-cli-with-tools"),
            "the declared default must not travel once an operator moved it: {request}"
        );
        let settings: WebSearchConfig = config.view("web_search");
        assert_eq!(settings.timeout, 9);
        // US-115: the request metadata and the user agent the reference sends,
        // and no credential in either.
        assert!(request.contains("secondary_call"), "{request}");
        assert!(request.contains("mistral-client-python/"), "{request}");
        let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        assert!(
            !body.contains("probe") || !body.contains("Bearer"),
            "the credential must travel only in the authorization header: {request}"
        );
    }

    /// US-115: a response carrying no text is a failure, not an empty answer,
    /// and the failure names nothing the operator holds in confidence.
    #[tokio::test]
    async fn a_web_search_answer_with_no_text_fails_rather_than_returning_nothing() {
        let directory = tempdir().expect("tempdir");
        let answer = "{\"outputs\": []}";
        let endpoint = serve_once(vec![format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: \
             close\r\n\r\n{answer}",
            answer.len()
        )]);
        let registry = registered_with(
            directory.path(),
            Some(WebSearchAccess {
                endpoint,
                api_key: SecretString::from("super-secret-key"),
            }),
            Arc::new(AllowApproval),
        )
        .await;

        let error = registry
            .invoke(
                "web_search",
                ToolInvocation {
                    call_id: "search-1".to_owned(),
                    arguments: json!({"query": "who ships parity"}),
                },
            )
            .await
            .expect_err("an answerless response is a failure");

        assert!(error.to_string().contains("no text"), "{error}");
        assert!(!error.to_string().contains("super-secret-key"), "{error}");
    }

    /// US-250: a page past the limit is cut at `max_content_bytes` itself, not
    /// at a bound that also depends on what the turn's output budget has left,
    /// and the marker that says so is this port's own wording.
    #[tokio::test]
    async fn a_long_page_is_cut_at_the_declared_bound() {
        let directory = tempdir().expect("tempdir");
        let registry = registered_with(directory.path(), None, Arc::new(AllowApproval)).await;
        let settings: WebFetchConfig = ToolConfigResolver::new().view("web_fetch");
        let body = "z".repeat(settings.max_content_bytes + 4_096);
        let endpoint = serve_once(vec![http_response(&body)]);

        let fetched = registry
            .invoke(
                "web_fetch",
                ToolInvocation {
                    call_id: "fetch-1".to_owned(),
                    arguments: json!({"url": endpoint}),
                },
            )
            .await
            .expect("fetch");
        assert_eq!(fetched.typed_result["was_truncated"], json!(true));
        let content = fetched.typed_result["content"]
            .as_str()
            .expect("content is text");
        let marker = format!(
            "\n\n[content truncated at {} bytes]",
            settings.max_content_bytes
        );
        assert_eq!(
            content.len(),
            settings.max_content_bytes + marker.len(),
            "the body is cut at the declared bound and nowhere else"
        );
        assert!(content.ends_with(&marker), "the marker states the bound");
        assert!(
            fetched.model_text.ends_with("\nwas_truncated: True"),
            "the flag reaches the model on its own line"
        );
    }

    /// US-250: the bound is a ceiling, not a trigger. A body that lands exactly
    /// on it is whole, so nothing is appended and nothing is flagged.
    #[tokio::test]
    async fn a_page_exactly_at_the_bound_is_not_truncated() {
        let directory = tempdir().expect("tempdir");
        let registry = registered_with(directory.path(), None, Arc::new(AllowApproval)).await;
        let settings: WebFetchConfig = ToolConfigResolver::new().view("web_fetch");
        let body = "z".repeat(settings.max_content_bytes);
        let endpoint = serve_once(vec![http_response(&body)]);

        let fetched = registry
            .invoke(
                "web_fetch",
                ToolInvocation {
                    call_id: "fetch-1".to_owned(),
                    arguments: json!({"url": endpoint}),
                },
            )
            .await
            .expect("fetch");
        assert_eq!(fetched.typed_result["was_truncated"], json!(false));
        assert_eq!(fetched.typed_result["content"], json!(body));
    }

    /// US-251: the request carries the two headers the reference sets beside
    /// its user agent, so a host that varies its answer on them answers this
    /// port the way it answers the reference.
    #[tokio::test]
    async fn the_request_carries_the_reference_headers() {
        let directory = tempdir().expect("tempdir");
        let registry = registered_with(directory.path(), None, Arc::new(AllowApproval)).await;
        let settings: WebFetchConfig = ToolConfigResolver::new().view("web_fetch");
        let (endpoint, requests) = serve_once_recording(http_response("page"));

        registry
            .invoke(
                "web_fetch",
                ToolInvocation {
                    call_id: "fetch-1".to_owned(),
                    arguments: json!({"url": endpoint}),
                },
            )
            .await
            .expect("fetch");

        let request = requests.recv().expect("the fixture recorded the request");
        assert!(
            request.contains(&format!("accept: {FETCH_ACCEPT}\r\n")),
            "{request}"
        );
        assert!(
            request.contains(&format!("accept-language: {FETCH_ACCEPT_LANGUAGE}\r\n")),
            "{request}"
        );
        assert!(
            request.contains(&format!("user-agent: {}\r\n", settings.user_agent)),
            "{request}"
        );
    }

    /// US-251: a bot challenge is answered by one retry under an agent that
    /// names itself, and the retry's page is what the model reads.
    #[tokio::test]
    async fn a_challenge_is_retried_once_under_an_agent_that_names_itself() {
        let directory = tempdir().expect("tempdir");
        let registry = registered_with(directory.path(), None, Arc::new(AllowApproval)).await;
        let (endpoint, requests) =
            serve_recording(vec![challenge_response(), http_response("the real page")]);

        let fetched = registry
            .invoke(
                "web_fetch",
                ToolInvocation {
                    call_id: "fetch-1".to_owned(),
                    arguments: json!({"url": endpoint}),
                },
            )
            .await
            .expect("the retry answers");
        assert_eq!(fetched.typed_result["content"], json!("the real page"));

        let first = requests.recv().expect("the first try was recorded");
        let retry = requests.recv().expect("the retry was recorded");
        assert!(
            !first.contains(&format!("user-agent: {HONEST_USER_AGENT}\r\n")),
            "the first try wears the browser agent: {first}"
        );
        assert!(
            retry.contains(&format!("user-agent: {HONEST_USER_AGENT}\r\n")),
            "the retry names itself: {retry}"
        );
        assert!(
            requests.recv_timeout(SETTLE).is_err(),
            "the retry is bounded to one attempt"
        );
    }

    /// US-251: the retry is bounded. A host that challenges every agent is
    /// reported rather than asked a third time.
    #[tokio::test]
    async fn a_challenge_that_persists_is_reported_after_one_retry() {
        let directory = tempdir().expect("tempdir");
        let registry = registered_with(directory.path(), None, Arc::new(AllowApproval)).await;
        let (endpoint, requests) =
            serve_recording(vec![challenge_response(), challenge_response()]);

        let refused = registry
            .invoke(
                "web_fetch",
                ToolInvocation {
                    call_id: "fetch-1".to_owned(),
                    arguments: json!({"url": endpoint}),
                },
            )
            .await
            .expect_err("a challenge the retry cannot pass is a failure");
        assert!(refused.to_string().contains("403"), "{refused}");
        assert!(requests.recv().is_ok(), "the first try was recorded");
        assert!(requests.recv().is_ok(), "the retry was recorded");
        assert!(
            requests.recv_timeout(SETTLE).is_err(),
            "no third attempt is made"
        );
    }

    /// US-251: only the challenge marker earns a retry. An ordinary refusal is
    /// reported on the first answer.
    #[tokio::test]
    async fn a_forbidden_without_the_marker_is_not_retried() {
        let directory = tempdir().expect("tempdir");
        let registry = registered_with(directory.path(), None, Arc::new(AllowApproval)).await;
        let forbidden =
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned();
        let (endpoint, requests) = serve_recording(vec![forbidden, http_response("never read")]);

        let refused = registry
            .invoke(
                "web_fetch",
                ToolInvocation {
                    call_id: "fetch-1".to_owned(),
                    arguments: json!({"url": endpoint}),
                },
            )
            .await
            .expect_err("an ordinary refusal is a failure");
        assert!(refused.to_string().contains("403"), "{refused}");
        assert!(requests.recv().is_ok(), "the first try was recorded");
        assert!(requests.recv_timeout(SETTLE).is_err(), "nothing is retried");
    }

    /// US-251: the HTML reader is reached by the test the reference applies. A
    /// charset parameter still names HTML, and a type that merely contains the
    /// word does not.
    #[tokio::test]
    async fn html_is_recognized_by_its_media_type_and_not_by_a_substring() {
        let directory = tempdir().expect("tempdir");
        let registry = registered_with(directory.path(), None, Arc::new(AllowApproval)).await;
        let page = "<html><body><p>Read me</p></body></html>";
        let typed = |content_type: &str| {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: \
                 close\r\n\r\n{page}",
                page.len()
            )
        };

        let charset = serve_once(vec![typed("text/html; charset=utf-8")]);
        let fetched = registry
            .invoke(
                "web_fetch",
                ToolInvocation {
                    call_id: "fetch-1".to_owned(),
                    arguments: json!({"url": charset}),
                },
            )
            .await
            .expect("fetch");
        assert_eq!(fetched.typed_result["content"], json!("Read me"));

        let adjacent = serve_once(vec![typed("application/vnd.html-ish+json")]);
        let raw = registry
            .invoke(
                "web_fetch",
                ToolInvocation {
                    call_id: "fetch-2".to_owned(),
                    arguments: json!({"url": adjacent}),
                },
            )
            .await
            .expect("fetch");
        assert_eq!(raw.typed_result["content"], json!(page));
    }

    /// A redirect loop stops at the hop budget, and the failure names the host
    /// rather than the URL, which can carry a query string.
    #[tokio::test]
    async fn a_redirect_chain_is_bounded_and_the_failure_names_only_the_host() {
        let directory = tempdir().expect("tempdir");
        let registry = registered_with(directory.path(), None, Arc::new(AllowApproval)).await;
        let hops = vec![String::new(); MAX_FETCH_REDIRECTS + 2];
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        let address = listener.local_addr().expect("listener address").to_string();
        let self_redirect = format!("http://{address}/next");
        std::thread::spawn(move || {
            for _ in hops {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buffer = [0_u8; 4096];
                let _ = stream.read(&mut buffer);
                let _ = stream.write_all(redirect_response(&self_redirect).as_bytes());
                let _ = stream.flush();
            }
        });

        let refused = registry
            .invoke(
                "web_fetch",
                ToolInvocation {
                    call_id: "fetch-1".to_owned(),
                    arguments: json!({"url": format!("http://{address}/start?token=secret")}),
                },
            )
            .await
            .expect_err("an unbounded redirect chain is refused");
        assert!(refused.to_string().contains("127.0.0.1"), "{refused}");
        assert!(!refused.to_string().contains("token=secret"), "{refused}");
    }

    /// A host that accepts and never answers fails at the requested timeout,
    /// and the failure names the host rather than the URL.
    #[tokio::test]
    async fn a_silent_host_times_out_without_echoing_the_url() {
        let directory = tempdir().expect("tempdir");
        let registry = registered_with(directory.path(), None, Arc::new(AllowApproval)).await;
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        let address = listener.local_addr().expect("listener address").to_string();
        // The listener is held open and never answers, so the request can only
        // end at the timeout.
        std::thread::spawn(move || {
            let accepted = listener.accept();
            std::thread::sleep(Duration::from_secs(5));
            drop(accepted);
        });

        let timed_out = registry
            .invoke(
                "web_fetch",
                ToolInvocation {
                    call_id: "fetch-1".to_owned(),
                    arguments: json!({
                        "url": format!("http://{address}/slow?token=secret"),
                        "timeout": 1,
                    }),
                },
            )
            .await
            .expect_err("a silent host times out");
        assert!(timed_out.to_string().contains("timed out"), "{timed_out}");
        assert!(timed_out.to_string().contains("127.0.0.1"), "{timed_out}");
        assert!(
            !timed_out.to_string().contains("token=secret"),
            "{timed_out}"
        );
    }

    /// The search answer and its citations come back from the conversations
    /// endpoint through the registered tool.
    #[tokio::test]
    async fn the_registered_web_search_tool_reports_its_answer_and_sources() {
        let directory = tempdir().expect("tempdir");
        let payload = json!({
            "outputs": [{"content": [
                {"type": "text", "text": "42"},
                {"type": "tool_reference", "url": "https://a.example", "title": "A"}
            ]}]
        })
        .to_string();
        let endpoint = serve_once(vec![format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{payload}",
            payload.len()
        )]);
        let registry = registered_with(
            directory.path(),
            Some(probe_access(endpoint)),
            Arc::new(AllowApproval),
        )
        .await;

        let answered = registry
            .invoke(
                "web_search",
                ToolInvocation {
                    call_id: "search-1".to_owned(),
                    arguments: json!({"query": "the answer"}),
                },
            )
            .await
            .expect("search");
        // `WebSearchResult` declares `query`, `answer` and `sources`, and the
        // agent loop renders one field per line from it, with the source list
        // as Python's repr of a list of dictionaries.
        assert_eq!(
            answered.model_text,
            "query: the answer\nanswer: 42\nsources: [{'title': 'A', 'url': 'https://a.example'}]"
        );
        assert_eq!(
            answered.typed_result["sources"],
            json!([{"title": "A", "url": "https://a.example"}])
        );
    }

    #[test]
    fn an_unwritten_todo_list_reads_back_empty() {
        let todos = Mutex::new(BTreeMap::new());
        let output = run_todo(
            &todos,
            "session-1",
            &json!({"action": "read"}),
            &todo_settings(),
        )
        .expect("reading an unwritten list is not an error");
        assert_eq!(output.typed_result["todos"], json!([]));
        assert_eq!(output.typed_result["total_count"], 0);
        assert_eq!(
            output.model_text,
            "verb: Retrieved\ntodos: []\ntotal_count: 0\nmessage: Retrieved 0 todos"
        );
    }

    #[test]
    fn a_written_todo_list_reads_back_with_the_schema_defaults_applied() {
        let todos = Mutex::new(BTreeMap::new());
        run_todo(
            &todos,
            "session-1",
            &json!({"action": "write", "todos": [{"id": "a", "content": "ship"}]}),
            &todo_settings(),
        )
        .expect("write");
        let output = run_todo(
            &todos,
            "session-1",
            &json!({"action": "read"}),
            &todo_settings(),
        )
        .expect("read");
        assert_eq!(
            output.typed_result["todos"],
            json!([{"id": "a", "content": "ship", "status": "pending", "priority": "medium"}])
        );
    }

    #[test]
    fn a_todo_list_is_kept_per_session() {
        let todos = Mutex::new(BTreeMap::new());
        run_todo(
            &todos,
            "session-1",
            &json!({"action": "write", "todos": [{"id": "a", "content": "ship"}]}),
            &todo_settings(),
        )
        .expect("write");
        let other = run_todo(
            &todos,
            "session-2",
            &json!({"action": "read"}),
            &todo_settings(),
        )
        .expect("read");
        assert_eq!(other.typed_result["todos"], json!([]));
    }

    #[test]
    fn a_duplicated_todo_id_is_refused_naming_the_id() {
        let todos = Mutex::new(BTreeMap::new());
        let error = run_todo(
            &todos,
            "session-1",
            &json!({"action": "write", "todos": [
                {"id": "a", "content": "one"},
                {"id": "a", "content": "two"}
            ]}),
            &todo_settings(),
        )
        .expect_err("a duplicated id is refused");
        assert!(error.to_string().contains('a'), "{error}");
        assert!(error.to_string().contains("more than once"), "{error}");
    }

    #[test]
    fn an_unknown_todo_action_names_the_two_that_exist() {
        let todos = Mutex::new(BTreeMap::new());
        let error = run_todo(
            &todos,
            "session-1",
            &json!({"action": "append"}),
            &todo_settings(),
        )
        .expect_err("an unknown action is refused");
        assert!(error.to_string().contains("append"), "{error}");
        assert!(error.to_string().contains("read"), "{error}");
    }

    #[test]
    fn a_non_http_scheme_is_refused_before_any_network_access() {
        let error = fetch_url(&json!({"url": "file:///etc/passwd"}))
            .expect_err("a file URL is not fetchable");
        assert!(error.to_string().contains("file"), "{error}");
        assert!(
            fetch_url(&json!({"url": "   "})).is_err(),
            "an empty URL is not fetchable"
        );
    }

    #[test]
    fn a_bare_host_is_normalized_to_https_like_the_reference() {
        let url = fetch_url(&json!({"url": "example.com/page"})).expect("a bare host normalizes");
        assert_eq!(url.as_str(), "https://example.com/page");
        let relative =
            fetch_url(&json!({"url": "//example.com/page"})).expect("protocol-relative normalizes");
        assert_eq!(relative.as_str(), "https://example.com/page");
    }

    /// A timeout outside the range is refused rather than reduced, so a call
    /// that cannot run as asked is reported instead of quietly running as
    /// something else, and the refusal names the ceiling it broke.
    #[test]
    fn a_timeout_out_of_range_is_refused_and_the_refusal_names_the_cap() {
        let settings: WebFetchConfig = ToolConfigResolver::new().view("web_fetch");
        assert_eq!(
            fetch_timeout(&json!({"timeout": Value::Null}), &settings).expect("default"),
            Duration::from_secs(settings.default_timeout)
        );
        assert_eq!(
            fetch_timeout(&json!({"timeout": 45}), &settings).expect("in range"),
            Duration::from_secs(45)
        );
        let over = fetch_timeout(&json!({"timeout": 9_000}), &settings).expect_err("over the cap");
        assert!(
            over.to_string().contains(&settings.max_timeout.to_string()),
            "the refusal names the cap: {over}"
        );
        assert!(fetch_timeout(&json!({"timeout": 0}), &settings).is_err());
        assert!(fetch_timeout(&json!({"timeout": -1}), &settings).is_err());

        // The ceiling and the default are the operator's to move.
        let resolver = ToolConfigResolver::new();
        resolver.update(
            "[web_fetch]\ndefault_timeout = 5\nmax_timeout = 7\n"
                .parse::<toml::Table>()
                .expect("settings parse"),
        );
        let configured: WebFetchConfig = resolver.view("web_fetch");
        assert_eq!(
            fetch_timeout(&json!({"timeout": Value::Null}), &configured).expect("default"),
            Duration::from_secs(5)
        );
        let lowered = fetch_timeout(&json!({"timeout": 9_000}), &configured)
            .expect_err("over the lowered cap");
        assert!(
            lowered.to_string().contains('7'),
            "the refusal names the configured cap: {lowered}"
        );
    }

    /// A page whose text folds to a different byte length than it occupies,
    /// `İ` being the everyday case, must not slide the offsets used to drop the
    /// script span. A Turkish title ahead of the head scripts is an ordinary
    /// page, not a crafted one.
    #[tokio::test]
    async fn a_page_whose_case_fold_changes_its_length_is_stripped_without_slicing_it_apart() {
        let directory = tempdir().expect("tempdir");
        let registry = registered_with(directory.path(), None, Arc::new(AllowApproval)).await;
        let page = format!(
            "<html><head><title>{}</title><script>é = 1</script></head><body><p>Body</p>\
             </body></html>",
            "İ".repeat(9)
        );
        let endpoint = serve_once(vec![format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{page}",
            page.len()
        )]);

        let fetched = registry
            .invoke(
                "web_fetch",
                ToolInvocation {
                    call_id: "fetch-1".to_owned(),
                    arguments: json!({"url": endpoint}),
                },
            )
            .await
            .expect("a page carrying U+0130 is fetched rather than panicking");
        assert!(fetched.model_text.contains(&"İ".repeat(9)), "{fetched:?}");
        assert!(fetched.model_text.contains("Body"), "{fetched:?}");
        assert!(!fetched.model_text.contains("é = 1"), "{fetched:?}");
    }

    #[test]
    fn html_reaches_the_model_as_prose_without_scripts_or_tags() {
        let text = html_to_text(
            "<html><head><style>a{color:red}</style></head><body><script>alert('x')</script>\
             <h1>Title</h1><p>Body &amp; more</p></body></html>",
        );
        assert_eq!(text, "Title\nBody & more");
    }

    #[test]
    fn a_search_response_yields_its_answer_and_deduplicated_sources() {
        let (answer, sources) = parse_search_response(&json!({
            "outputs": [
                {"content": [
                    {"type": "text", "text": "the answer"},
                    {"type": "tool_reference", "url": "https://a.example", "title": "A"},
                    {"type": "tool_reference", "url": "https://a.example", "title": "A"}
                ]}
            ]
        }));
        assert_eq!(answer, "the answer");
        assert_eq!(
            sources,
            vec![json!({"title": "A", "url": "https://a.example"})]
        );
    }

    #[test]
    fn a_string_content_search_response_is_read_too() {
        let (answer, sources) =
            parse_search_response(&json!({"outputs": [{"content": " short "}]}));
        assert_eq!(answer, "short");
        assert!(sources.is_empty());
    }
}
