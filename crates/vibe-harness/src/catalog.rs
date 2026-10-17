//! The tools a unified session declares to the model, and which harness tool
//! each one runs.
//!
//! The names, the order and the parameter schemas are the reference's
//! (`harness/core/src/core/features/` and the provided tools of
//! `vibe/app_server/_provided_tools.py`); the descriptions are this port's own
//! prose, which `NOTICE` requires and the parity ledger records.

use serde_json::{Value, json};
use vibe_core::tools::shell::CommandEnvironment;

#[cfg(test)]
mod catalog_tests;

/// One declaration as the request carries it.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelTool {
    pub name: &'static str,
    pub description: String,
    pub parameters: Value,
}

/// The harness tool a model tool runs, when it runs one directly.
#[must_use]
pub fn harness_name(model_name: &str) -> Option<&'static str> {
    Some(match model_name {
        "read_file" => "file_system.read_file",
        "write_file" => "file_system.write_file",
        "edit" => "file_system.search_replace",
        "bash" => "file_system.bash",
        "skill" => "skill.read",
        "ask_user_question" => "ui.ask_user_question",
        "todo" => "vibe.todo",
        "unified_harness_scratchpad" => "vibe.unified_harness_scratchpad",
        "cron" => "vibe.cron",
        _ => return None,
    })
}

/// The arguments the harness tool takes for the ones the model sent, which
/// already satisfy the model-visible schema.
#[must_use]
pub fn harness_arguments(model_name: &str, arguments: Value) -> Value {
    if model_name != "edit" {
        return arguments;
    }
    json!({
        "file_path": arguments["file_path"],
        "content": [{
            "old_str": arguments["old_string"],
            "new_str": arguments["new_string"],
            "replace_all": arguments.get("replace_all").cloned().unwrap_or(Value::Bool(false)),
        }],
    })
}

/// What the `bash` tool tells the model about the shell it drives.
///
/// Reference `CommandEnvironment::profile` (`harness/core/src/core/tools/`)
/// describes one tool per command environment, so a model on Windows learns
/// which syntax its commands are read in; the prose here is this port's own.
fn bash_description(environment: CommandEnvironment) -> &'static str {
    match environment {
        CommandEnvironment::Unix => {
            "Run a command through the login shell in the working directory and return its output."
        }
        CommandEnvironment::GitBash => {
            "Run a command through Git Bash on native Windows, in the working directory, and \
             return its output. Each call starts a fresh `bash -c` process, so write POSIX shell \
             syntax (not PowerShell or cmd.exe syntax); stdout, stderr and the return code come \
             back separately."
        }
        CommandEnvironment::PowerShell => {
            "Run a command through PowerShell on native Windows, in the working directory, and \
             return its output. Each call starts a fresh PowerShell process, so write PowerShell \
             syntax and cmdlets (not Bash or cmd.exe syntax); stdout, stderr and the return code \
             come back separately."
        }
    }
}

