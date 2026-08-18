//! What the family publishes: the five tool declarations and the control-key
//! vocabulary two of them share.
//!
//! A spec is the tool's whole contract with the model, so these are the
//! functions a parity measurement reads. They compose no behavior and reach no
//! state, which is what lets the surface a Windows operator sees be proven from
//! a POSIX host.

use serde_json::{Value, json};

use crate::schema::{ObjectSchema, Property};
use crate::tools::{ToolAvailability, ToolPresentationKind, ToolSource, ToolSpec};

use super::{LEGACY_SELECTION_PRIORITY, MANAGED_SELECTION_PRIORITY, ShellFamily};
use crate::tools::config::declared_document;

/// Reference `ControlKey`, in declaration order, paired with the bytes each one
/// writes. The schema publishes the names; the stdin tool writes the bytes.
pub(super) const CONTROL_KEYS: [(&str, &[u8]); 40] = [
    ("ctrl_@", b"\x00"),
    ("ctrl_a", b"\x01"),
    ("ctrl_b", b"\x02"),
    ("ctrl_c", b"\x03"),
    ("ctrl_d", b"\x04"),
    ("ctrl_e", b"\x05"),
    ("ctrl_f", b"\x06"),
    ("ctrl_g", b"\x07"),
    ("ctrl_h", b"\x08"),
    ("ctrl_i", b"\x09"),
    ("tab", b"\x09"),
    ("ctrl_j", b"\x0a"),
    ("enter", b"\r"),
    ("return", b"\r"),
    ("ctrl_k", b"\x0b"),
    ("ctrl_l", b"\x0c"),
    ("ctrl_m", b"\r"),
    ("ctrl_n", b"\x0e"),
    ("ctrl_o", b"\x0f"),
    ("ctrl_p", b"\x10"),
    ("ctrl_q", b"\x11"),
    ("ctrl_r", b"\x12"),
    ("ctrl_s", b"\x13"),
    ("ctrl_t", b"\x14"),
    ("ctrl_u", b"\x15"),
    ("ctrl_v", b"\x16"),
    ("ctrl_w", b"\x17"),
    ("ctrl_x", b"\x18"),
    ("ctrl_y", b"\x19"),
    ("ctrl_z", b"\x1a"),
    ("esc", b"\x1b"),
    ("escape", b"\x1b"),
    ("backspace", b"\x7f"),
    ("delete", b"\x1b[3~"),
    ("up", b"\x1b[A"),
    ("down", b"\x1b[B"),
    ("right", b"\x1b[C"),
    ("left", b"\x1b[D"),
    ("home", b"\x1b[H"),
    ("end", b"\x1b[F"),
];

// --------------------------------------------------------------------------
// Specifications
// --------------------------------------------------------------------------

