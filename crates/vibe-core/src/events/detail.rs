//! Typed details for the entries the projection publishes.
//!
//! A history entry used to carry an untyped `detail` object, which meant a
//! client had to guess what an effect was and could not render a notice written
//! by the other implementation. The reference types all three: an effect detail
//! is a 14-variant union keyed on the tool's semantic kind, a notice detail an
//! 8-variant union keyed on what happened, and a callback detail names whether
//! it wants an approval or an answer.
//!
//! Each effect detail carries its presentation. The call display is computed
//! once, here, where the projection builds the entry, so the terminal client,
//! the editor bridge and a reference client all render the same header instead
//! of each deriving one from the raw arguments.
//!
//! The vocabularies and the [`EffectDetail`] an effect carries live here; what
//! its call header says is `call`, how it settles is `result`, what a proxied
//! tool adds is `remote`, the notice and callback details are `notice` and
//! `callback`, and the argument and output readers they all share are
//! `readers`.

use std::fmt;
use std::path::Path;

use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

mod call;
mod callback;
mod notice;
mod output;
mod readers;
mod remote;
mod result;

use call::{call_display, generic_call_summary, project_input};
pub use callback::{
    ApprovalDecision, CallbackDetail, CallbackOutput, QuestionChoice, UserAnswer, UserQuestion,
    UserQuestionRequest, UserQuestionResult,
};
pub use notice::{HookNotice, NoticeDetail};
pub use output::{has_output_model, project_output, shell_transcript};
pub use readers::tagged_text;
use readers::{enum_argument, string_argument};
pub use remote::{RemoteSettlement, RemoteToolKind, RemoteToolOrigin};
pub use result::persisted_effect_state;

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
    /// A session opened in a git worktree, which the reference publishes as a
    /// transcript entry of its own (`vibe/app_server/_worktree_effects.py`)
    /// rather than as a tool call.
    Worktree,
    /// A background process the Unified harness manages
    /// (`vibe/app_server/_unified_tool_projection.py`).
    Process,
    /// A call to the Unified harness's scratchpad tool, which v2.26.0 projects
    /// onto a kind of its own (`vibe/app_server/_unified_tool_projection.py`).
    Scratchpad,
}

impl ToolEffectKind {
    /// Every kind, in the order the reference vocabulary declares them.
    pub const ALL: [Self; 15] = [
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
        Self::Worktree,
        Self::Process,
        Self::Scratchpad,
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
            Self::Worktree => "Creating worktree",
            Self::Process => "Running process",
            Self::Scratchpad => "Using the scratchpad",
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
            Self::Worktree => "worktree",
            Self::Process => "process",
            Self::Scratchpad => "scratchpad",
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
            Self::Worktree => ("Creating", "Created"),
            Self::Tool | Self::Todo | Self::Process | Self::Scratchpad => ("Running", "Ran"),
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
    /// Why the call ran without asking. Reference `EffectResultDisplay`
    /// declares it since v2.25.4 and serializes it without dropping a null, so
    /// the key is on every result display; only the smart-approve path of the
    /// Unified backend ever fills it, which this port does not run.
    #[serde(default)]
    pub approval_note: Option<String>,
    #[serde(default)]
    pub suffix: String,
}

// --------------------------------------------------------------------------
// Effect detail
// --------------------------------------------------------------------------

/// A tool call as a client renders it.
///
/// One struct rather than twelve: the variants differ only by `kind`, by the
/// shape `input` takes and by the subagent's child session, so the union is
/// discriminated on the wire without twelve near-identical Rust declarations.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectDetail {
    // Declared in the reference's order (`_EffectDetailBase` then each kind's
    // own fields), which is the order a serialized entry names them in.
    pub tool_name: String,
    pub display: EffectCallDisplay,
    pub kind: ToolEffectKind,
    /// The call arguments in the shape this kind declares. Never null: a
    /// generic effect publishes an object, which is what its variant requires.
    #[serde(default)]
    pub input: Value,
    /// Only the subagent variant declares it, so it stays off every other kind's
    /// wire form rather than being published as a surplus null, and a
    /// subagent's publishes null until its child session opens.
    #[serde(default)]
    pub child_session_id: Option<String>,
    /// The remote this call is proxied to, absent for a tool the session
    /// implements itself. It routes the call header, the status text and the
    /// settled header, which is why it travels with the detail instead of being
    /// re-derived from the published name. It never reaches the wire: the
    /// reference's effect detail declares no such field, and the display it
    /// routes is published in full.
    #[serde(skip)]
    pub remote: Option<RemoteToolOrigin>,
}