/// Every declaration, in the order the reference sends them, with the shell
/// tool described for the session's command `environment`.
#[must_use]
pub fn model_tools(environment: CommandEnvironment) -> Vec<ModelTool> {
    vec![
        ModelTool {
            name: "search_tool_functions",
            description: SEARCH_DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Words describing the capability to look for, matched against function names and descriptions.",
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["best_match", "details", "all_connector_capabilities"],
                        "default": "best_match",
                        "description": "best_match ranks functions by the query; details returns full signatures for the named functions; all_connector_capabilities lists what the named connectors offer.",
                    },
                    "functions": {
                        "type": "array",
                        "items": {"type": "string", "minLength": 1},
                        "maxItems": 10,
                        "default": [],
                        "description": "Fully qualified function names to describe in details mode.",
                    },
                    "functionNames": {
                        "type": "array",
                        "items": {"type": "string", "minLength": 1},
                        "maxItems": 10,
                        "description": "Alias of functions, kept for older callers.",
                    },
                    "connectors": {
                        "type": "array",
                        "items": {"type": "string", "minLength": 1},
                        "maxItems": 10,
                        "default": [],
                        "description": "Connector names whose capabilities to list, or to restrict a search to.",
                    },
                },
                "additionalProperties": false,
            }),
        },
        ModelTool {
            name: "run_typescript",
            description: RUN_TYPESCRIPT_DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {"code": {"type": "string"}},
                "required": ["code"],
                "additionalProperties": false,
            }),
        },
        ModelTool {
            name: "read_file",
            description: "Return the text of a file, optionally starting at a zero-based line and stopping after a number of lines.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "default": 0, "minimum": 0},
                    "limit": {"type": ["integer", "null"], "default": null, "minimum": 1},
                },
                "required": ["path"],
                "additionalProperties": false,
            }),
        },
        ModelTool {
            name: "write_file",
            description: "Write UTF-8 text to a file, replacing it if present.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                },
                "required": ["path", "content"],
                "additionalProperties": false,
            }),
        },
        ModelTool {
            name: "edit",
            description: EDIT_DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The file to change, relative to the working directory or absolute.",
                    },
                    "old_string": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The text to find, character for character, whitespace and line endings included.",
                    },
                    "new_string": {
                        "type": "string",
                        "description": "What replaces the match. Empty deletes it; it has to differ from old_string.",
                    },
                    "replace_all": {
                        "type": "boolean",
                        "default": false,
                        "description": "Replace every match instead of requiring exactly one.",
                    },
                },
                "required": ["file_path", "old_string", "new_string"],
                "additionalProperties": false,
            }),
        },
        ModelTool {
            name: "bash",
            description: bash_description(environment).to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_seconds": {"type": "integer", "default": 300, "minimum": 1, "maximum": 300},
                },
                "required": ["command"],
                "additionalProperties": false,
            }),
        },
        ModelTool {
            name: "skill",
            description: SKILL_DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The skill to load, as the list of available skills names it.",
                    },
                },
                "required": ["name"],
                "additionalProperties": false,
            }),
        },
        ModelTool {
            name: "ask_user_question",
            description: "Put one or more multiple-choice questions to the user and wait for the answers.".to_owned(),
            parameters: question_schema(),
        },
        ModelTool {
            name: "todo",
            description: TODO_DESCRIPTION.to_owned(),
            parameters: todo_schema(),
        },
        ModelTool {
            name: "unified_harness_scratchpad",
            description: SCRATCHPAD_DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "title": "ScratchpadArgs",
                "properties": {
                    "action": {
                        "type": "string",
                        "title": "Action",
                        "enum": ["read", "write", "list"],
                        "description": "read returns a note, write stores one (replacing it), list names every note kept so far.",
                    },
                    "path": {
                        "anyOf": [{"type": "string"}, {"type": "null"}],
                        "default": null,
                        "title": "Path",
                        "description": "The note's relative path inside the scratchpad, for read and write.",
                    },
                    "content": {
                        "anyOf": [{"type": "string"}, {"type": "null"}],
                        "default": null,
                        "title": "Content",
                        "description": "The text a write stores.",
                    },
                },
                "required": ["action"],
            }),
        },
        ModelTool {
            name: "cron",
            description: CRON_DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["schedule", "schedule_cron", "list", "cancel", "clear"],
                        "description": "What to do: add a repeating prompt by interval or by cron expression, list, cancel one, or clear them all.",
                    },
                    "cron": {
                        "type": "string",
                        "minLength": 1,
                        "title": "Cron",
                        "description": "A five-field cron expression, for schedule_cron.",
                    },
                    "id": {"type": "string", "minLength": 1, "title": "Id"},
                    "interval_seconds": {
                        "type": "integer",
                        "minimum": 30,
                        "title": "Interval Seconds",
                    },
                    "prompt": {"type": "string", "minLength": 1, "title": "Prompt"},
                },
                "required": ["action"],
                "additionalProperties": false,
            }),
        },
    ]
}

fn question_schema() -> Value {
    json!({
        "type": "object",
        "title": "UserQuestionRequest",
        "$defs": {
            "QuestionChoice": {
                "type": "object",
                "title": "QuestionChoice",
                "properties": {
                    "label": {
                        "type": "string",
                        "title": "Label",
                        "description": "The short answer the user picks.",
                    },
                    "description": {
                        "type": "string",
                        "default": "",
                        "title": "Description",
                        "description": "What choosing it means.",
                    },
                },
                "required": ["label"],
                "additionalProperties": false,
            },
            "UserQuestion": {
                "type": "object",
                "title": "UserQuestion",
                "properties": {
                    "question": {
                        "type": "string",
                        "title": "Question",
                        "description": "What to ask.",
                    },
                    "header": {
                        "type": "string",
                        "default": "",
                        "maxLength": 20,
                        "title": "Header",
                        "description": "A tag of a word or two shown above the question.",
                    },
                    "options": {
                        "type": "array",
                        "items": {"$ref": "#/$defs/QuestionChoice"},
                        "minItems": 2,
                        "title": "Options",
                        "description": "The answers offered; the user can always type another unless hideOther is set.",
                    },
                    "multiSelect": {
                        "type": "boolean",
                        "default": false,
                        "title": "Multiselect",
                        "description": "Let the user pick several answers.",
                    },
                    "hideOther": {
                        "type": "boolean",
                        "default": false,
                        "title": "Hideother",
                        "description": "Offer no free-text answer.",
                    },
                },
                "required": ["question", "options"],
                "additionalProperties": false,
            },
        },
        "properties": {
            "questions": {
                "type": "array",
                "items": {"$ref": "#/$defs/UserQuestion"},
                "minItems": 1,
                "title": "Questions",
                "description": "The questions, asked together.",
            },
            "footerNote": {
                "anyOf": [{"type": "string"}, {"type": "null"}],
                "default": null,
                "title": "Footernote",
                "description": "A line shown under the questions.",
            },
        },
        "required": ["questions"],
        "additionalProperties": false,
    })
}

