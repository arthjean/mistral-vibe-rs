//! Agent profiles: the overlay a named agent runs its turns under.
//!
//! A profile carries a model, a tool allowlist or denylist, and a set of
//! permission rules. Reference expresses all three as a TOML overlay, so what
//! lives here is the translation from that document into the typed values the
//! session composes: which tools survive the filter, which model entry the
//! profile pins, and which rules the permission store is seeded with.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::Table;

use super::{ExtensionError, ExtensionSource, parse_agent};
use crate::atomic_file::write_atomically;
use crate::policy::{PermissionMode, PermissionRule, PermissionScope};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Agent,
    Subagent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProfile {
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub kind: AgentKind,
    pub safety: String,
    pub overrides: Table,
    pub source: ExtensionSource,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AgentApproval {
    #[default]
    Prompt,
    Edits,
    All,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentRuntimeSettings {
    pub enabled_tools: Vec<String>,
    pub disabled_tools: Vec<String>,
    pub permission_rules: Vec<PermissionRule>,
    pub approval: AgentApproval,
    pub model: Option<String>,
    pub thinking: Option<bool>,
    pub reasoning_effort: Option<String>,
    pub mode: Option<String>,
    pub system_prompt_id: Option<String>,
}

impl AgentProfile {
    #[must_use]
    pub fn runtime_settings(&self) -> AgentRuntimeSettings {
        let approval = if self
            .overrides
            .get("bypass_tool_permissions")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false)
        {
            AgentApproval::All
        } else if auto_approves_edits(&self.overrides) {
            AgentApproval::Edits
        } else {
            AgentApproval::Prompt
        };
        let active_model = self
            .overrides
            .get("active_model")
            .and_then(toml::Value::as_str);
        let model = active_model.map(|active_model| {
            profile_model(&self.overrides, active_model)
                .and_then(|model| model.get("name"))
                .and_then(toml::Value::as_str)
                .unwrap_or(active_model)
                .to_owned()
        });
        let reasoning_effort = active_model
            .and_then(|active_model| profile_model(&self.overrides, active_model))
            .and_then(|model| model.get("thinking"))
            .and_then(toml::Value::as_str)
            .map(ToOwned::to_owned);
        let mut disabled_tools = profile_tool_names(&self.overrides, "disabled_tools");
        disabled_tools.extend(profile_tools_disabled_without_allowlist(&self.overrides));
        disabled_tools.sort();
        disabled_tools.dedup();
        AgentRuntimeSettings {
            enabled_tools: profile_tool_names(&self.overrides, "enabled_tools"),
            disabled_tools,
            permission_rules: profile_permission_rules(&self.name, &self.overrides),
            approval,
            model,
            thinking: reasoning_effort.as_deref().map(|effort| effort != "off"),
            reasoning_effort: reasoning_effort.filter(|effort| effort != "off"),
            mode: self
                .overrides
                .get("mode")
                .and_then(toml::Value::as_str)
                .filter(|mode| matches!(*mode, "code" | "plan"))
                .map(ToOwned::to_owned),
            system_prompt_id: self
                .overrides
                .get("system_prompt_id")
                .and_then(toml::Value::as_str)
                .map(ToOwned::to_owned),
        }
    }
}

pub(super) fn profile_tool_names(overrides: &Table, key: &str) -> Vec<String> {
    overrides
        .get(key)
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(canonical_tool_name)
        .map(ToOwned::to_owned)
        .collect()
}

pub(super) fn profile_model<'a>(overrides: &'a Table, active_model: &str) -> Option<&'a Table> {
    overrides
        .get("models")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table)
        .find(|model| model.get("alias").and_then(toml::Value::as_str) == Some(active_model))
}

pub(super) fn profile_tools_disabled_without_allowlist(overrides: &Table) -> Vec<String> {
    overrides
        .get("tools")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(Table::iter)
        .filter(|(_, value)| {
            value.as_table().is_some_and(|settings| {
                settings.get("permission").and_then(toml::Value::as_str) == Some("never")
                    && settings
                        .get("allowlist")
                        .and_then(toml::Value::as_array)
                        .is_none_or(Vec::is_empty)
            })
        })
        .map(|(name, _)| canonical_tool_name(name))
        .map(ToOwned::to_owned)
        .collect()
}

