//! Typed details for the entries the projection publishes.
//!
//! A history entry used to carry an untyped `detail` object, which meant a
//! client had to guess what an effect was and could not render a notice written
//! by the other implementation. The reference types all three: an effect detail
//! is a 12-variant union keyed on the tool's semantic kind, a notice detail an
//! 8-variant union keyed on what happened, and a callback detail names whether
//! it wants an approval or an answer.
//!
//! Each effect detail carries its presentation. The call display is computed
//! once, here, where the projection builds the entry, so the terminal client,
//! the editor bridge and a reference client all render the same header instead
//! of each deriving one from the raw arguments.

use std::fmt;

use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::policy::PermissionRequirement;

// --------------------------------------------------------------------------
// Vocabularies
// --------------------------------------------------------------------------

/// What an effect is doing, independent of the tool that does it.
///
/// The kind decides the presentation and the shape of `input`, which is why a
/// remote tool with no dedicated variant lands on [`ToolEffectKind::Tool`] and
/// publishes its raw arguments rather than an invented shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffectKind {
    Tool,
    Shell,
    FileEdit,
    FileSearch,
    FileRead,
    Todo,
    FileWrite,
    UserQuestion,
    WebSearch,
    WebFetch,
    Skill,
    Subagent,
}

impl ToolEffectKind {
    /// Every kind, in the order the reference vocabulary declares them.
    pub const ALL: [Self; 12] = [
        Self::Tool,
        Self::Shell,
        Self::FileEdit,
        Self::FileSearch,
        Self::FileRead,
        Self::Todo,
        Self::FileWrite,
        Self::UserQuestion,
        Self::WebSearch,
        Self::WebFetch,
        Self::Skill,
        Self::Subagent,
    ];

    /// Which kind a tool name maps to. A name with no mapping is a plain tool.
    #[must_use]
    pub fn from_tool_name(name: &str) -> Self {
        match name {
            "shell" | "bash" | "windows_shell" | "experimental_bash" | "git_bash"
            | "powershell" => Self::Shell,
            "edit" | "patch" => Self::FileEdit,
            "search" | "grep" => Self::FileSearch,
            "read" | "read_file" => Self::FileRead,
            "todo" => Self::Todo,
            "write" | "write_file" => Self::FileWrite,
            "ask_user_question" => Self::UserQuestion,
            "web_search" => Self::WebSearch,
            "web_fetch" => Self::WebFetch,
            "skill" => Self::Skill,
            "task" => Self::Subagent,
            _ => Self::Tool,
        }
    }

    /// What a client shows while the effect runs.
    #[must_use]
    pub fn status_text(self) -> &'static str {
        match self {
            Self::Tool => "Running tool",
            Self::Shell => "Running command",
            Self::FileEdit => "Editing files",
            Self::FileSearch => "Searching files",
            Self::FileRead => "Reading file",
            Self::Todo => "Managing todos",
            Self::FileWrite => "Writing file",
            Self::UserQuestion => "Waiting for user input",
            Self::WebSearch => "Searching the web",
            Self::WebFetch => "Fetching page",
            Self::Skill => "Loading skill",
            Self::Subagent => "Running subagent",
        }
    }

    /// The wire value, which is also the label a diagnostic prints.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Shell => "shell",
            Self::FileEdit => "file_edit",
            Self::FileSearch => "file_search",
            Self::FileRead => "file_read",
            Self::Todo => "todo",
            Self::FileWrite => "file_write",
            Self::UserQuestion => "user_question",
            Self::WebSearch => "web_search",
            Self::WebFetch => "web_fetch",
            Self::Skill => "skill",
            Self::Subagent => "subagent",
        }
    }

    /// The verb pair a header uses while running and once settled. `Todo` is
    /// absent because its `action` argument, not its kind, decides the verb.
    fn verbs(self) -> (&'static str, &'static str) {
        match self {
            Self::Shell | Self::Subagent => ("Running", "Ran"),
            Self::FileRead => ("Reading", "Read"),
            Self::FileWrite => ("Creating", "Created"),
            Self::FileEdit => ("Editing", "Edited"),
            Self::FileSearch | Self::WebSearch => ("Searching", "Searched"),
            Self::UserQuestion => ("Asking", "Asked"),
            Self::WebFetch => ("Fetching", "Fetched"),
            Self::Skill => ("Loading", "Loaded"),
            Self::Tool | Self::Todo => ("Running", "Ran"),
        }
    }
}

/// Where a tracked task stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoEffectStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

/// How much a tracked task matters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoEffectPriority {
    Low,
    #[default]
    Medium,
    High,
}

/// One tracked task, as the todo effect's input publishes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoEffectItem {
    pub id: String,
    pub content: String,
    #[serde(default)]
    pub status: TodoEffectStatus,
    #[serde(default)]
    pub priority: TodoEffectPriority,
}

impl TodoEffectItem {
    /// The item a raw todo argument names, defaulting the fields it omitted.
    ///
    /// Lenient rather than strict: an argument the model got slightly wrong
    /// must still publish an entry a client can render, and the variant's model
    /// forbids the surplus keys a passthrough would carry.
    fn from_argument(value: &Value) -> Self {
        Self {
            id: string_argument(value, &["id"]),
            content: string_argument(value, &["content"]),
            status: enum_argument(value, "status"),
            priority: enum_argument(value, "priority"),
        }
    }
}