/// A presentation as the reference persists it: `exclude_none`, so a field
/// left empty is not written.
fn without_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, item)| !item.is_null())
                .collect(),
        ),
        other => other,
    }
}

impl EffectDetail {
    /// Reference `ToolCallPresentation`, which a persisted call records under
    /// `presentation`.
    #[must_use]
    pub fn presentation(&self) -> Value {
        json!({
            "kind": self.kind,
            "display": without_nulls(serde_json::to_value(&self.display).unwrap_or(Value::Null)),
        })
    }

    /// The header a call that ran to an answer settles with: the one the
    /// projection publishes for a completed call.
    #[must_use]
    pub fn completed_display(
        &self,
        answered: &Value,
        emitted: &Value,
        working_directory: Option<&Path>,
    ) -> EffectResultDisplay {
        match &self.remote {
            Some(remote) => EffectResultDisplay::for_remote(
                remote,
                &self.display,
                &RemoteSettlement::answered(answered),
            ),
            None => EffectResultDisplay::completed_at(
                self.kind,
                &self.display,
                answered,
                emitted,
                working_directory,
            ),
        }
    }
}

/// Reference `ToolResultPresentation`, which a persisted answer records under
/// `tool_result.presentation`.
#[must_use]
pub fn result_presentation(
    kind: ToolEffectKind,
    display: &EffectResultDisplay,
    projected: &Value,
) -> Value {
    let mut presentation = json!({
        "kind": kind,
        "display": without_nulls(serde_json::to_value(display).unwrap_or(Value::Null)),
    });
    if !projected.is_null()
        && let Some(object) = presentation.as_object_mut()
    {
        object.insert("projectedOutput".to_owned(), projected.clone());
    }
    presentation
}

impl Serialize for EffectDetail {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("toolName", &self.tool_name)?;
        map.serialize_entry("display", &self.display)?;
        map.serialize_entry("kind", &self.kind)?;
        map.serialize_entry("input", &self.input)?;
        if self.kind == ToolEffectKind::Subagent || self.child_session_id.is_some() {
            map.serialize_entry("childSessionId", &self.child_session_id)?;
        }
        // Reference `SubagentEffectDetail.agent_name`, the name a spawn gave
        // its child, which only the Unified Harness fills in.
        if self.kind == ToolEffectKind::Subagent {
            map.serialize_entry("agentName", &None::<String>)?;
        }
        map.end()
    }
}

/// The tool that hands a finished plan to the user for a verdict.
const EXIT_PLAN_MODE: &str = "exit_plan_mode";

/// What a client shows while the plan waits on the user's verdict. Reference
/// `ExitPlanMode.get_status_text`, in this port's own words (`NOTICE`).
const PLAN_REVIEW_STATUS: &str = "Awaiting the user's verdict";

/// Reference `ExitPlanMode.format_call_display`: the call reads as a request
/// to leave planning rather than as a tool running, in this port's own words.
fn plan_review_header(display: &mut EffectCallDisplay) {
    const REQUEST: &str = "leave plan mode";
    "The plan is ready for review".clone_into(&mut display.summary);
    "Requesting".clone_into(&mut display.verb);
    display.message = Some(REQUEST.to_owned());
    "Requested".clone_into(&mut display.settled_verb);
    display.settled_message = Some(REQUEST.to_owned());
    PLAN_REVIEW_STATUS.clone_into(&mut display.status_text);
}

