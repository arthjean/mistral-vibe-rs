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
use crate::policy::{ApprovalAgent, PermissionRequirement, PermissionStore, PolicyGuardedTool};
use crate::schema::{ObjectSchema, Property};
use crate::tools::{
    OwnedToolHandlerFuture, RegistrationOutcome, ToolAvailability, ToolError, ToolExecutionOutput,
    ToolHandler, ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource,
    ToolSpec,
};

/// Reference `TodoConfig.max_todos`.
const MAX_TODOS: usize = 100;
/// Reference `WebFetchConfig.default_timeout`.
const DEFAULT_FETCH_TIMEOUT_SECONDS: u64 = 30;
/// Reference `WebFetchConfig.max_timeout`, also the cap the schema description
/// states.
const MAX_FETCH_TIMEOUT_SECONDS: u64 = 120;
/// Reference `WebFetchConfig.max_content_bytes`.
const MAX_FETCH_CONTENT_BYTES: usize = 120_000;
/// The redirect budget the security NFR sets for `web_fetch`.
const MAX_FETCH_REDIRECTS: usize = 5;
/// Reference `WebFetchConfig.user_agent`: a browser string, because a plain
/// bot agent is refused by a large share of the pages an operator asks for.
const FETCH_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/120.0.0.0 Safari/537.36";
/// Reference `WebSearchConfig.timeout`.
const SEARCH_TIMEOUT_SECONDS: u64 = 120;
/// How many skill names an unknown-skill error lists before it truncates.
const MAX_LISTED_SKILLS: usize = 40;
/// Reference `skill.py:_MAX_LISTED_FILES`.
const MAX_LISTED_SKILL_FILES: usize = 10;

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
    /// Reference `WebSearchConfig.model`.
    pub model: String,
    pub api_key: SecretString,
}

