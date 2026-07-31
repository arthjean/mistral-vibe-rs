use std::path::Path;

use toml::{Table, Value as TomlValue};
use vibe_core::extensions::{AgentKind, AgentProfile, ExtensionSource};

pub(crate) fn default_profile() -> AgentProfile {
    builtin_agent(
        "default",
        "Default",
        "Requires approval for tool executions",
        AgentKind::Agent,
        "neutral",
        toml_table([("disabled_tools", string_array(["exit_plan_mode"]))]),
    )
}

pub(crate) fn profiles(vibe_home: &Path) -> Vec<AgentProfile> {
    let plans_pattern = vibe_home
        .join("plans")
        .join("*")
        .to_string_lossy()
        .into_owned();
    let plan_tools = TomlValue::Table(toml_table([
        (
            "write_file",
            TomlValue::Table(toml_table([
                ("permission", TomlValue::String("never".to_owned())),
                (
                    "allowlist",
                    TomlValue::Array(vec![TomlValue::String(plans_pattern.clone())]),
                ),
            ])),
        ),
        (
            "edit",
            TomlValue::Table(toml_table([
                ("permission", TomlValue::String("never".to_owned())),
                (
                    "allowlist",
                    TomlValue::Array(vec![TomlValue::String(plans_pattern.clone())]),
                ),
            ])),
        ),
        (
            "read_file",
            TomlValue::Table(toml_table([(
                "allowlist",
                TomlValue::Array(vec![TomlValue::String(plans_pattern)]),
            )])),
        ),
    ]));
    let edit_permissions = TomlValue::Table(toml_table([
        (
            "write_file",
            TomlValue::Table(toml_table([(
                "permission",
                TomlValue::String("always".to_owned()),
            )])),
        ),
        (
            "edit",
            TomlValue::Table(toml_table([(
                "permission",
                TomlValue::String("always".to_owned()),
            )])),
        ),
    ]));
    vec![
        builtin_agent(
            "plan",
            "Plan",
            "Read-only agent for exploration and planning",
            AgentKind::Agent,
            "safe",
            toml_table([
                ("mode", TomlValue::String("plan".to_owned())),
                ("tools", plan_tools),
            ]),
        ),
        builtin_agent(
            "accept-edits",
            "Accept Edits",
            "Auto-approves file edits only",
            AgentKind::Agent,
            "destructive",
            toml_table([
                ("disabled_tools", string_array(["exit_plan_mode"])),
                ("tools", edit_permissions),
            ]),
        ),
        builtin_agent(
            "auto-approve",
            "Auto Approve",
            "Auto-approves all tool executions",
            AgentKind::Agent,
            "yolo",
            toml_table([
                ("bypass_tool_permissions", TomlValue::Boolean(true)),
                ("disabled_tools", string_array(["exit_plan_mode"])),
            ]),
        ),
        builtin_agent(
            "explore",
            "Explore",
            "Read-only subagent for codebase exploration",
            AgentKind::Subagent,
            "safe",
            toml_table([
                ("enabled_tools", string_array(["search", "read"])),
                ("system_prompt_id", TomlValue::String("explore".to_owned())),
            ]),
        ),
        builtin_agent(
            "lean",
            "Lean",
            "Specialized mode for Lean 4 code analysis, proof assistance, and theorem proving",
            AgentKind::Agent,
            "neutral",
            toml_table([
                ("system_prompt_id", TomlValue::String("lean".to_owned())),
                ("active_model", TomlValue::String("leanstral".to_owned())),
                (
                    "models",
                    TomlValue::Array(vec![TomlValue::Table(toml_table([
                        ("name", TomlValue::String("labs-leanstral-1-5".to_owned())),
                        ("alias", TomlValue::String("leanstral".to_owned())),
                        ("thinking", TomlValue::String("high".to_owned())),
                    ]))]),
                ),
                ("disabled_tools", string_array(["exit_plan_mode"])),
            ]),
        ),
    ]
}

pub(crate) fn system_prompt(id: &str) -> Option<&'static str> {
    match id {
        "explore" => Some(
            "You are a senior engineer analyzing codebases. Use only read-only tools. Start with code, a diagram, or structured output, then give at most two concise context sentences.",
        ),
        "lean" => Some(
            "You are Leanstral, a Lean 4 coding agent. Inspect the project and its Lean toolchain before changing code. Prefer focused edits, verify them with the narrowest relevant lake build or test, use grind when the project Lean version supports it, and never use native_decide.",
        ),
        _ => None,
    }
}

fn builtin_agent(
    name: &str,
    display_name: &str,
    description: &str,
    kind: AgentKind,
    safety: &str,
    overrides: Table,
) -> AgentProfile {
    AgentProfile {
        name: name.to_owned(),
        display_name: display_name.to_owned(),
        description: description.to_owned(),
        kind,
        safety: safety.to_owned(),
        overrides,
        source: ExtensionSource::Builtin,
        path: None,
    }
}

fn toml_table<const N: usize>(entries: [(&str, TomlValue); N]) -> Table {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn string_array<const N: usize>(values: [&str; N]) -> TomlValue {
    TomlValue::Array(
        values
            .into_iter()
            .map(|value| TomlValue::String(value.to_owned()))
            .collect(),
    )
}