/// Which stage of a hook run a notice reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookScope {
    #[default]
    PostAgent,
    PreTool,
    PostTool,
}

/// How a hook finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookSeverity {
    Ok,
    Warning,
    Error,
}

/// What an operator may answer an approval callback with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecisionType {
    Approve,
    ApproveForSession,
    ApprovePermanently,
    Deny,
    CancelTurn,
}

impl ApprovalDecisionType {
    /// Every decision, which is also the default choice set an approval offers.
    pub const ALL: [Self; 5] = [
        Self::Approve,
        Self::ApproveForSession,
        Self::ApprovePermanently,
        Self::Deny,
        Self::CancelTurn,
    ];
}

// --------------------------------------------------------------------------
// Presentation
// --------------------------------------------------------------------------

/// How a client renders an effect while it is being called.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectCallDisplay {
    /// The collapsed one-line form.
    pub summary: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub suffix: String,
    #[serde(default)]
    pub verb: String,
    /// What is being acted on. The live and settled headers share it, so a
    /// finished call never renames its own subject.
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub settled_verb: String,
    #[serde(default)]
    pub settled_message: Option<String>,
    /// What the loading indicator says while the effect runs.
    pub status_text: String,
}

impl EffectCallDisplay {
    /// Fills the four defaults the reference's adapter applies after the tool
    /// has spoken, so a display that named none of them still renders.
    ///
    /// `settledMessage` falls back to the summary and not to the message, which
    /// is what makes a settled header name the whole call rather than the
    /// subject the running header narrowed to. A display that named one of the
    /// four keeps it: the adapter only fills what is missing.
    fn fill_defaults(&mut self) {
        if self.message.is_none() {
            self.message = Some(self.summary.clone());
        }
        if self.verb.is_empty() {
            self.verb = "Running".to_owned();
        }
        if self.settled_message.is_none() {
            self.settled_message = Some(self.summary.clone());
        }
        if self.settled_verb.is_empty() {
            self.settled_verb = "Ran".to_owned();
        }
    }

    /// The subject the header renders, falling back to the collapsed summary
    /// when a call arrived without the argument that names it.
    #[must_use]
    pub fn subject(&self) -> &str {
        self.message
            .as_deref()
            .filter(|message| !message.is_empty())
            .unwrap_or(&self.summary)
    }
}

/// How a client renders an effect once it has settled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectResultDisplay {
    pub success: bool,
    #[serde(default)]
    pub verb: String,
    pub message: String,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub suffix: String,
}

/// Which registry published a tool this session only proxies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteToolKind {
    Mcp,
    Connector,
}

/// The remote a proxied call reaches, carried beside the published name.
///
/// The published name is not invertible: `public_tool_name` sanitizes an MCP
/// name and joins it to a sanitized alias, so `acme_create_issue` no longer
/// says where the alias stops. The presentation needs the name the server
/// itself publishes, so the registration that knows it hands it forward rather
/// than letting the projection guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteToolOrigin {
    pub kind: RemoteToolKind,
    /// The name the server publishes the tool under, which the alias prefix is
    /// absent from.
    pub remote_name: String,
}

impl RemoteToolOrigin {
    /// The origin of a tool an MCP server publishes as `remote_name`.
    #[must_use]
    pub fn mcp(remote_name: impl Into<String>) -> Self {
        Self {
            kind: RemoteToolKind::Mcp,
            remote_name: remote_name.into(),
        }
    }

    /// The origin of a tool a connector publishes as `remote_name`.
    #[must_use]
    pub fn connector(remote_name: impl Into<String>) -> Self {
        Self {
            kind: RemoteToolKind::Connector,
            remote_name: remote_name.into(),
        }
    }

    /// What the loading indicator says while a proxied call runs.
    ///
    /// The reference's two proxy factories close over the remote tool, so the
    /// sentence names the tool as its server published it and never the alias
    /// prefix the session added. A registration that recorded no remote name
    /// falls back to the published one rather than naming nothing.
    #[must_use]
    fn status_text(&self, published_name: &str) -> String {
        let named = if self.remote_name.is_empty() {
            published_name
        } else {
            &self.remote_name
        };
        match self.kind {
            RemoteToolKind::Mcp => format!("Calling MCP tool {named}"),
            RemoteToolKind::Connector => format!("Calling connector tool {named}"),
        }
    }

    /// The subject the settled header names, which the connector prefixes.
    fn settled_subject(&self, answered: &str) -> String {
        match self.kind {
            RemoteToolKind::Mcp => answered.to_owned(),
            RemoteToolKind::Connector => format!("connector {answered}"),
        }
    }
}

/// What a settled proxied call reports, in the three fields the reference's
/// adapter reads before it hands the result to the proxy.
///
/// They are named rather than positional because two of the three are strings
/// that would otherwise be transposable, and reading an error as a skip reason
/// would publish a failure as a skip.
#[derive(Debug, Clone, Copy)]
pub struct RemoteSettlement<'a> {
    /// What the call failed with, empty when it did not fail.
    pub error: &'a str,
    /// Whether the call never ran, which a reason alone does not say.
    pub skipped: bool,
    /// Why it never ran, empty when nothing named a reason.
    pub skip_reason: &'a str,
    /// The typed result the server answered with.
    pub output: &'a Value,
}