impl std::fmt::Debug for WebSearchAccess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebSearchAccess")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl WebSearchAccess {
    /// Reference `WebSearchConfig.model`.
    pub const DEFAULT_MODEL: &'static str = "mistral-vibe-cli-with-tools";
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
            model: Self::DEFAULT_MODEL.to_owned(),
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
    /// `working_directory` and `project_trusted` reach the skill catalog, which
    /// is discovered per session because a project may ship its own skills.
    pub fn register(
        &self,
        session_id: &str,
        working_directory: &Path,
        project_trusted: bool,
        registry: &ToolRegistry,
        policy: PermissionStore,
        approval: Arc<dyn ApprovalAgent>,
    ) -> Result<Vec<RegistrationOutcome>, ToolError> {
        let mut outcomes = vec![
            registry.register(todo_spec(), self.todo_handler(session_id))?,
            registry.register(
                skill_spec(),
                self.skill_handler(working_directory, project_trusted),
            )?,
            registry.register(
                web_fetch_spec(),
                Arc::new(PolicyGuardedTool::new(
                    "web_fetch",
                    policy.clone(),
                    approval.clone(),
                    Arc::new(|invocation| {
                        let url = fetch_url(&invocation.arguments)?;
                        Ok(vec![PermissionRequirement::Network { url }])
                    }),
                    web_fetch_handler(),
                )),
            )?,
        ];
        if let Some(access) = &self.web_search {
            let access = access.clone();
            let scope = Url::parse(&access.endpoint).map_err(|error| {
                ToolError::Execution(format!(
                    "the web search endpoint `{}` is not a URL: {error}",
                    access.endpoint
                ))
            })?;
            outcomes.push(registry.register(
                web_search_spec(),
                Arc::new(PolicyGuardedTool::new(
                    "web_search",
                    policy,
                    approval,
                    Arc::new(move |_invocation| {
                        Ok(vec![PermissionRequirement::Network { url: scope.clone() }])
                    }),
                    web_search_handler(access),
                )),
            )?);
        }
        Ok(outcomes)
    }

    fn todo_handler(&self, session_id: &str) -> Arc<dyn ToolHandler> {
        let todos = self.todos.clone();
        let session_id = session_id.to_owned();
        Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let todos = todos.clone();
                let session_id = session_id.clone();
                let arguments = invocation.arguments.clone();
                Box::pin(async move { run_todo(&todos, &session_id, &arguments) })
            },
        )
    }

    fn skill_handler(
        &self,
        working_directory: &Path,
        project_trusted: bool,
    ) -> Arc<dyn ToolHandler> {
        let roots = DiscoveryRoots {
            configured: Vec::new(),
            project: vec![working_directory.join(".vibe")],
            user: vec![self.vibe_home.join("extensions")],
            project_trusted,
        };
        Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let roots = roots.clone();
                let name = invocation.arguments["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                Box::pin(async move { run_skill(&roots, &name) })
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
        config: json!({"maxTodos": MAX_TODOS}),
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
            if items.len() > MAX_TODOS {
                return Err(ToolError::Execution(format!(
                    "the todo list holds {} items, exceeding the {MAX_TODOS}-item limit",
                    items.len()
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
    Ok(ToolExecutionOutput {
        model_text: format!("{verb} {total} todos"),
        display: json!({"kind": "todo", "count": total}),
        // The transcript renders the todo widget from the typed result, so the
        // items travel there rather than in the display metadata.
        typed_result: json!({"verb": verb, "todos": items, "totalCount": total}),
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
        config: Value::Null,
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

fn run_skill(roots: &DiscoveryRoots, name: &str) -> Result<ToolExecutionOutput, ToolError> {
    let catalog = discover_extensions(roots, BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
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
        return Err(ToolError::Execution(format!(
            "skill `{name}` was not found; available skills: {listed}"
        )));
    };
    let content = render_skill(skill);
    Ok(ToolExecutionOutput {
        model_text: content.clone(),
        display: json!({"kind": "skill", "name": skill.name}),
        typed_result: json!({"name": skill.name, "content": content}),
        chunks: Vec::new(),
    })
}

/// The block the model reads back, carrying the skill body, its base directory
/// and a sample of the files that sit next to it.
fn render_skill(skill: &SkillDefinition) -> String {
    let base = skill.path.parent().unwrap_or(Path::new("."));
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(base) {
        let mut names = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != "SKILL.md")
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names.truncate(MAX_LISTED_SKILL_FILES);
        files = names;
    }
    let file_lines = files
        .iter()
        .map(|file| format!("<file>{file}</file>"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "<skill_content name=\"{name}\">\n# Skill: {name}\n\n{body}\n\nBase directory for this \
         skill: {base}\nRelative paths in this skill are relative to this base \
         directory.\nNote: file list is sampled.\n\n<skill_files>\n{file_lines}\n</skill_files>\n\
         </skill_content>",
        name = skill.name,
        body = skill.body.trim(),
        base = base.display(),
    )
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
        config: json!({
            "maxContentBytes": MAX_FETCH_CONTENT_BYTES,
            "maxRedirects": MAX_FETCH_REDIRECTS,
            "timeoutSeconds": DEFAULT_FETCH_TIMEOUT_SECONDS,
        }),
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
    // protocol-relative or bare host, which the reference normalises to https
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

fn fetch_timeout(arguments: &Value) -> Result<Duration, ToolError> {
    let Some(requested) = arguments["timeout"].as_i64() else {
        return Ok(Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECONDS));
    };
    if requested <= 0 {
        return Err(ToolError::SchemaViolation {
            path: "/timeout".to_owned(),
            message: "must be a positive number of seconds".to_owned(),
        });
    }
    let seconds = u64::try_from(requested).unwrap_or(MAX_FETCH_TIMEOUT_SECONDS);
    Ok(Duration::from_secs(seconds.min(MAX_FETCH_TIMEOUT_SECONDS)))
}

fn web_fetch_handler() -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let arguments = invocation.arguments.clone();
            Box::pin(async move { run_web_fetch(&arguments, &output).await })
        },
    )
}

async fn run_web_fetch(
    arguments: &Value,
    output: &ToolOutputSink,
) -> Result<ToolExecutionOutput, ToolError> {
    let url = fetch_url(arguments)?;
    let timeout = fetch_timeout(arguments)?;
    let host = url.host_str().unwrap_or("the requested host").to_owned();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(MAX_FETCH_REDIRECTS))
        .timeout(timeout)
        .user_agent(FETCH_USER_AGENT)
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
    let limit = MAX_FETCH_CONTENT_BYTES.min(output.remaining_bytes());
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
        config: json!({"timeoutSeconds": SEARCH_TIMEOUT_SECONDS}),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

fn web_search_handler(access: WebSearchAccess) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let access = access.clone();
            let query = invocation.arguments["query"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            Box::pin(async move { run_web_search(&access, &query).await })
        },
    )
}

async fn run_web_search(
    access: &WebSearchAccess,
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
        .timeout(Duration::from_secs(SEARCH_TIMEOUT_SECONDS))
        .build()
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let response = client
        .post(&endpoint)
        .bearer_auth(access.api_key.expose_secret())
        .json(&json!({
            "model": access.model,
            "instructions": "Always use the web_search tool to answer the query. Never answer \
                             from memory alone.",
            "tools": [{"type": "web_search"}],
            "inputs": query,
            "store": false,
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
            .register("session-1", root, true, &registry, policy, approval)
            .expect("register");
        registry
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
            model: WebSearchAccess::DEFAULT_MODEL.to_owned(),
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
        assert_eq!(access.model, WebSearchAccess::DEFAULT_MODEL);

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
        assert_eq!(read.model_text, "Retrieved 1 todos");
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

    /// A page past the limit comes back truncated rather than overflowing the
    /// turn's output budget.
    #[tokio::test]
    async fn a_long_page_is_truncated_inside_the_output_budget() {
        let directory = tempdir().expect("tempdir");
        let registry = registered_with(directory.path(), None, Arc::new(AllowApproval)).await;
        let body = "z".repeat(MAX_FETCH_CONTENT_BYTES + 4_096);
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
            fetched.model_text.len() < MAX_FETCH_CONTENT_BYTES + 128,
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
        let output = run_todo(&todos, "session-1", &json!({"action": "read"}))
            .expect("reading an unwritten list is not an error");
        assert_eq!(output.typed_result["todos"], json!([]));
        assert_eq!(output.model_text, "Retrieved 0 todos");
    }

    #[test]
    fn a_written_todo_list_reads_back_with_the_schema_defaults_applied() {
        let todos = Mutex::new(BTreeMap::new());
        run_todo(
            &todos,
            "session-1",
            &json!({"action": "write", "todos": [{"id": "a", "content": "ship"}]}),
        )
        .expect("write");
        let output = run_todo(&todos, "session-1", &json!({"action": "read"})).expect("read");
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
        )
        .expect("write");
        let other = run_todo(&todos, "session-2", &json!({"action": "read"})).expect("read");
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
        )
        .expect_err("a duplicated id is refused");
        assert!(error.to_string().contains('a'), "{error}");
        assert!(error.to_string().contains("more than once"), "{error}");
    }

    #[test]
    fn an_unknown_todo_action_names_the_two_that_exist() {
        let todos = Mutex::new(BTreeMap::new());
        let error = run_todo(&todos, "session-1", &json!({"action": "append"}))
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
    fn a_bare_host_is_normalised_to_https_like_the_reference() {
        let url = fetch_url(&json!({"url": "example.com/page"})).expect("a bare host normalises");
        assert_eq!(url.as_str(), "https://example.com/page");
        let relative =
            fetch_url(&json!({"url": "//example.com/page"})).expect("protocol-relative normalises");
        assert_eq!(relative.as_str(), "https://example.com/page");
    }

    #[test]
    fn the_fetch_timeout_defaults_and_is_capped() {
        assert_eq!(
            fetch_timeout(&json!({"timeout": Value::Null})).expect("default"),
            Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECONDS)
        );
        assert_eq!(
            fetch_timeout(&json!({"timeout": 9_000})).expect("capped"),
            Duration::from_secs(MAX_FETCH_TIMEOUT_SECONDS)
        );
        assert!(fetch_timeout(&json!({"timeout": 0})).is_err());
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