fn todo_schema() -> Value {
    json!({
        "type": "object",
        "title": "TodoArgs",
        "$defs": {
            "TodoItem": {
                "type": "object",
                "title": "TodoItem",
                "properties": {
                    "id": {
                        "type": "string",
                        "title": "Id",
                        "description": "A stable identifier, kept when the item is rewritten.",
                    },
                    "content": {
                        "type": "string",
                        "title": "Content",
                        "description": "What the task is.",
                    },
                    "status": {
                        "$ref": "#/$defs/TodoStatus",
                        "default": "pending",
                        "description": "Where the task stands.",
                    },
                    "priority": {
                        "$ref": "#/$defs/TodoPriority",
                        "default": "medium",
                        "description": "How much the task matters.",
                    },
                },
                "required": ["id", "content"],
            },
            "TodoPriority": {
                "type": "string",
                "title": "TodoPriority",
                "enum": ["low", "medium", "high"],
            },
            "TodoStatus": {
                "type": "string",
                "title": "TodoStatus",
                "enum": ["pending", "in_progress", "completed", "cancelled"],
            },
        },
        "properties": {
            "action": {
                "type": "string",
                "title": "Action",
                "enum": ["read", "write"],
                "description": "read returns the list; write replaces it with todos.",
            },
            "todos": {
                "anyOf": [{"type": "array", "items": {"$ref": "#/$defs/TodoItem"}}, {"type": "null"}],
                "default": null,
                "title": "Todos",
                "description": "The whole list a write keeps.",
            },
        },
        "required": ["action"],
    })
}

const SEARCH_DESCRIPTION: &str = "\
Find the functions run_typescript can call beyond the direct tools: MCP \
servers, connectors and the built-in groups under tools.<group>.<function>. \
Search by query to get the best matches with their signatures, ask for \
details on functions you already know by name, or list everything a \
connector offers. Call it before writing code that uses a function you have \
not seen described in this conversation.";

const RUN_TYPESCRIPT_DESCRIPTION: &str = "\
Run a TypeScript program in a sandbox and return what it produced. The \
program reaches every tool function through the global `tools` object, \
grouped by namespace (`await tools.process.start({...})`, \
`await tools.subagent.run({...})`), and can combine several calls, loop, \
filter results and compute before answering. Only what the program logs or \
returns comes back, so print the parts you need. Each call is independent: \
no variable survives from one run to the next, and the program has no \
network or filesystem access of its own beyond those functions. Use \
search_tool_functions first to learn the names and signatures of functions \
you have not used yet.";

const EDIT_DESCRIPTION: &str = "\
Replace text in a file. Read the file first: old_string has to match the \
file exactly, indentation and line endings included, and occur exactly once \
unless replace_all is true. When an edit is refused, read the file again \
before retrying.";

const SKILL_DESCRIPTION: &str = "\
Load the instructions of one of the skills listed as available to this \
session, by name. Load a skill when the task matches what it describes, \
before starting the work, and follow what it says.";

const TODO_DESCRIPTION: &str = "\
Keep the task list for this session. Write the whole list each time it \
changes, with one item in progress at a time, and mark items completed as \
soon as they are. Read it to recall where the work stands. Use it for work \
with several steps; skip it for a single quick action.";

const SCRATCHPAD_DESCRIPTION: &str = "\
A private notebook for this session, kept outside the workspace. Store \
intermediate findings, plans and large outputs there instead of repeating \
them in the conversation, and read them back when needed. Paths are relative \
to the scratchpad and cannot leave it.";

const CRON_DESCRIPTION: &str = "\
Schedule a prompt to run again later in this session, every so many seconds \
or on a cron expression, and list, cancel or clear what is scheduled. A \
scheduled prompt starts its own turn when the session is idle.";