impl<'a> RemoteSettlement<'a> {
    /// The settlement of a call that answered with `output`.
    #[must_use]
    pub fn answered(output: &'a Value) -> Self {
        Self {
            error: "",
            skipped: false,
            skip_reason: "",
            output,
        }
    }

    /// The settlement of a call that failed with `error`.
    #[must_use]
    pub fn failed(error: &'a str, output: &'a Value) -> Self {
        Self {
            error,
            skipped: false,
            skip_reason: "",
            output,
        }
    }

    /// The settlement of a call that never ran.
    #[must_use]
    pub fn skipped(skip_reason: &'a str, output: &'a Value) -> Self {
        Self {
            error: "",
            skipped: true,
            skip_reason,
            output,
        }
    }
}

/// The result a remote server answered with, as far as the settled header
/// reads it.
///
/// The reference models it as `MCPToolResult`, which its own proxy builds, so
/// every field it reads is the proxy's. This port's peers publish the server's
/// `structuredContent` verbatim as the typed result, so the same keys can be
/// the server's instead: a tool answering `{"ok": false}` about its own subject
/// is not a proxy reporting a failed call. `server` is the field that tells the
/// two apart, because only the proxy's own result names the source, which is
/// why it is required here rather than defaulted. A payload that is not an
/// object at all is what the reference's `isinstance` check rejects, and it
/// settles as a failure.
#[derive(Debug, Deserialize)]
struct RemoteToolResult {
    #[serde(default = "remote_result_ok")]
    ok: bool,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    server: Option<String>,
}

impl RemoteToolResult {
    /// Whether this payload is the proxy's own result rather than the content
    /// the remote tool answered about its own subject.
    const fn is_the_proxys_own(&self) -> bool {
        self.server.is_some()
    }
}

const fn remote_result_ok() -> bool {
    true
}

// --------------------------------------------------------------------------
// Effect detail
// --------------------------------------------------------------------------

/// A tool call as a client renders it.
///
/// One struct rather than twelve: the variants differ only by `kind`, by the
/// shape `input` takes and by the subagent's child session, so the union is
/// discriminated on the wire without twelve near-identical Rust declarations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectDetail {
    pub kind: ToolEffectKind,
    pub tool_name: String,
    pub display: EffectCallDisplay,
    /// The call arguments in the shape this kind declares. Never null: a
    /// generic effect publishes an object, which is what its variant requires.
    #[serde(default)]
    pub input: Value,
    /// Only the subagent variant declares it, so it stays off every other kind's
    /// wire form rather than being published as a surplus null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
    /// The remote this call is proxied to, absent for a tool the session
    /// implements itself. It routes the call header, the status text and the
    /// settled header, which is why it travels with the detail instead of being
    /// re-derived from the published name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteToolOrigin>,
}

impl EffectDetail {
    /// The detail for a call to `tool_name` with `arguments`.
    #[must_use]
    pub fn for_call(tool_name: &str, arguments: &Value) -> Self {
        // A caller holding a decoded object holds no order to keep: the map it
        // read them from iterates lexicographically already.
        Self::for_decoded_call(tool_name, arguments, &[], None)
    }

    /// The shared constructor, told which order the arguments arrived in and
    /// which remote the call is proxied to.
    fn for_decoded_call(
        tool_name: &str,
        arguments: &Value,
        wire_order: &[String],
        remote: Option<&RemoteToolOrigin>,
    ) -> Self {
        // A proxy takes none of the per-kind presentations whatever its
        // published name reads like, which the generic kind is what expresses.
        let kind = if remote.is_some() {
            ToolEffectKind::Tool
        } else {
            ToolEffectKind::from_tool_name(tool_name)
        };
        Self {
            kind,
            tool_name: tool_name.to_owned(),
            display: call_display(kind, tool_name, arguments, remote, wire_order),
            input: project_input(kind, arguments),
            child_session_id: None,
            remote: remote.cloned(),
        }
    }

    /// The detail for a call this session proxies to `remote`.
    ///
    /// The kind is the generic one whatever the published name reads like: the
    /// reference routes a proxy through the base presentation, so a server that
    /// publishes a tool called `bash` is still presented as a remote call and
    /// not as a shell.
    #[must_use]
    pub fn for_proxied_call(tool_name: &str, arguments: &str, remote: &RemoteToolOrigin) -> Self {
        Self::for_decoded_call(tool_name, &decode_arguments(arguments), &[], Some(remote))
    }

    /// The detail for a call whose arguments were recorded as a JSON string,
    /// which is how the engine carries them.
    #[must_use]
    pub fn for_encoded_call(tool_name: &str, arguments: &str) -> Self {
        Self::for_decoded_call(
            tool_name,
            &decode_arguments(arguments),
            &wire_key_order(arguments),
            None,
        )
    }
}

/// The arguments a tool call carried, whether they were encoded as a JSON
/// string, already parsed, or never recorded.
fn decode_arguments(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({}))
}

/// The order the top-level arguments arrived in.
///
/// The reference interpolates the `dict` its model was called with, which keeps
/// insertion order, so the fallback summary names the first three arguments as
/// the caller wrote them. Decoding into a [`serde_json::Map`] loses that: this
/// port does not build `serde_json` with `preserve_order`, and turning it on
/// would reorder every object it serializes, including the ones a measured
/// parity claim compares byte for byte. Reading the keys back off the same text
/// keeps the order where it is needed and nowhere else. Arguments that never
/// decoded name no order, and the summary falls back to the map's own.
fn wire_key_order(arguments: &str) -> Vec<String> {
    serde_json::from_str::<KeyOrder>(arguments).map_or_else(|_| Vec::new(), |order| order.0)
}