impl EffectDetail {
    /// The detail for a call to `tool_name` with `arguments`, with every file
    /// path it names displayed against the process working directory.
    #[must_use]
    pub fn for_call(tool_name: &str, arguments: &Value) -> Self {
        Self::for_call_at(tool_name, arguments, None)
    }

    /// The detail for a call made by a session sitting in `working_directory`.
    ///
    /// Reference `ToolUIDataAdapter` renders a file path relative to the
    /// session's own directory, which it binds through `file_display_harness`
    /// (`vibe/core/tools/ui.py:119-121`), and falls back to the process
    /// directory when none is bound. [`None`] is that fallback.
    #[must_use]
    pub fn for_call_at(
        tool_name: &str,
        arguments: &Value,
        working_directory: Option<&Path>,
    ) -> Self {
        // A caller holding a decoded object holds no order to keep: the map it
        // read them from iterates lexicographically already.
        Self::for_decoded_call(tool_name, arguments, &[], None, working_directory)
            .reviewing_plan(arguments.is_object())
    }

    /// The detail an approval presents for a call, which a session proxying
    /// the tool to `remote` presents as the proxied call its effect will be.
    #[must_use]
    pub fn for_gated_call(
        tool_name: &str,
        arguments: &Value,
        remote: Option<&RemoteToolOrigin>,
        working_directory: Option<&Path>,
    ) -> Self {
        match remote {
            Some(remote) => {
                Self::for_decoded_call(tool_name, arguments, &[], Some(remote), working_directory)
            }
            None => Self::for_call_at(tool_name, arguments, working_directory),
        }
    }

    /// The shared constructor, told which order the arguments arrived in,
    /// which remote the call is proxied to and where the session sits.
    fn for_decoded_call(
        tool_name: &str,
        arguments: &Value,
        wire_order: &[String],
        remote: Option<&RemoteToolOrigin>,
        working_directory: Option<&Path>,
    ) -> Self {
        // A proxy takes none of the per-kind presentations whatever its
        // published name reads like, which the generic kind is what expresses.
        let kind = if remote.is_some() {
            ToolEffectKind::Tool
        } else {
            ToolEffectKind::from_tool_name(tool_name)
        };
        let display = call_display(
            kind,
            tool_name,
            arguments,
            remote,
            wire_order,
            working_directory,
        );
        Self {
            kind,
            tool_name: tool_name.to_owned(),
            display,
            input: project_input(kind, arguments),
            child_session_id: None,
            remote: remote.cloned(),
        }
    }

    /// The detail of a call announced before its arguments were read.
    /// Reference `get_no_args_display`: the header is the tool's name, and the
    /// input is absent.
    #[must_use]
    pub fn announced(tool_name: &str, remote: Option<&RemoteToolOrigin>) -> Self {
        let kind = if remote.is_some() {
            ToolEffectKind::Tool
        } else {
            ToolEffectKind::from_tool_name(tool_name)
        };
        let mut display = EffectCallDisplay {
            summary: tool_name.to_owned(),
            status_text: remote.map_or_else(
                || kind.status_text().to_owned(),
                |remote| remote.status_text(tool_name),
            ),
            ..EffectCallDisplay::default()
        };
        // Reference `Task.get_call_display` presents a call it cannot read yet
        // as the subagent it will start rather than by the tool's name.
        if kind == ToolEffectKind::Subagent && remote.is_none() {
            display.summary = kind.status_text().to_owned();
            display.verb = "Running".to_owned();
            display.message = Some(kind.label().to_owned());
            display.settled_verb = "Ran".to_owned();
            display.settled_message = Some(kind.label().to_owned());
        }
        if remote.is_none() && tool_name == EXIT_PLAN_MODE {
            PLAN_REVIEW_STATUS.clone_into(&mut display.status_text);
        }
        display.fill_defaults();
        Self {
            kind,
            tool_name: tool_name.to_owned(),
            display,
            input: Value::Null,
            child_session_id: None,
            remote: remote.cloned(),
        }
    }

