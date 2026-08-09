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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::config::DotenvValues;
use crate::extensions::{DiscoveryRoots, SkillDefinition, discover_extensions};
use crate::policy::{
    PermissionContext, PermissionMode, PermissionRequirement, PolicyGuardedTool, ToolGuard,
};
use crate::schema::{ObjectSchema, Property};
use crate::skills::SkillDiscovery;
use crate::tools::config::{
    SharedToolConfig, TodoConfig, ToolConfigResolver, WebFetchConfig, WebSearchConfig,
    declared_document,
};
use crate::tools::reference_text;
use crate::tools::{
    OwnedToolHandlerFuture, RegistrationOutcome, ToolAvailability, ToolError, ToolExecutionOutput,
    ToolHandler, ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource,
    ToolSpec,
};

/// The redirect budget the security NFR sets for `web_fetch`.
const MAX_FETCH_REDIRECTS: usize = 5;
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

// --------------------------------------------------------------------------
// todo
// --------------------------------------------------------------------------

/// Directive coverage for `todo`, whose reference description this port must
/// cover without reproducing (`NOTICE`).
///
/// | Reference directive | Covered by |
/// |---|---|
/// | Every call names an action, `read` or `write` | the `action` description |
/// | `write` replaces the whole list rather than patching it | "replaces the whole list" |
/// | Ids are stable across updates so an item can be re-stated | the `id` description |
/// | Exactly one item is `in_progress` at a time | "Keep one item in_progress" |
/// | An item is completed as soon as it is done, not in a batch at the end | "mark it completed as soon as it is done" |
fn todo_spec() -> ToolSpec {
    let item = ObjectSchema::new()
        .define(
            "TodoStatus",
            Property::string().constrained("enum", json!(TodoItem::STATUSES)),
        )
        .define(
            "TodoPriority",
            Property::string().constrained("enum", json!(TodoItem::PRIORITIES)),
        )
        .required(
            "id",
            Property::string().described("A stable identifier for the task, reused across updates"),
        )
        .required(
            "content",
            Property::string().described("A short description of the task"),
        )
        .optional(
            "status",
            Property::reference("TodoStatus")
                .described("Where the task stands: pending, in_progress, completed, cancelled")
                .with_default("pending"),
        )
        .optional(
            "priority",
            Property::reference("TodoPriority")
                .described("How much the task matters: high, medium, low")
                .with_default("medium"),
        );
    ToolSpec {
        name: "todo".to_owned(),
        description: "Track a multi-step task. `read` returns the current list, `write` replaces \
                      the whole list with the one you send. Keep one item in_progress at a time \
                      and mark it completed as soon as it is done rather than settling the list \
                      at the end."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .define("TodoItem", item)
            .required(
                "action",
                Property::string().described(
                    "Required on every call: `read` to view the current list, `write` to replace \
                     it",
                ),
            )
            .optional(
                "todos",
                Property::array(Property::reference("TodoItem"))
                    .described(
                        "Required when action is `write`: the whole list, which replaces the \
                         previous one",
                    )
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build(),
        output_schema: None,
        config: declared_document("todo"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

fn run_todo(
    todos: &Mutex<BTreeMap<String, Vec<TodoItem>>>,
    session_id: &str,
    arguments: &Value,
    settings: &TodoConfig,
) -> Result<ToolExecutionOutput, ToolError> {
    let action = arguments["action"].as_str().unwrap_or_default();
    let mut stored = todos
        .lock()
        .map_err(|_| ToolError::Execution("the todo list lock is poisoned".to_owned()))?;
    let (verb, items) = match action {
        // A list that was never written reads back empty, not as an error.
        "read" => (
            "Retrieved",
            stored.get(session_id).cloned().unwrap_or_default(),
        ),
        "write" => {
            let items = parse_todo_items(&arguments["todos"])?;
            if items.len() > settings.max_todos {
                return Err(ToolError::Execution(format!(
                    "the todo list holds {} items, exceeding the {}-item limit",
                    items.len(),
                    settings.max_todos
                )));
            }
            let mut seen = BTreeSet::new();
            for item in &items {
                if !seen.insert(item.id.as_str()) {
                    return Err(ToolError::Execution(format!(
                        "todo id `{}` appears more than once",
                        item.id
                    )));
                }
            }
            stored.insert(session_id.to_owned(), items.clone());
            ("Updated", items)
        }
        other => {
            return Err(ToolError::Execution(format!(
                "unknown todo action `{other}`; use `read` or `write`"
            )));
        }
    };
    drop(stored);
    let total = items.len();
    // Reference `TodoResult.message` is a computed field, so it is rendered
    // from the verb and the count rather than stored.
    let message = format!("{verb} {total} todos");
    let rendered = items
        .iter()
        .map(TodoItem::rendered_fields)
        .collect::<Vec<_>>();
    Ok(ToolExecutionOutput {
        model_text: reference_text::joined(&[
            ("verb", verb.to_owned()),
            ("todos", reference_text::dictionary_list(&rendered)),
            ("total_count", total.to_string()),
            ("message", message.clone()),
        ]),
        display: json!({"kind": "todo", "count": total}),
        // The transcript renders the todo widget from the typed result, so the
        // items travel there rather than in the display metadata.
        typed_result: json!({
            "verb": verb,
            "todos": items,
            "total_count": total,
            "message": message,
        }),
        chunks: Vec::new(),
    })
}

fn parse_todo_items(value: &Value) -> Result<Vec<TodoItem>, ToolError> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let entries = value.as_array().ok_or_else(|| ToolError::SchemaViolation {
        path: "/todos".to_owned(),
        message: "must be an array of todo items".to_owned(),
    })?;
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            serde_json::from_value::<TodoItem>(entry.clone()).map_err(|error| {
                ToolError::SchemaViolation {
                    path: format!("/todos/{index}"),
                    message: error.to_string(),
                }
            })
        })
        .collect()
}

// --------------------------------------------------------------------------
// skill
// --------------------------------------------------------------------------

/// Directive coverage for `skill`.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The name comes from the advertised skill list | "named in available_skills" |
/// | Loading a skill injects its instructions into the conversation | "loads its instructions into this conversation" |
/// | The loaded instructions are followed for the rest of the task | "Follow them for the rest of the task" |
fn skill_spec() -> ToolSpec {
    ToolSpec {
        name: "skill".to_owned(),
        description: "Load a skill named in available_skills, which loads its instructions into \
                      this conversation. Follow them for the rest of the task."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "name",
                Property::string()
                    .described("The name of the skill, as advertised in available_skills"),
            )
            .build(),
        output_schema: None,
        config: declared_document("skill"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

fn run_skill(
    roots: &DiscoveryRoots,
    loaded: &Mutex<BTreeMap<String, BTreeSet<String>>>,
    session_id: &str,
    name: &str,
) -> Result<ToolExecutionOutput, ToolError> {
    let catalog = discover_extensions(
        roots,
        BTreeMap::new(),
        crate::skills::builtins::builtin_skills(),
        BTreeMap::new(),
    );
    let Some(skill) = catalog.skills.get(name) else {
        // An unknown name is answered with what does exist: a model that
        // guessed the name can correct itself without another round trip.
        let available = catalog
            .skills
            .keys()
            .take(MAX_LISTED_SKILLS)
            .cloned()
            .collect::<Vec<_>>();
        let listed = if available.is_empty() {
            "none".to_owned()
        } else {
            available.join(", ")
        };
        // The catalog's issue list is answered with rather than discarded: a
        // skill that is missing because its own file would not parse is
        // otherwise indistinguishable from one that was never written, and this
        // error is the only surface a mid-turn tool call reads.
        let unloadable = catalog
            .issues
            .iter()
            .filter(|issue| issue.mechanism == "skills")
            .take(MAX_LISTED_SKILLS)
            .map(|issue| format!("{}: {}", issue.path.display(), issue.message))
            .collect::<Vec<_>>();
        let unloadable = if unloadable.is_empty() {
            String::new()
        } else {
            format!(
                "; {} skill file(s) could not be loaded: {}",
                unloadable.len(),
                unloadable.join("; ")
            )
        };
        return Err(ToolError::Execution(format!(
            "skill `{name}` was not found; available skills: {listed}{unloadable}"
        )));
    };
    let directory = skill_directory(skill);
    // A skill already loaded in this conversation is acknowledged rather than
    // rendered again: the instructions are still in the transcript, and paying
    // for them twice buys nothing.
    let already_loaded = {
        let mut loaded = loaded
            .lock()
            .map_err(|_| ToolError::Execution("the skill ledger lock is poisoned".to_owned()))?;
        !loaded
            .entry(session_id.to_owned())
            .or_default()
            .insert(skill.name.clone())
    };
    Ok(skill_output(skill, directory.as_deref(), already_loaded))
}

/// The output a skill load answers with, shared by the `skill` tool and the
/// synthetic pair a slash invocation appends, so both paths deliver the same
/// bytes for the same skill.
fn skill_output(
    skill: &SkillDefinition,
    directory: Option<&Path>,
    already_loaded: bool,
) -> ToolExecutionOutput {
    let content = if already_loaded {
        format!(
            "The skill `{}` was already loaded earlier in this conversation; reuse those \
             instructions.",
            skill.name
        )
    } else {
        render_skill(skill, directory)
    };
    let directory_field = directory.map(|path| path.to_string_lossy().replace('\\', "/"));
    ToolExecutionOutput {
        model_text: reference_text::joined(&[
            ("name", skill.name.clone()),
            ("content", content.clone()),
            (
                "skill_dir",
                reference_text::optional(directory_field.clone()),
            ),
        ]),
        display: json!({"kind": "skill", "name": skill.name}),
        typed_result: json!({
            "name": skill.name,
            "content": content,
            "skill_dir": directory_field,
        }),
        chunks: Vec::new(),
    }
}

/// Resolves a slash invocation against the same catalog and loaded ledger the
/// `skill` tool answers from.
///
/// Reference `parse_skill_command`: the trimmed prompt's first word past the
/// `/` names the skill case-insensitively, and a name that is unknown or not
/// user invocable resolves to nothing. A resolved skill is recorded in the
/// ledger, so a later `skill` tool call is acknowledged instead of rendered
/// again.
struct SkillInvocationResolver {
    roots: DiscoveryRoots,
    loaded: Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>,
    session_id: String,
}

impl crate::skills::InvokedSkillResolver for SkillInvocationResolver {
    fn resolve(&self, prompt: &str) -> Option<crate::skills::InvokedSkill> {
        // The engine asks this question of every user message, and discovery
        // walks five roots parsing every `SKILL.md` it finds. A prompt that
        // cannot name a skill is answered before that walk is paid for, which
        // is also how the reference behaves: its catalog is built once with the
        // session, not once per turn.
        if !prompt.trim_start().starts_with('/') {
            return None;
        }
        let catalog = discover_extensions(
            &self.roots,
            BTreeMap::new(),
            crate::skills::builtins::builtin_skills(),
            BTreeMap::new(),
        );
        let parsed = crate::skills::parse_skill_command(&catalog.skills, prompt)?;
        let skill = catalog.skills.get(&parsed.name)?;
        if let Ok(mut loaded) = self.loaded.lock() {
            loaded
                .entry(self.session_id.clone())
                .or_default()
                .insert(skill.name.clone());
        }
        let directory = skill_directory(skill);
        Some(crate::skills::InvokedSkill {
            name: skill.name.clone(),
            loaded: skill_output(skill, directory.as_deref(), false),
            already_loaded: skill_output(skill, directory.as_deref(), true),
        })
    }
}

/// The directory a skill's files sit in, or [`None`] when it has none on disk.
///
/// A skill declared without a file on disk has no base directory, and the
/// reference then omits the two lines that would otherwise name an empty path.
fn skill_directory(skill: &SkillDefinition) -> Option<PathBuf> {
    let base = skill.path.as_deref()?.parent()?;
    base.is_dir().then(|| base.to_path_buf())
}

/// The block the model reads back, carrying the skill body, its base directory
/// and a sample of the files that sit next to it.
///
/// The walk is recursive and the names are relative to the base, which is what
/// makes a skill shipping `references/api.md` advertise that path rather than
/// only its top-level directory.
fn render_skill(skill: &SkillDefinition, base: Option<&Path>) -> String {
    let file_lines = base
        .map(skill_files)
        .unwrap_or_default()
        .iter()
        .map(|file| format!("<file>{file}</file>"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut lines = vec![
        crate::skills::skill_content_marker(&skill.name),
        format!("# Skill: {}", skill.name),
        String::new(),
        skill.body.trim().to_owned(),
        String::new(),
    ];
    if let Some(base) = base {
        lines.push(format!("Base directory for this skill: {}", base.display()));
        lines.push("Relative paths in this skill resolve against that base directory.".to_owned());
    }
    lines.extend([
        "Note: the file list below is a sample.".to_owned(),
        String::new(),
        "<skill_files>".to_owned(),
        file_lines,
        "</skill_files>".to_owned(),
        "</skill_content>".to_owned(),
    ]);
    lines.join("\n")
}

/// The files that ship with a skill, sorted, without its own `SKILL.md`, and
/// capped so a large bundle cannot flood the conversation.
fn skill_files(base: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let mut pending = vec![base.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if entry.file_name() != "SKILL.md"
                && let Ok(relative) = path.strip_prefix(base)
            {
                names.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    names.sort();
    names.truncate(MAX_LISTED_SKILL_FILES);
    names
}

// --------------------------------------------------------------------------
// web_fetch
// --------------------------------------------------------------------------

/// Directive coverage for `web_fetch`.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The tool retrieves the content of one URL | "Retrieve one web page" |
/// | HTML is converted to text before the model sees it | "HTML arrives as text" |
/// | Long pages are truncated | "a long page is truncated" |
/// | The timeout is optional and capped | the `timeout` description, "at most 120" |
fn web_fetch_spec() -> ToolSpec {
    ToolSpec {
        name: "web_fetch".to_owned(),
        description: "Retrieve one web page over http or https. HTML arrives as text with the \
                      markup stripped, and a long page is truncated rather than flooding the \
                      conversation."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "url",
                Property::string().described("The URL whose content is retrieved"),
            )
            .optional(
                "timeout",
                Property::integer()
                    .described("How long to wait, in seconds, at most 120")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build(),
        output_schema: None,
        config: declared_document("web_fetch"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

/// The target of a `web_fetch` call, refused before any network access when it
/// is empty or carries a scheme other than http.
fn fetch_url(arguments: &Value) -> Result<Url, ToolError> {
    let raw = arguments["url"].as_str().unwrap_or_default().trim();
    if raw.is_empty() {
        return Err(ToolError::SchemaViolation {
            path: "/url".to_owned(),
            message: "must not be empty".to_owned(),
        });
    }
    // A URL that already carries a scheme is judged on it. Anything else is a
    // protocol-relative or bare host, which the reference normalizes to https
    // rather than refusing.
    if let Ok(url) = Url::parse(raw) {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ToolError::Execution(format!(
                "`{}` is not an http or https scheme",
                url.scheme()
            )));
        }
        return Ok(url);
    }
    Url::parse(&format!("https://{}", raw.trim_start_matches('/')))
        .map_err(|error| ToolError::Execution(format!("`{raw}` is not a URL: {error}")))
}

/// How long one call may wait: what it asked for, bounded by the configured
/// ceiling, or the configured default when it asked for nothing.
fn fetch_timeout(arguments: &Value, settings: &WebFetchConfig) -> Result<Duration, ToolError> {
    let Some(requested) = arguments["timeout"].as_i64() else {
        return Ok(Duration::from_secs(settings.default_timeout));
    };
    if requested <= 0 {
        return Err(ToolError::SchemaViolation {
            path: "/timeout".to_owned(),
            message: "must be a positive number of seconds".to_owned(),
        });
    }
    let seconds = u64::try_from(requested).unwrap_or(settings.max_timeout);
    Ok(Duration::from_secs(seconds.min(settings.max_timeout)))
}

fn web_fetch_handler(config: ToolConfigResolver) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let arguments = invocation.arguments.clone();
            let settings: WebFetchConfig = config.view("web_fetch");
            Box::pin(async move { run_web_fetch(&arguments, &settings, &output).await })
        },
    )
}

async fn run_web_fetch(
    arguments: &Value,
    settings: &WebFetchConfig,
    output: &ToolOutputSink,
) -> Result<ToolExecutionOutput, ToolError> {
    let url = fetch_url(arguments)?;
    let timeout = fetch_timeout(arguments, settings)?;
    let host = url.host_str().unwrap_or("the requested host").to_owned();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(MAX_FETCH_REDIRECTS))
        .timeout(timeout)
        .user_agent(settings.user_agent.clone())
        .build()
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let response = client.get(url.clone()).send().await.map_err(|error| {
        // A URL can carry credentials or a query string, so the failure names
        // the host and nothing else.
        if error.is_timeout() {
            ToolError::Execution(format!(
                "fetching from {host} timed out after {} seconds",
                timeout.as_secs()
            ))
        } else if error.is_redirect() {
            ToolError::Execution(format!(
                "fetching from {host} exceeded {MAX_FETCH_REDIRECTS} redirects"
            ))
        } else {
            ToolError::Execution(format!("fetching from {host} failed"))
        }
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(ToolError::Execution(format!(
            "fetching from {host} returned HTTP {}",
            status.as_u16()
        )));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("text/plain")
        .to_owned();
    let body = response
        .text()
        .await
        .map_err(|_| ToolError::Execution(format!("the body from {host} is not text")))?;
    let text = if content_type.contains("html") {
        html_to_text(&body)
    } else {
        body
    };
    // The sink owns the turn's output budget, so the page is bounded by the
    // smaller of its own limit and what the sink still has room for.
    let limit = settings.max_content_bytes.min(output.remaining_bytes());
    let truncated = text.len() > limit;
    let content = if truncated {
        let mut boundary = limit;
        while boundary > 0 && !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        format!(
            "{}\n\n[content truncated at {boundary} bytes]",
            &text[..boundary]
        )
    } else {
        text
    };
    Ok(ToolExecutionOutput {
        model_text: content.clone(),
        display: json!({"kind": "webFetch", "url": url.as_str()}),
        typed_result: json!({
            "url": url.as_str(),
            "content": content,
            "contentType": content_type,
            "wasTruncated": truncated,
        }),
        chunks: Vec::new(),
    })
}

/// Strips markup so an HTML page reaches the model as prose.
///
/// The reference runs `markdownify`; there is no equivalent in this workspace
/// and the non-goals exclude execution-trace parity, so this drops the elements
/// that carry no prose and then the tags, which is what makes the body
/// readable.
fn html_to_text(html: &str) -> String {
    let without_blocks = ["script", "style", "noscript", "iframe", "svg"]
        .into_iter()
        .fold(html.to_owned(), |document, tag| {
            drop_element(&document, tag)
        });
    let mut text = String::with_capacity(without_blocks.len());
    let mut tag = String::new();
    let mut inside_tag = false;
    for character in without_blocks.chars() {
        match character {
            '<' => {
                inside_tag = true;
                tag.clear();
            }
            '>' if inside_tag => {
                inside_tag = false;
                // A block-level tag ends a line; an inline one only separates
                // words, so `a<b>bold</b>c` does not become three lines.
                text.push(if is_block_tag(&tag) { '\n' } else { ' ' });
            }
            _ if inside_tag => tag.push(character),
            _ => text.push(character),
        }
    }
    let text = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a tag body names an element that breaks the line around its text.
fn is_block_tag(tag: &str) -> bool {
    let name = tag
        .trim_start_matches('/')
        .split([' ', '\t', '\n', '\r', '/'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "br"
            | "div"
            | "dd"
            | "dl"
            | "dt"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "table"
            | "tbody"
            | "td"
            | "tfoot"
            | "th"
            | "thead"
            | "tr"
            | "ul"
    )
}

/// Removes every `<tag ...> ... </tag>` span, case-insensitively.
///
/// The case fold is ASCII-only on purpose: element names are ASCII, and a full
/// Unicode fold changes the byte length of characters such as U+0130, which
/// would slide every offset found in the folded copy off its counterpart in the
/// original and slice a fetched page mid-codepoint.
fn drop_element(document: &str, tag: &str) -> String {
    let lowered = document.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut result = String::with_capacity(document.len());
    let mut cursor = 0;
    while let Some(start) = lowered[cursor..].find(&open) {
        let start = cursor + start;
        result.push_str(&document[cursor..start]);
        cursor = match lowered[start..].find(&close) {
            Some(end) => start + end + close.len(),
            None => document.len(),
        };
    }
    result.push_str(&document[cursor..]);
    result
}

// --------------------------------------------------------------------------
// web_search
// --------------------------------------------------------------------------

/// Directive coverage for `web_search`.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The tool answers a query from live web results | "Answer a question from live web results" |
/// | It is reached for when the answer may have changed since training | "reach for it when the answer may have moved since training" |
/// | The answer comes back with its sources | "The answer comes back with the pages it rests on" |
fn web_search_spec() -> ToolSpec {
    ToolSpec {
        name: "web_search".to_owned(),
        description: "Answer a question from live web results: reach for it when the answer may \
                      have moved since training rather than guessing. The answer comes back with \
                      the pages it rests on."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "query",
                Property::string()
                    .constrained("minLength", 1)
                    .described("The search query"),
            )
            .build(),
        output_schema: None,
        config: declared_document("web_search"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

fn web_search_handler(access: WebSearchAccess, config: ToolConfigResolver) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let access = access.clone();
            let settings: WebSearchConfig = config.view("web_search");
            let query = invocation.arguments["query"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            Box::pin(async move { run_web_search(&access, &settings, &query).await })
        },
    )
}

async fn run_web_search(
    access: &WebSearchAccess,
    settings: &WebSearchConfig,
    query: &str,
) -> Result<ToolExecutionOutput, ToolError> {
    if query.trim().is_empty() {
        return Err(ToolError::SchemaViolation {
            path: "/query".to_owned(),
            message: "must not be empty".to_owned(),
        });
    }
    let endpoint = format!("{}/v1/conversations", access.endpoint.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(settings.timeout))
        .build()
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let response = client
        .post(&endpoint)
        .bearer_auth(access.api_key.expose_secret())
        // The reference sends the SDK's own user agent for this call and tags
        // the request as a secondary one, which is how the endpoint tells a
        // tool-issued search from a turn. The product name stays this port's:
        // the prefix is what the endpoint routes on, not the identity.
        .header(reqwest::header::USER_AGENT, SEARCH_USER_AGENT)
        .json(&json!({
            "model": settings.model,
            "instructions": "Always use the web_search tool to answer the query. Never answer \
                             from memory alone.",
            "tools": [{"type": "web_search"}],
            "inputs": query,
            "store": false,
            "metadata": search_request_metadata(),
        }))
        .send()
        .await
        .map_err(|_| ToolError::Execution("the web search request failed".to_owned()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(ToolError::Execution(format!(
            "the web search endpoint returned HTTP {}",
            status.as_u16()
        )));
    }
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| ToolError::Execution("the web search response is not JSON".to_owned()))?;
    let (answer, sources) = parse_search_response(&payload);
    if answer.is_empty() {
        return Err(ToolError::Execution(
            "the web search response carries no text".to_owned(),
        ));
    }
    Ok(ToolExecutionOutput {
        model_text: answer.clone(),
        display: json!({"kind": "webSearch", "query": query}),
        typed_result: json!({"query": query, "answer": answer, "sources": sources}),
        chunks: Vec::new(),
    })
}

/// The request metadata reference `build_request_metadata` attaches, with the
/// fields this port can answer for.
///
/// `exclude_none` upstream means an absent field is left out rather than sent
/// as null, so the same fields are omitted here.
fn search_request_metadata() -> Value {
    json!({
        "os": std::env::consts::OS,
        "version": env!("CARGO_PKG_VERSION"),
        "call_type": "secondary_call",
    })
}

/// Pulls the answer text and the cited pages out of a conversations response.
///
/// `content` is a plain string for a short answer and a chunk list otherwise,
/// and the citations arrive as `tool_reference` chunks carrying a URL.
fn parse_search_response(payload: &Value) -> (String, Vec<Value>) {
    let mut answer = String::new();
    let mut sources = Vec::new();
    let mut seen = BTreeSet::new();
    let outputs = payload["outputs"].as_array().cloned().unwrap_or_default();
    for entry in outputs {
        match &entry["content"] {
            Value::String(text) => answer.push_str(text),
            Value::Array(chunks) => {
                for chunk in chunks {
                    match chunk["type"].as_str() {
                        Some("text") => {
                            answer.push_str(chunk["text"].as_str().unwrap_or_default());
                        }
                        Some("tool_reference") => {
                            let Some(url) = chunk["url"].as_str() else {
                                continue;
                            };
                            if seen.insert(url.to_owned()) {
                                sources.push(json!({
                                    "title": chunk["title"].as_str().unwrap_or(url),
                                    "url": url,
                                }));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    (answer.trim().to_owned(), sources)
}

#[cfg(test)]
mod tests {
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
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        let address = listener.local_addr().expect("listener address").to_string();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buffer = [0_u8; 8192];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let _ = sender.send(String::from_utf8_lossy(&buffer[..read]).into_owned());
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });
        (format!("http://{address}"), receiver)
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

    /// A page past the limit comes back truncated rather than overflowing the
    /// turn's output budget.
    #[tokio::test]
    async fn a_long_page_is_truncated_inside_the_output_budget() {
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
        assert_eq!(fetched.typed_result["wasTruncated"], json!(true));
        assert!(
            fetched.model_text.len() < settings.max_content_bytes + 128,
            "the page must stay inside the limit"
        );
        assert!(
            fetched.model_text.contains("content truncated at"),
            "the truncation is reported to the model"
        );
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
        assert_eq!(answered.model_text, "42");
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

    #[test]
    fn the_fetch_timeout_defaults_and_is_capped() {
        let settings: WebFetchConfig = ToolConfigResolver::new().view("web_fetch");
        assert_eq!(
            fetch_timeout(&json!({"timeout": Value::Null}), &settings).expect("default"),
            Duration::from_secs(settings.default_timeout)
        );
        assert_eq!(
            fetch_timeout(&json!({"timeout": 9_000}), &settings).expect("capped"),
            Duration::from_secs(settings.max_timeout)
        );
        assert!(fetch_timeout(&json!({"timeout": 0}), &settings).is_err());

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
        assert_eq!(
            fetch_timeout(&json!({"timeout": 9_000}), &configured).expect("capped"),
            Duration::from_secs(7)
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