/// The keys of a JSON object, in the order the text carries them.
struct KeyOrder(Vec<String>);

impl<'de> Deserialize<'de> for KeyOrder {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Keys;

        impl<'de> Visitor<'de> for Keys {
            type Value = Vec<String>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A>(self, mut entries: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut keys = Vec::new();
                while let Some(key) = entries.next_key::<String>()? {
                    entries.next_value::<IgnoredAny>()?;
                    keys.push(key);
                }
                Ok(keys)
            }
        }

        deserializer.deserialize_map(Keys).map(Self)
    }
}

/// Projects raw call arguments onto the fields the kind's input model declares.
///
/// The reference input models forbid surplus fields, so the projection copies
/// what the variant declares and drops the rest instead of forwarding an object
/// a conforming client would reject.
/// The spellings a call or a result names a file under.
///
/// Reference argument models are snake_case and the wire projection is
/// camelCase, so a value read back from either side answers to both, and a tool
/// that names the field `path` answers to that too. Naming the list once is
/// what keeps two readers of the same field from accepting different spellings.
const FILE_PATH_KEYS: &[&str] = &["file_path", "filePath", "path"];

/// The same, for a result the edit tool wrote: its `EditResult` publishes the
/// path under `file`, which is the spelling read first.
const EDITED_FILE_PATH_KEYS: &[&str] = &["file", "file_path", "filePath", "path"];

fn project_input(kind: ToolEffectKind, arguments: &Value) -> Value {
    match kind {
        ToolEffectKind::Tool => match arguments {
            Value::Object(_) => arguments.clone(),
            // A generic input is not nullable, so a call with no object
            // arguments publishes an empty one rather than a null.
            Value::Null => json!({}),
            value => json!({"value": value}),
        },
        ToolEffectKind::Shell => json!({
            "command": string_argument(arguments, &["command", "cmd"]),
        }),
        ToolEffectKind::FileEdit => json!({
            "filePath": string_argument(arguments, FILE_PATH_KEYS),
            "oldString": string_argument(arguments, &["old_string", "oldString"]),
            "newString": string_argument(arguments, &["new_string", "newString"]),
            "replaceAll": bool_argument(arguments, &["replace_all", "replaceAll"]),
        }),
        ToolEffectKind::FileSearch => json!({
            "pattern": string_argument(arguments, &["pattern", "query"]),
            "path": optional_string_argument(arguments, &["path"]).unwrap_or_else(|| ".".to_owned()),
            "maxMatches": number_argument(arguments, &["max_matches", "maxMatches"]),
        }),
        ToolEffectKind::FileRead => json!({
            "filePath": string_argument(arguments, FILE_PATH_KEYS),
            "offset": number_argument(arguments, &["offset", "startLine", "start_line"]),
            "limit": number_argument(arguments, &["limit", "maxLines", "max_lines"])
                .unwrap_or(DEFAULT_READ_LIMIT),
        }),
        ToolEffectKind::Todo => json!({
            "action": string_argument(arguments, &["action"]),
            "todos": arguments
                .get("todos")
                .and_then(Value::as_array)
                .map(|todos| todos.iter().map(TodoEffectItem::from_argument).collect::<Vec<_>>()),
        }),
        ToolEffectKind::FileWrite => json!({
            "filePath": string_argument(arguments, FILE_PATH_KEYS),
            "content": string_argument(arguments, &["content", "text"]),
        }),
        ToolEffectKind::UserQuestion => json!({
            "questions": arguments
                .get("questions")
                .filter(|questions| questions.is_array())
                .cloned()
                .unwrap_or_else(|| json!([])),
            "footerNote": optional_string_argument(arguments, &["footer_note", "footerNote"]),
        }),
        ToolEffectKind::WebSearch => json!({
            "query": string_argument(arguments, &["query"]),
        }),
        ToolEffectKind::WebFetch => json!({
            "url": string_argument(arguments, &["url"]),
            "timeout": number_argument(arguments, &["timeout", "timeoutSeconds"]),
        }),
        ToolEffectKind::Skill => json!({
            "name": string_argument(arguments, &["name"]),
        }),
        ToolEffectKind::Subagent => json!({
            "task": string_argument(arguments, &["task", "prompt"]),
            "agent": string_argument(arguments, &["agent", "subagent_type"]),
        }),
    }
}

/// The reference default read window, published when a call did not name one.
const DEFAULT_READ_LIMIT: u64 = 2000;

/// The label a settled header carries when nothing named why a call was
/// skipped.
const SKIPPED: &str = "Skipped";

/// The label a proxied call settles under when its payload is not a result the
/// remote model accepts.
const NO_RESULT: &str = "No result";