/// Directive coverage for a family's command tool, whose reference description
/// this port must cover without reproducing (`NOTICE`).
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The tool runs one shell command in the workspace | "Run one shell command in the working directory" |
/// | Prefer the file tools over shell equivalents for reading and searching | "reach for read_file and grep instead of cat and grep(1)" |
/// | Quote paths that carry spaces | "Quote every path that carries a space" |
/// | A command that needs approval is paused until the operator answers | "A command the policy does not allow outright waits for approval" |
/// | The timeout is optional and bounded | the `timeout` description |
/// | The managed variant can return a live session instead of waiting | the `background` description |
/// | The Windows families name the shell they drive | the family sentence |
///
/// The legacy Windows variants publish the four overrides the POSIX one does
/// not, because reference `GitBashArgs` and `WindowsShellArgs` carry them even
/// though `BashArgs` does not.
pub(super) fn command_spec(family: ShellFamily, managed: bool) -> ToolSpec {
    let description = format!(
        "Run one {} command in the working directory. Reach for read_file and grep instead of cat \
         and grep(1), quote every path that carries a space, and expect that a command the policy \
         does not allow outright waits for approval before it runs.",
        match family {
            ShellFamily::Bash => "shell",
            ShellFamily::GitBash => "Git Bash",
            ShellFamily::PowerShell => "PowerShell",
        }
    );
    let overrides = managed || family != ShellFamily::Bash;
    let mut schema = ObjectSchema::new().required(
        "command",
        Property::string().described(if managed {
            "The shell command to run"
        } else {
            "The shell command to execute"
        }),
    );
    schema = schema.optional(
        "timeout",
        Property::integer()
            .described(if managed {
                "How long to wait, in seconds, before the process group is killed"
            } else {
                "How long to wait, in seconds, before the command is abandoned"
            })
            .with_default(Value::Null)
            .nullable(),
    );
    if managed {
        schema = schema.optional(
            "background",
            Property::boolean()
                .described("Return a live session immediately instead of waiting for the exit")
                .with_default(false),
        );
    }
    if overrides {
        schema = schema
            .optional(
                "timeout_seconds",
                Property::number()
                    .constrained("minimum", 0)
                    .described("How long to wait in the foreground before the session is left running or killed")
                    .with_default(Value::Null)
                    .nullable(),
            );
    }
    if managed {
        schema = schema.optional(
            "hard_timeout",
            Property::boolean()
                .described("Kill the process group when timeout_seconds expires instead of leaving the session running")
                .with_default(false),
        );
    }
    if overrides {
        schema = schema
            .optional(
                "cwd",
                Property::string()
                    .described("Run somewhere other than the working directory")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "env",
                Property::map(Property::string())
                    .described("Environment variables added to the ones the session inherits")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "shell",
                Property::string()
                    .described("Run the command through another shell executable")
                    .with_default(Value::Null)
                    .nullable(),
            );
    }
    ToolSpec {
        name: family.name().to_owned(),
        description,
        input_schema: schema.build(),
        output_schema: None,
        config: declared_document(family.name()),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: if managed {
            MANAGED_SELECTION_PRIORITY
        } else {
            LEGACY_SELECTION_PRIORITY
        },
    }
}

