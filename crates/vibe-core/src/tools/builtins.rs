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
use crate::extensions::{DiscoveryRoots, SkillDefinition};
use crate::policy::{
    PermissionContext, PermissionMode, PermissionRequirement, PolicyGuardedTool, ToolGuard,
};
use crate::skills::SkillDiscovery;
use crate::tools::config::{SharedToolConfig, TodoConfig, ToolConfigResolver, declared_document};
use crate::tools::document::Document;
use crate::tools::{
    OwnedToolHandlerFuture, RegistrationOutcome, ToolError, ToolHandler, ToolInvocation,
    ToolOutputSink, ToolRegistry,
};

mod skill;
mod todo;
mod web_fetch;
mod web_search;

pub use skill::skill_payload;
use skill::{SkillInvocationResolver, run_skill, skill_spec};
use todo::{run_todo, todo_spec};
use web_fetch::{fetch_url, web_fetch_handler, web_fetch_spec};
use web_search::{web_search_handler, web_search_spec};

/// How many hops `web_fetch` follows before it refuses the chain.
///
/// Reference `web_fetch.py:_MAX_REDIRECTS`: the twenty-first redirect is
/// refused, so a redirect loop terminates rather than running until the
/// timeout.
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

    /// The entry as reference `TodoItem` declares its fields.
    fn document(&self) -> Document {
        Document::new()
            .field("id", self.id.clone())
            .field("content", self.content.clone())
            .field("status", self.status.clone())
            .field("priority", self.priority.clone())
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
    /// The skills that exist without a file on disk, which a discovered skill
    /// of the same name never shadows.
    builtin_skills: Arc<BTreeMap<String, SkillDefinition>>,
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
            builtin_skills: Arc::new(crate::skills::builtins::builtin_skills()),
        }
    }

    /// The same tools with one more skill that has no directory on disk, the
    /// way the reference's skill manager holds a skill registered with a
    /// prompt alone.
    #[must_use]
    pub fn with_builtin_skill(mut self, skill: SkillDefinition) -> Self {
        Arc::make_mut(&mut self.builtin_skills).insert(skill.name.clone(), skill);
        self
    }

    /// The same tools with `skills` in place of every skill that has no
    /// directory on disk, which is how a unified session's plugin skills
    /// replace the legacy builtins.
    #[must_use]
    pub fn with_seeded_skills(mut self, skills: Arc<BTreeMap<String, SkillDefinition>>) -> Self {
        self.builtin_skills = skills;
        self
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
                    // Reference `WebFetch.resolve_permission`: a configured
                    // `always` or `never` settles the call, and everything else
                    // asks for the origin the fetch reaches, scheme and port
                    // included, so approving `https://host` does not cover
                    // `http://host` or another port on it.
                    {
                        let settings = config.clone();
                        Arc::new(move |invocation: &ToolInvocation| {
                            let configured = settings.view::<SharedToolConfig>("web_fetch");
                            if configured.permission != PermissionMode::Ask {
                                return Ok(PermissionContext::settled(configured.permission));
                            }
                            let url = fetch_url(&invocation.arguments)?;
                            let Some(origin) = web_fetch::url_origin(&url) else {
                                return Ok(PermissionContext::deferred());
                            };
                            Ok(PermissionContext::asking(vec![
                                PermissionRequirement::url_pattern(&origin),
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
            builtins: self.builtin_skills.clone(),
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
        let builtins = self.builtin_skills.clone();
        let session_id = session_id.to_owned();
        Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let roots = roots.clone();
                let builtins = builtins.clone();
                let loaded = loaded.clone();
                let session_id = session_id.clone();
                let name = invocation.arguments["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                Box::pin(async move { run_skill(&roots, &builtins, &loaded, &session_id, &name) })
            },
        )
    }
}

#[cfg(test)]
mod builtins_tests;