/// The header a client shows while the call runs.
///
/// A proxied call takes none of the per-kind branches: the reference presents
/// it through the base class, whose display is the published name alone.
fn call_display(
    kind: ToolEffectKind,
    tool_name: &str,
    arguments: &Value,
    remote: Option<&RemoteToolOrigin>,
    wire_order: &[String],
) -> EffectCallDisplay {
    if let Some(remote) = remote {
        let mut display = EffectCallDisplay {
            summary: tool_name.to_owned(),
            status_text: remote.status_text(tool_name),
            ..EffectCallDisplay::default()
        };
        display.fill_defaults();
        return display;
    }
    let (verb, settled_verb, summary, message) = if kind == ToolEffectKind::Todo {
        todo_call_display(arguments)
    } else {
        let (verb, settled_verb) = kind.verbs();
        let (summary, message) = call_summary(kind, tool_name, arguments, wire_order);
        (verb.to_owned(), settled_verb.to_owned(), summary, message)
    };
    // A call whose arguments never arrived would otherwise render a bare verb.
    let message = if message.is_empty() {
        summary.clone()
    } else {
        message
    };
    let mut display = EffectCallDisplay {
        summary,
        content: None,
        suffix: String::new(),
        verb,
        // A kind that names its own subject names it in both headers, which is
        // what keeps a settled call from renaming what it was acting on. Only a
        // presentation that names none falls back to the summary below.
        message: Some(message.clone()),
        settled_verb,
        settled_message: Some(message),
        status_text: kind.status_text().to_owned(),
    };
    display.fill_defaults();
    display
}

fn call_summary(
    kind: ToolEffectKind,
    tool_name: &str,
    arguments: &Value,
    wire_order: &[String],
) -> (String, String) {
    match kind {
        ToolEffectKind::Shell => {
            let command = string_argument(arguments, &["command", "cmd"]);
            (format!("bash: {command}"), command)
        }
        ToolEffectKind::FileRead => {
            let mut message = string_argument(arguments, FILE_PATH_KEYS);
            let mut extras = Vec::new();
            if let Some(offset) = number_argument(arguments, &["offset", "startLine", "start_line"])
                && offset > 0
            {
                extras.push(format!("from line {offset}"));
            }
            if let Some(limit) = number_argument(arguments, &["limit", "maxLines", "max_lines"]) {
                extras.push(format!("limit {limit} lines"));
            }
            if !extras.is_empty() {
                message = format!("{message} ({})", extras.join(", "));
            }
            (format!("Reading {message}"), message)
        }
        ToolEffectKind::FileWrite => {
            let path = string_argument(arguments, FILE_PATH_KEYS);
            (format!("Writing {path}"), path)
        }
        ToolEffectKind::FileEdit => {
            let name = file_name(&string_argument(arguments, FILE_PATH_KEYS));
            (format!("Editing {name}"), name)
        }
        ToolEffectKind::FileSearch => {
            let pattern = string_argument(arguments, &["pattern", "query"]);
            let mut message = format!("'{pattern}'");
            let path = string_argument(arguments, &["path"]);
            if !path.is_empty() && path != "." {
                message = format!("{message} in {path}");
            }
            if let Some(max) = number_argument(arguments, &["max_matches", "maxMatches"]) {
                message = format!("{message} (max {max} matches)");
            }
            (format!("Grepping {message}"), message)
        }
        ToolEffectKind::UserQuestion => {
            let questions = arguments
                .get("questions")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            match questions {
                [only] => {
                    let message = only
                        .get("question")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    (format!("Asking: {message}"), message)
                }
                many => {
                    let message = format!("{} questions", many.len());
                    (format!("Asking {message}"), message)
                }
            }
        }
        ToolEffectKind::WebSearch => {
            let query = string_argument(arguments, &["query"]);
            let message = format!("the web: '{query}'");
            (format!("Searching {message}"), message)
        }
        ToolEffectKind::WebFetch => {
            let message = host_of(&string_argument(arguments, &["url"]));
            (format!("Fetching: {message}"), message)
        }
        ToolEffectKind::Skill => {
            let message = format!("skill: {}", string_argument(arguments, &["name"]));
            (format!("Loading {message}"), message)
        }
        ToolEffectKind::Subagent => {
            let message = format!(
                "{} agent: {}",
                string_argument(arguments, &["agent", "subagent_type"]),
                string_argument(arguments, &["task", "prompt"])
            );
            (format!("Running {message}"), message)
        }
        // A tool with no presentation of its own shows its arguments, and the
        // summary is all there is to show.
        ToolEffectKind::Tool | ToolEffectKind::Todo => {
            let summary = generic_call_summary(tool_name, arguments, wire_order);
            (summary.clone(), summary)
        }
    }
}

/// The todo effect names its own verbs: the same kind reads, writes, or fails
/// to recognize its action.
fn todo_call_display(arguments: &Value) -> (String, String, String, String) {
    match string_argument(arguments, &["action"]).as_str() {
        "read" => (
            "Retrieving".to_owned(),
            "Retrieved".to_owned(),
            "Reading todos".to_owned(),
            "todos".to_owned(),
        ),
        "write" => {
            let count = arguments
                .get("todos")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            (
                "Updating".to_owned(),
                "Updated".to_owned(),
                format!("Writing {count} todos"),
                format!("{count} todos"),
            )
        }
        action => (
            "Running".to_owned(),
            "Ran".to_owned(),
            format!("Unknown action: {action}"),
            format!("unknown todo action: {action}"),
        ),
    }
}