pub(super) fn profile_permission_rules(
    profile_name: &str,
    overrides: &Table,
) -> Vec<PermissionRule> {
    let mut rules = BTreeMap::<(String, String), PermissionRule>::new();
    for (raw_tool, value) in overrides
        .get("tools")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(Table::iter)
    {
        let Some(settings) = value.as_table() else {
            continue;
        };
        let Some(mode) = settings
            .get("permission")
            .and_then(toml::Value::as_str)
            .and_then(profile_permission_mode)
        else {
            continue;
        };
        let tool = canonical_tool_name(raw_tool).to_owned();
        let allowlist = settings
            .get("allowlist")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>();
        if allowlist.is_empty() || mode == PermissionMode::Never {
            insert_profile_rule(&mut rules, profile_name, &tool, None, "*".to_owned(), mode);
        }
        for pattern in allowlist {
            let allowlist_mode = if mode == PermissionMode::Never {
                PermissionMode::Always
            } else {
                mode
            };
            insert_profile_rule(
                &mut rules,
                profile_name,
                &tool,
                Some(profile_permission_scope(&tool)),
                pattern.to_owned(),
                allowlist_mode,
            );
        }
    }
    rules.into_values().collect()
}

fn insert_profile_rule(
    rules: &mut BTreeMap<(String, String), PermissionRule>,
    profile_name: &str,
    tool: &str,
    scope: Option<PermissionScope>,
    pattern: String,
    mode: PermissionMode,
) {
    rules.insert(
        (tool.to_owned(), pattern.clone()),
        PermissionRule {
            tool: tool.to_owned(),
            scope,
            pattern,
            mode,
            rationale: format!("agent-profile:{profile_name}"),
        },
    );
}

pub(super) fn profile_permission_mode(value: &str) -> Option<PermissionMode> {
    match value {
        "never" => Some(PermissionMode::Never),
        "ask" => Some(PermissionMode::Ask),
        "always" => Some(PermissionMode::Always),
        _ => None,
    }
}

/// The scope a profile's `allowlist` entry answers for.
///
/// A file tool's entry is a path glob, which is the shape an
/// `outside_directory` requirement carries; every other tool's is a command or
/// a name, which is the `command_pattern` shape. The reference reaches the same
/// place through the tool's own configured allowlist rather than through a
/// rule, so this mapping is what keeps a profile written for one client
/// meaningful on the other.
pub(super) fn profile_permission_scope(tool: &str) -> PermissionScope {
    match tool {
        "read_file" | "grep" | "edit" | "write_file" => PermissionScope::OutsideDirectory,
        _ => PermissionScope::CommandPattern,
    }
}

/// Maps a profile's tool name onto the name this port publishes.
///
/// `read_file` and `grep` used to be rewritten to invented local names; they
/// are now published verbatim, so only the names this port has not yet aligned
/// still need a rewrite. Each remaining entry disappears with the story that
/// publishes the reference name.
pub(super) fn canonical_tool_name(name: &str) -> &str {
    match name {
        "write" | "write_file" => "edit",
        other => other,
    }
}

pub(super) fn auto_approves_edits(overrides: &Table) -> bool {
    overrides
        .get("tools")
        .and_then(toml::Value::as_table)
        .is_some_and(|tools| {
            ["edit", "write", "write_file"].into_iter().any(|tool| {
                tools
                    .get(tool)
                    .and_then(toml::Value::as_table)
                    .is_some_and(|settings| {
                        settings.get("permission").and_then(toml::Value::as_str) == Some("always")
                            && settings
                                .get("allowlist")
                                .and_then(toml::Value::as_array)
                                .is_none_or(Vec::is_empty)
                    })
            })
        })
}