/// Directive coverage for a family's `_output` tool.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | Poll a running or finished session for its output | "Read what a session has written" |
/// | Pass back the cursor to read only what is new | the `cursor` description |
/// | Waiting is optional and bounded | the `wait_seconds` description |
/// | A finished session still answers with its last output and status | "A session that has exited still answers" |
pub(super) fn output_spec(family: ShellFamily) -> ToolSpec {
    ToolSpec {
        name: family.tool_name("output"),
        description: format!(
            "Read what a {} session has written since a cursor. A session that has exited still \
             answers, with its final output and its exit status.",
            family.name()
        ),
        input_schema: ObjectSchema::new()
            .required("session_id", Property::string())
            .optional(
                "cursor",
                Property::integer()
                    .constrained("minimum", 0)
                    .described("The next_cursor a previous read returned")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "wait_seconds",
                Property::number().constrained("minimum", 0).with_default(0),
            )
            .optional(
                "max_bytes",
                Property::integer()
                    .constrained("exclusiveMinimum", 0)
                    .described("How many bytes to read at most")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build(),
        output_schema: None,
        config: declared_document(&family.tool_name("output")),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: LEGACY_SELECTION_PRIORITY,
    }
}

/// Directive coverage for a family's `_stdin` tool.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | Feed a running session's standard input | "Write to a running session" |
/// | Text is sent verbatim, newline included | the `text` description |
/// | Control keys are named rather than encoded | the `control` description |
/// | Raw bytes travel as base64 | the `bytes_base64` description |
/// | Exactly one of the three inputs is supplied | "Supply exactly one of text, control or bytes_base64" |
pub(super) fn stdin_spec(family: ShellFamily) -> ToolSpec {
    ToolSpec {
        name: family.tool_name("stdin"),
        description: format!(
            "Write to a running {} session. Supply exactly one of text, control or bytes_base64; \
             supplying none or several is refused rather than resolved by precedence.",
            family.name()
        ),
        input_schema: ObjectSchema::new()
            .required("session_id", Property::string())
            .optional(
                "text",
                Property::string()
                    .described("Text sent exactly as written; end it with \\n to press Enter")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "control",
                Property::array(Property::string().constrained("enum", json!(control_key_names())))
                    .described(
                        "Named control sequences, for example ctrl_c, ctrl_d, esc, tab, enter or \
                         the arrow keys",
                    ),
            )
            .optional(
                "bytes_base64",
                Property::string()
                    .described("Raw bytes to write, base64 encoded")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build(),
        output_schema: None,
        config: declared_document(&family.tool_name("stdin")),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: LEGACY_SELECTION_PRIORITY,
    }
}

/// Directive coverage for a family's `_sessions` tool.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | `list` reports this family's sessions | the `action` description |
/// | `inspect` and `kill` need a session id and act on that one session | the `action` and `session_id` descriptions |
/// | `reset` stops every session in the family | the `action` description |
/// | `clear_logs` only applies to `reset` | the `clear_logs` description |
pub(super) fn sessions_spec(family: ShellFamily) -> ToolSpec {
    ToolSpec {
        name: family.tool_name("sessions"),
        description: format!("Inspect and stop {} sessions.", family.name()),
        input_schema: ObjectSchema::new()
            .optional(
                "action",
                Property::string()
                    .constrained("enum", json!(["list", "inspect", "kill", "reset"]))
                    .described(
                        "`list` reports every session of this family, `inspect` reads the one \
                         named by session_id, `kill` stops exactly that one, and `reset` stops \
                         them all",
                    )
                    .with_default("list"),
            )
            .optional(
                "session_id",
                Property::string()
                    .described("Required by `inspect` and `kill`, ignored by `list` and `reset`")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "clear_logs",
                Property::boolean()
                    .described("Used by `reset` alone: also delete the stored session logs")
                    .with_default(false),
            )
            .optional(
                "max_bytes",
                Property::integer()
                    .constrained("exclusiveMinimum", 0)
                    .described("How many bytes of output to read at most")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build(),
        output_schema: None,
        config: declared_document(&family.tool_name("sessions")),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: LEGACY_SELECTION_PRIORITY,
    }
}

/// Directive coverage for a family's `_log_file` tool.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | Read, write or append a file in the shell-tool directory | "Read, write or append a file the sessions keep" |
/// | A session id names that session's own log | "session_id names that session's log" |
/// | A relative path stays inside the shell-tool directory | "a relative path stays inside the session log directory" |
/// | Reading resumes from an offset and is bounded | the `offset` and `max_bytes` descriptions |
pub(super) fn log_file_spec(family: ShellFamily) -> ToolSpec {
    ToolSpec {
        name: family.tool_name("log_file"),
        description: format!(
            "Read, write or append a file the {} sessions keep. session_id names that session's \
             log, and a relative path stays inside the session log directory.",
            family.name()
        ),
        input_schema: ObjectSchema::new()
            .required(
                "action",
                Property::string().constrained("enum", json!(["read", "write", "append"])),
            )
            .optional(
                "session_id",
                Property::string().with_default(Value::Null).nullable(),
            )
            .optional(
                "relative_path",
                Property::string().with_default(Value::Null).nullable(),
            )
            .optional(
                "offset",
                Property::integer()
                    .constrained("minimum", 0)
                    .described("Where to start reading, in bytes")
                    .with_default(0),
            )
            .optional(
                "max_bytes",
                Property::integer()
                    .constrained("exclusiveMinimum", 0)
                    .described("How many bytes to read at most")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "content",
                Property::string().with_default(Value::Null).nullable(),
            )
            .build(),
        output_schema: None,
        config: declared_document(&family.tool_name("log_file")),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: LEGACY_SELECTION_PRIORITY,
    }
}

fn control_key_names() -> Vec<&'static str> {
    CONTROL_KEYS.iter().map(|(name, _)| *name).collect()
}