/// The fallback for tools without their own presentation: the first three
/// arguments, in the order the call carried them.
fn generic_call_summary(tool_name: &str, arguments: &Value, wire_order: &[String]) -> String {
    let object = arguments.as_object();
    let ordered = if wire_order.is_empty() {
        object
            .into_iter()
            .flatten()
            .map(|(key, value)| (key.as_str(), value))
            .collect::<Vec<_>>()
    } else {
        wire_order
            .iter()
            .filter_map(|key| {
                object.and_then(|fields| fields.get(key).map(|value| (key.as_str(), value)))
            })
            .collect()
    };
    let rendered = ordered
        .into_iter()
        .take(3)
        .map(|(key, value)| format!("{key}={}", python_repr(value)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{tool_name}({rendered})")
}

/// One argument value, spelled the way the reference interpreter spells it.
///
/// The generic summary interpolates its arguments, so a value crossing it
/// renders under Python's literal syntax rather than JSON's: the two disagree
/// on booleans, on the null, and on the quote a key or a string carries.
fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::String(text) => python_string(text),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, value)| format!("{}: {}", python_string(key), python_repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        number => number.to_string(),
    }
}

/// A string literal under Python's quoting rule: single quotes, unless the text
/// carries one and no double quote, in which case the pair switches rather than
/// escaping.
fn python_string(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\");
    if escaped.contains('\'') && !escaped.contains('"') {
        format!("\"{escaped}\"")
    } else {
        format!("'{}'", escaped.replace('\'', "\\'"))
    }
}

impl EffectResultDisplay {
    /// The header of a call that failed. The subject is unchanged, so a failed
    /// call still names what it was acting on.
    #[must_use]
    pub fn failed(call: &EffectCallDisplay) -> Self {
        Self {
            success: false,
            verb: call.settled_verb.clone(),
            message: call.subject().to_owned(),
            warnings: Vec::new(),
            suffix: String::new(),
        }
    }

    /// The header of a call that never ran to completion.
    #[must_use]
    pub fn skipped(tool_name: &str) -> Self {
        Self {
            success: false,
            verb: String::new(),
            message: format!("{tool_name}: skipped"),
            warnings: Vec::new(),
            suffix: String::new(),
        }
    }

    /// The header a proxied call settles under.
    ///
    /// The reference reads the three branches in this order and stops at the
    /// first that answers: an errored call reports what went wrong, a skipped
    /// call reports why it never ran, and only then does the proxy read the
    /// result its server sent. Ordering them anywhere but here would let a
    /// remote failure render as a success.
    #[must_use]
    pub fn for_remote(
        remote: &RemoteToolOrigin,
        call: &EffectCallDisplay,
        settled: &RemoteSettlement<'_>,
    ) -> Self {
        if !settled.error.is_empty() {
            return Self {
                success: false,
                verb: call.settled_verb.clone(),
                message: settled.error.to_owned(),
                warnings: Vec::new(),
                suffix: String::new(),
            };
        }
        if settled.skipped {
            return Self {
                success: false,
                verb: String::new(),
                message: if settled.skip_reason.is_empty() {
                    SKIPPED.to_owned()
                } else {
                    settled.skip_reason.to_owned()
                },
                warnings: Vec::new(),
                suffix: String::new(),
            };
        }
        // A payload the remote result model does not accept settles as a
        // failure: the reference's proxies check the result's class before
        // reading it and report having no result rather than presenting a shape
        // they cannot read.
        let Ok(result) = serde_json::from_value::<RemoteToolResult>(settled.output.clone()) else {
            return Self {
                success: false,
                verb: String::new(),
                message: NO_RESULT.to_owned(),
                warnings: Vec::new(),
                suffix: String::new(),
            };
        };
        // Only the proxy's own result answers for the call. The server's own
        // structured content says nothing about whether the call succeeded, so
        // a payload that is not the proxy's settles from what this port knows:
        // the call answered, under the name the origin carries.
        let (ok, answered) = if result.is_the_proxys_own() {
            let answered = result
                .tool
                .filter(|tool| !tool.is_empty())
                .unwrap_or_else(|| remote.remote_name.clone());
            (result.ok, answered)
        } else {
            (true, remote.remote_name.clone())
        };
        Self {
            success: ok,
            verb: "Ran".to_owned(),
            message: remote.settled_subject(&answered),
            warnings: Vec::new(),
            suffix: String::new(),
        }
    }

    /// The header of a cancelled call, or nothing when it produced no result.
    ///
    /// A cancellation is the one settled state the reference lets carry a null
    /// display, because a call cancelled before it produced anything has no
    /// result to present.
    #[must_use]
    pub fn cancelled(tool_name: &str, output: &Value, emitted: &Value) -> Option<Self> {
        if let Ok(display) = serde_json::from_value::<Self>(emitted.clone()) {
            return Some(display);
        }
        (!output.is_null()).then(|| Self::skipped(tool_name))
    }

    /// The header of a completed call, read from its typed output.
    ///
    /// A tool that emitted its own result display keeps it: the reference
    /// widgets consume `state.display` without re-deriving it.
    #[must_use]
    pub fn completed(
        kind: ToolEffectKind,
        call: &EffectCallDisplay,
        output: &Value,
        emitted: &Value,
    ) -> Self {
        if let Ok(display) = serde_json::from_value::<Self>(emitted.clone()) {
            return display;
        }
        let warnings = output
            .get("warnings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let Header {
            verb,
            message,
            suffix,
        } = completed_header(kind, call, output);
        if kind == ToolEffectKind::UserQuestion
            && output
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            return Self {
                success: false,
                verb: "Cancelled".to_owned(),
                message: "by user".to_owned(),
                warnings,
                suffix: String::new(),
            };
        }
        Self {
            success: true,
            verb,
            message,
            warnings,
            suffix,
        }
    }
}