    /// The detail of a call that settled without ever resolving to a tool.
    /// Reference `_result_only_effect` (`vibe/app_server/_projector.py`): a
    /// generic effect named after the call, with no input.
    #[must_use]
    pub fn unresolved(tool_name: &str) -> Self {
        Self {
            kind: ToolEffectKind::Tool,
            tool_name: tool_name.to_owned(),
            display: EffectCallDisplay {
                summary: tool_name.to_owned(),
                verb: "Running".to_owned(),
                message: Some(tool_name.to_owned()),
                settled_verb: "Ran".to_owned(),
                settled_message: Some(tool_name.to_owned()),
                status_text: format!("Running {tool_name}"),
                ..EffectCallDisplay::default()
            },
            input: Value::Null,
            child_session_id: None,
            remote: None,
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
        Self::for_decoded_call(
            tool_name,
            &decode_arguments(arguments),
            &[],
            Some(remote),
            None,
        )
    }

    /// The detail for a call whose arguments were recorded as a JSON string,
    /// which is how the engine carries them.
    #[must_use]
    pub fn for_encoded_call(tool_name: &str, arguments: &str) -> Self {
        Self::for_encoded_call_at(tool_name, arguments, None)
    }

    /// The same, for a session sitting in `working_directory`, as
    /// [`Self::for_call_at`] reads it.
    #[must_use]
    pub fn for_encoded_call_at(
        tool_name: &str,
        arguments: &str,
        working_directory: Option<&Path>,
    ) -> Self {
        Self::for_decoded_call(
            tool_name,
            &decode_arguments(arguments),
            &wire_key_order(arguments),
            None,
            working_directory,
        )
        .reviewing_plan(
            serde_json::from_str::<Value>(arguments).is_ok_and(|value| value.is_object()),
        )
    }

    /// Reference `ExitPlanModeArgs` accepts any object; arguments that are
    /// absent or not an object fall back to the generic header.
    fn reviewing_plan(mut self, valid_arguments: bool) -> Self {
        if valid_arguments && self.remote.is_none() && self.tool_name == EXIT_PLAN_MODE {
            plan_review_header(&mut self.display);
        }
        self
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
/// the caller wrote them. Reading the keys back off the text states that order
/// here rather than leaning on how [`serde_json::Map`] is built. Arguments that
/// never decoded name no order, and the summary falls back to the map's own.
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

impl EffectDetail {
    /// Reference `_history_effect` without a recorded presentation: a generic
    /// effect whose header names the tool and its first three arguments, and
    /// whose input is the arguments as recorded (the raw text when it is not
    /// JSON).
    #[must_use]
    pub fn generic(tool_name: &str, arguments: Option<&str>) -> Self {
        let input = arguments.map_or(Value::Null, |raw| {
            serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
        });
        let summary = if input.is_object() {
            generic_call_summary(
                tool_name,
                &input,
                &arguments.map(wire_key_order).unwrap_or_default(),
            )
        } else {
            tool_name.to_owned()
        };
        Self {
            kind: ToolEffectKind::Tool,
            tool_name: tool_name.to_owned(),
            display: EffectCallDisplay {
                summary: summary.clone(),
                verb: "Running".to_owned(),
                message: Some(summary.clone()),
                settled_verb: "Ran".to_owned(),
                settled_message: Some(summary),
                status_text: format!("Running {tool_name}"),
                ..EffectCallDisplay::default()
            },
            input,
            child_session_id: None,
            remote: None,
        }
    }
}

#[cfg(test)]
mod detail_presentation_parity_tests;
#[cfg(test)]
mod detail_tests;
