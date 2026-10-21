//! Reference `_process_actions` argument checks, which refuse a call before
//! it is gated.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{Operation, failure};
use crate::state::ToolResult;

fn invalid(tool: &str, location: &str, kind: &str, message: &str) -> ToolResult {
    failure(
        "invalid_arguments",
        &format!("Invalid arguments for {tool}"),
        json!({"errors": [{"location": location, "message": message, "type": kind}], "tool": tool}),
    )
}

/// The refusal of `arguments` to `operation`, or `None` when they may run.
#[must_use]
pub fn precheck(operation: Operation, arguments: &Value, workspace: &Path) -> Option<ToolResult> {
    match operation {
        Operation::Start => {
            let tool = "process.start";
            if arguments["command"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .is_empty()
            {
                return Some(invalid(
                    tool,
                    "command",
                    "empty_command",
                    "Command must not be empty",
                ));
            }
            let cwd = start_directory(arguments, workspace);
            if !cwd.is_dir() {
                return Some(invalid(
                    tool,
                    "cwd",
                    "invalid_working_directory",
                    "Working directory must exist and be a directory",
                ));
            }
            for (key, value) in arguments["env"].as_object().into_iter().flatten() {
                let value = value.as_str().unwrap_or_default();
                let problem = if key.is_empty() {
                    Some((
                        "invalid_environment_name",
                        "Environment variable name must not be empty",
                    ))
                } else if key.contains('=') {
                    Some((
                        "invalid_environment_name",
                        "Environment variable name must not contain '='",
                    ))
                } else if key.contains('\0') {
                    Some((
                        "invalid_environment_name",
                        "Environment variable name must not contain a null character",
                    ))
                } else if value.contains('\0') {
                    Some((
                        "invalid_environment_value",
                        "Environment variable value must not contain a null character",
                    ))
                } else {
                    None
                };
                if let Some((kind, message)) = problem {
                    return Some(invalid(tool, &format!("env.{key}"), kind, message));
                }
            }
            None
        }
        Operation::Output
            if arguments["waitMs"]
                .as_u64()
                .is_some_and(|wait| wait > 30_000) =>
        {
            Some(invalid(
                "process.output",
                "waitMs",
                "value_too_large",
                "waitMs must be at most 30000",
            ))
        }
        Operation::Write => {
            if arguments["text"].as_str().is_some_and(str::is_empty) {
                return Some(invalid(
                    "process.write",
                    "text",
                    "empty_input",
                    "Process input must not be empty",
                ));
            }
            let encoded = arguments["bytesBase64"].as_str()?;
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .is_err()
                .then(|| {
                    invalid(
                        "process.write",
                        "bytesBase64",
                        "invalid_base64",
                        "bytesBase64 must be valid base64",
                    )
                })
        }
        Operation::Output | Operation::List | Operation::Stop => None,
    }
}

/// Where a process starts: its `cwd`, relative to the workspace, or the
/// workspace.
pub(super) fn start_directory(arguments: &Value, workspace: &Path) -> PathBuf {
    match arguments["cwd"]
        .as_str()
        .filter(|cwd| !cwd.trim().is_empty())
    {
        Some(cwd) => workspace.join(cwd),
        None => workspace.to_path_buf(),
    }
}