#[derive(Debug, Clone)]
pub struct AgentRegistry {
    agents: BTreeMap<String, AgentProfile>,
    user_directory: PathBuf,
    active: String,
}

impl AgentRegistry {
    #[must_use]
    pub fn with_initial(profile: AgentProfile, user_directory: PathBuf) -> Self {
        let active = profile.name.clone();
        Self {
            agents: BTreeMap::from([(profile.name.clone(), profile)]),
            user_directory,
            active,
        }
    }

    pub fn new(
        agents: BTreeMap<String, AgentProfile>,
        user_directory: PathBuf,
        active: &str,
    ) -> Result<Self, ExtensionError> {
        if !agents.contains_key(active) {
            return Err(ExtensionError::MissingAgent(active.to_owned()));
        }
        Ok(Self {
            agents,
            user_directory,
            active: active.to_owned(),
        })
    }

    #[must_use]
    pub fn list(&self) -> Vec<&AgentProfile> {
        self.agents.values().collect()
    }

    pub fn profile(&self, name: &str) -> Result<&AgentProfile, ExtensionError> {
        let profile = self
            .agents
            .get(name)
            .ok_or_else(|| ExtensionError::MissingAgent(name.to_owned()))?;
        if profile.kind != AgentKind::Agent {
            return Err(ExtensionError::SubagentCannotBePrimary(name.to_owned()));
        }
        Ok(profile)
    }

    pub fn set_active(&mut self, name: &str) -> Result<&AgentProfile, ExtensionError> {
        self.profile(name)?;
        self.active = name.to_owned();
        Ok(&self.agents[name])
    }

    pub fn register_builtin(&mut self, profile: AgentProfile) {
        self.agents.insert(profile.name.clone(), profile);
    }

    pub fn install(&mut self, source: &Path) -> Result<AgentProfile, ExtensionError> {
        let profile = parse_agent(source, ExtensionSource::User)?;
        fs::create_dir_all(&self.user_directory).map_err(|source| ExtensionError::Io {
            path: self.user_directory.clone(),
            source,
        })?;
        let destination = self.user_directory.join(format!("{}.toml", profile.name));
        let contents = fs::read(source).map_err(|error| ExtensionError::Io {
            path: source.to_path_buf(),
            source: error,
        })?;
        write_atomically(&destination, "agent", &contents).map_err(|error| ExtensionError::Io {
            path: error.path,
            source: error.source,
        })?;
        let installed = parse_agent(&destination, ExtensionSource::User)?;
        self.agents
            .insert(installed.name.clone(), installed.clone());
        Ok(installed)
    }

    pub fn uninstall(&mut self, name: &str) -> Result<(), ExtensionError> {
        let profile = self
            .agents
            .get(name)
            .ok_or_else(|| ExtensionError::MissingAgent(name.to_owned()))?;
        if profile.source != ExtensionSource::User {
            return Err(ExtensionError::AgentNotUserOwned(name.to_owned()));
        }
        let path = profile
            .path
            .clone()
            .ok_or_else(|| ExtensionError::AgentNotUserOwned(name.to_owned()))?;
        let canonical_parent =
            fs::canonicalize(&self.user_directory).map_err(|source| ExtensionError::Io {
                path: self.user_directory.clone(),
                source,
            })?;
        let canonical = fs::canonicalize(&path).map_err(|source| ExtensionError::Io {
            path: path.clone(),
            source,
        })?;
        if !canonical.starts_with(&canonical_parent) {
            return Err(ExtensionError::AgentNotUserOwned(name.to_owned()));
        }
        fs::remove_file(&canonical).map_err(|source| ExtensionError::Io {
            path: canonical,
            source,
        })?;
        self.agents.remove(name);
        if self.active == name {
            // `new` guarantees the active agent exists; keep that invariant by
            // falling back to a profile that is actually registered.
            self.active = self
                .agents
                .values()
                .find(|profile| profile.kind == AgentKind::Agent)
                .map(|profile| profile.name.clone())
                .ok_or_else(|| ExtensionError::MissingAgent("default".to_owned()))?;
        }
        Ok(())
    }
}