/// The three strings a settled effect's header is built from, named so a caller
/// cannot transpose them.
struct Header {
    /// What the tool did, in the past tense a client renders.
    verb: String,
    /// What it did it to.
    message: String,
    /// The parenthetical a truncated or capped result carries, empty otherwise.
    suffix: String,
}

impl Header {
    fn new(verb: impl Into<String>, message: impl Into<String>, suffix: impl Into<String>) -> Self {
        Self {
            verb: verb.into(),
            message: message.into(),
            suffix: suffix.into(),
        }
    }
}

/// The header a settled call publishes, per effect kind.
///
/// Reference builds each one from the tool's own typed result, so the shape a
/// client renders is one table rather than a widget per tool.
fn completed_header(kind: ToolEffectKind, call: &EffectCallDisplay, output: &Value) -> Header {
    /// The parenthetical a capped or clipped result carries.
    const TRUNCATED: &str = "(truncated)";
    let truncated = |clipped: bool| if clipped { TRUNCATED } else { "" };

    match kind {
        ToolEffectKind::Shell => {
            let command = string_argument(output, &["command"]);
            let subject = if command.is_empty() {
                call.subject().to_owned()
            } else {
                command
            };
            Header::new("Ran", subject, "")
        }
        ToolEffectKind::FileRead => {
            let lines = read_line_count(output);
            let word = if lines == 1 { "line" } else { "lines" };
            let name = file_name(&string_argument(output, FILE_PATH_KEYS));
            Header::new(
                "Read",
                format!("{lines} {word} from {name}"),
                truncated(read_was_truncated(output, lines)),
            )
        }
        ToolEffectKind::FileWrite => Header::new(
            "Created",
            file_name(&string_argument(output, FILE_PATH_KEYS)),
            "",
        ),
        ToolEffectKind::FileEdit => Header::new(
            "Edited",
            file_name(&string_argument(output, EDITED_FILE_PATH_KEYS)),
            "",
        ),
        ToolEffectKind::FileSearch => {
            let count = search_match_count(output);
            let word = if count == 1 { "match" } else { "matches" };
            let pattern = string_argument(output, &["pattern"]);
            let subject = if pattern.is_empty() {
                format!("({count} {word})")
            } else {
                format!("{pattern} ({count} {word})")
            };
            Header::new("Searched", subject, truncated(truncated_flag(output)))
        }
        ToolEffectKind::Todo => {
            let verb = string_argument(output, &["verb"]);
            let count = output
                .get("total_count")
                .or_else(|| output.get("totalCount"))
                .and_then(Value::as_u64)
                .unwrap_or_else(|| {
                    output
                        .get("todos")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len) as u64
                });
            let verb = if verb.is_empty() {
                "Updated".to_owned()
            } else {
                verb
            };
            Header::new(verb, format!("{count} todos"), "")
        }
        ToolEffectKind::UserQuestion => Header::new("Answered", answered_message(output), ""),
        ToolEffectKind::WebSearch => {
            let sources = output
                .get("sources")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let plural = if sources == 1 { "" } else { "s" };
            Header::new(
                "Searched",
                format!(
                    "'{}' ({sources} source{plural})",
                    string_argument(output, &["query"])
                ),
                "",
            )
        }
        ToolEffectKind::WebFetch => {
            let content_length = output
                .get("content")
                .and_then(Value::as_str)
                .map_or(0, str::len);
            let content_type = string_argument(output, &["content_type", "contentType"]);
            Header::new(
                "Fetched",
                format!(
                    "{} ({} chars, {})",
                    string_argument(output, &["url"]),
                    grouped_thousands(content_length),
                    content_type.split(';').next().unwrap_or_default()
                ),
                truncated(truncated_flag(output)),
            )
        }
        ToolEffectKind::Skill => Header::new("Loaded", call.subject(), ""),
        ToolEffectKind::Subagent => Header::new("Completed", call.subject(), ""),
        ToolEffectKind::Tool => Header::new("Ran", call.subject(), ""),
    }
}

// --------------------------------------------------------------------------
// Notice detail
// --------------------------------------------------------------------------

/// What a hook notice reports about one hook or one hook run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookNotice {
    #[serde(default)]
    pub scope: HookScope,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub hook_name: Option<String>,
    #[serde(default)]
    pub status: Option<HookSeverity>,
    #[serde(default)]
    pub content: Option<String>,
}

/// Why a notice entry was written.
///
/// A condition with no variant here is not published as a notice: the port used
/// to invent a `kind`, which a reference client rejects outright.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum NoticeDetail {
    HookRunStarted(HookNotice),
    HookRunCompleted(HookNotice),
    HookStarted(HookNotice),
    HookCompleted(HookNotice),
    AgentChanged {
        agent_name: String,
    },
    ContextCleared {
        #[serde(default)]
        plan_file_path: Option<String>,
    },
    SessionTitleUpdated {
        title: String,
    },
    PlanReviewStarted {
        file_path: String,
    },
    PlanReviewEnded,
    WaitingForInput {
        task_id: String,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        predefined_answers: Option<Vec<String>>,
    },
    ScheduledLoopFired {
        loop_id: String,
    },
}

