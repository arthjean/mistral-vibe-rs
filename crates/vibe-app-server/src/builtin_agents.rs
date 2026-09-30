use std::path::Path;

use toml::{Table, Value as TomlValue};
use vibe_core::extensions::{AgentKind, AgentProfile, ExtensionSource};

/// The builtin profile that stays out of the picker until the configuration
/// offers it or a session is started under it
/// (`vibe/core/agents/manager.py:37-56` and `:96-107`).
pub(crate) const SMART_APPROVE: &str = "smart-approve";

/// Where a builtin sits in reference `BUILTIN_AGENTS`, and after every
/// builtin for any other name. The sort that uses it is stable, so agents of
/// equal rank keep the order they arrive in.
pub(crate) fn declaration_rank(name: &str) -> usize {
    const ORDER: [&str; 7] = [
        "ask",
        "plan",
        "accept-edits",
        SMART_APPROVE,
        "auto-approve",
        "explore",
        "lean",
    ];
    ORDER
        .iter()
        .position(|builtin| *builtin == name)
        .unwrap_or(ORDER.len())
}

pub(crate) fn default_profile() -> AgentProfile {
    builtin_agent(
        "ask",
        "Ask",
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
        // Reference `SMART_APPROVE` (`vibe/core/agents/models.py:100-108`):
        // no permission override, because the classifier that gates each call
        // belongs to the Unified Harness. On the legacy harness, which is the
        // only one this port runs, its calls are approved the ordinary way.
        builtin_agent(
            SMART_APPROVE,
            "Smart Approve",
            "Classifies each tool call and auto-runs the safe ones, prompting only for risky ones",
            AgentKind::Agent,
            "smart",
            toml_table([("disabled_tools", string_array(["exit_plan_mode"]))]),
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
                (
                    "enabled_tools",
                    string_array(["grep", "read_file", "skill"]),
                ),
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
                ("allowed_models", string_array(["leanstral"])),
                (
                    "providers",
                    TomlValue::Array(vec![TomlValue::Table(toml_table([
                        ("name", TomlValue::String("mistral-testing".to_owned())),
                        (
                            "api_base",
                            TomlValue::String("https://api.mistral.ai/v1".to_owned()),
                        ),
                        (
                            "api_key_env_var",
                            TomlValue::String("MISTRAL_API_KEY".to_owned()),
                        ),
                        ("backend", TomlValue::String("mistral".to_owned())),
                    ]))]),
                ),
                (
                    "models",
                    TomlValue::Array(vec![TomlValue::Table(toml_table([
                        ("name", TomlValue::String("labs-leanstral-1-5".to_owned())),
                        ("provider", TomlValue::String("mistral-testing".to_owned())),
                        ("alias", TomlValue::String("leanstral".to_owned())),
                        ("thinking", TomlValue::String("high".to_owned())),
                        ("temperature", TomlValue::Float(1.0)),
                        ("auto_compact_threshold", TomlValue::Integer(200_000)),
                    ]))]),
                ),
                (
                    "compaction_model",
                    TomlValue::Table(toml_table([
                        ("name", TomlValue::String("mistral-small-latest".to_owned())),
                        ("provider", TomlValue::String("mistral-testing".to_owned())),
                        ("alias", TomlValue::String("devstral-compact".to_owned())),
                        ("temperature", TomlValue::Float(0.2)),
                        ("thinking", TomlValue::String("off".to_owned())),
                    ])),
                ),
                (
                    "tools",
                    TomlValue::Table(toml_table([(
                        "bash",
                        TomlValue::Table(toml_table([(
                            "default_timeout",
                            TomlValue::Integer(1200),
                        )])),
                    )])),
                ),
                ("disabled_tools", string_array(["exit_plan_mode"])),
            ]),
        ),
    ]
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