// --------------------------------------------------------------------------
// Callback detail and output
// --------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionChoice {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserQuestion {
    pub question: String,
    #[serde(default)]
    pub header: String,
    pub options: Vec<QuestionChoice>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default)]
    pub hide_other: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserQuestionRequest {
    pub questions: Vec<UserQuestion>,
    #[serde(default)]
    pub footer_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserAnswer {
    pub question: String,
    pub answer: String,
    #[serde(default)]
    pub is_other: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserQuestionResult {
    pub answers: Vec<UserAnswer>,
    #[serde(default)]
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDecision {
    #[serde(rename = "type")]
    pub decision: ApprovalDecisionType,
}

/// What a callback is asking the client for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum CallbackDetail {
    Approval {
        effect: Box<EffectDetail>,
        /// Reference `ApprovalCallbackDetail.required_permissions`: the
        /// requirement models themselves, because a client renders what an
        /// "allow always" would grant from the scope and the two patterns.
        #[serde(default)]
        required_permissions: Vec<PermissionRequirement>,
        #[serde(default)]
        choices: Vec<ApprovalDecisionType>,
        #[serde(default)]
        related_entry_id: Option<String>,
    },
    UserInput {
        request: UserQuestionRequest,
        #[serde(default)]
        related_entry_id: Option<String>,
    },
}

impl CallbackDetail {
    /// The entry this callback relates to, when the caller named one.
    #[must_use]
    pub fn related_entry_id(&self) -> Option<&str> {
        match self {
            Self::Approval {
                related_entry_id, ..
            }
            | Self::UserInput {
                related_entry_id, ..
            } => related_entry_id.as_deref(),
        }
    }
}

/// What a client answered a callback with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum CallbackOutput {
    Approval {
        decision: ApprovalDecision,
        #[serde(default)]
        feedback: Option<String>,
    },
    UserInput {
        result: UserQuestionResult,
    },
}

impl CallbackOutput {
    /// Whether the operator asked for the whole turn to stop.
    #[must_use]
    pub fn requests_turn_cancel(&self) -> bool {
        matches!(
            self,
            Self::Approval {
                decision: ApprovalDecision {
                    decision: ApprovalDecisionType::CancelTurn,
                },
                ..
            }
        )
    }
}

// --------------------------------------------------------------------------
// Argument and output readers
// --------------------------------------------------------------------------

fn string_argument(arguments: &Value, keys: &[&str]) -> String {
    optional_string_argument(arguments, keys).unwrap_or_default()
}

fn optional_string_argument(arguments: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
}

fn number_argument(arguments: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_u64))
}

/// The vocabulary member `key` names, or the member the reference defaults to.
fn enum_argument<T: Default + serde::de::DeserializeOwned>(arguments: &Value, key: &str) -> T {
    arguments
        .get(key)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default()
}

fn bool_argument(arguments: &Value, keys: &[&str]) -> bool {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_bool))
        .unwrap_or(false)
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

fn host_of(url: &str) -> String {
    let without_scheme = url
        .split_once("://")
        .map_or(url, |(_, remainder)| remainder);
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    if host.is_empty() {
        url.chars().take(50).collect()
    } else {
        host.to_owned()
    }
}

fn read_line_count(output: &Value) -> u64 {
    if let Some(count) = output
        .get("num_lines")
        .or_else(|| output.get("numLines"))
        .and_then(Value::as_u64)
    {
        return count;
    }
    let start = output
        .get("startLine")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let end = output
        .get("endLine")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if end >= start && start > 0 {
        return end.saturating_sub(start).saturating_add(1);
    }
    output
        .get("content")
        .and_then(Value::as_str)
        .map_or(0, |content| content.lines().count() as u64)
}

fn truncated_flag(output: &Value) -> bool {
    output
        .get("was_truncated")
        .or_else(|| output.get("wasTruncated"))
        .or_else(|| output.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The read suffix: the flag, or a window that stops short of the file.
fn read_was_truncated(output: &Value, lines: u64) -> bool {
    if truncated_flag(output) {
        return true;
    }
    let Some(total) = output
        .get("total_lines")
        .or_else(|| output.get("totalLines"))
        .and_then(Value::as_u64)
    else {
        return false;
    };
    let start = output
        .get("start_line")
        .or_else(|| output.get("startLine"))
        .and_then(Value::as_u64)
        .unwrap_or(1);
    start.saturating_add(lines).saturating_sub(1) < total
}

fn search_match_count(output: &Value) -> usize {
    if let Some(count) = output
        .get("match_count")
        .or_else(|| output.get("matchCount"))
        .and_then(Value::as_u64)
    {
        return usize::try_from(count).unwrap_or(usize::MAX);
    }
    match output {
        Value::Array(matches) => matches.len(),
        value => value
            .get("matches")
            .and_then(Value::as_str)
            .map_or(0, |matches| matches.lines().count()),
    }
}

fn answered_message(output: &Value) -> String {
    output
        .get("answers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|answer| {
            let question = answer
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let other = answer
                .get("is_other")
                .or_else(|| answer.get("isOther"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let text = answer
                .get("answer")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!(
                "\"{question}\" → {}{text}",
                if other { "(Other) " } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The thousands-grouped form a byte count renders as.
fn grouped_thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len().saturating_add(digits.len() / 3));
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(test)]
mod detail_presentation_parity_tests;
#[cfg(test)]
mod detail_tests;
