use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fs, io};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;
use toml::Table;

use crate::atomic_file::write_atomically;
use crate::engine::CancellationToken;
use crate::policy::{PermissionMode, PermissionRule, PermissionScope};
use crate::storage::{SessionStore, StorageError};
use crate::text::{bounded_utf8, matches_wildcard, truncate_utf8};

const MAX_EXTENSION_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_HOOK_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_DELEGATION_RESULT_BYTES: usize = 64 * 1024;
const MAX_DELEGATION_DEPTH: u8 = 3;
const MAX_DELEGATION_DURATION: Duration = Duration::from_secs(60);
const MAX_CHILD_ID_ATTEMPTS: usize = 1024;
static CHILD_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionSource {
    Builtin,
    Configured,
    Project,
    User,
}

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

fn profile_tool_names(overrides: &Table, key: &str) -> Vec<String> {
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

fn profile_model<'a>(overrides: &'a Table, active_model: &str) -> Option<&'a Table> {
    overrides
        .get("models")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table)
        .find(|model| model.get("alias").and_then(toml::Value::as_str) == Some(active_model))
}

fn profile_tools_disabled_without_allowlist(overrides: &Table) -> Vec<String> {
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

fn profile_permission_rules(profile_name: &str, overrides: &Table) -> Vec<PermissionRule> {
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

fn profile_permission_mode(value: &str) -> Option<PermissionMode> {
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
fn profile_permission_scope(tool: &str) -> PermissionScope {
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
fn canonical_tool_name(name: &str) -> &str {
    match name {
        "write" | "write_file" => "edit",
        other => other,
    }
}

fn auto_approves_edits(overrides: &Table) -> bool {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub user_invocable: bool,
    pub body: String,
    pub source: ExtensionSource,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextExtension {
    pub name: String,
    pub content: String,
    pub source: ExtensionSource,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    PreTool,
    PostTool,
    PostAgent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookSpec {
    pub name: String,
    pub kind: HookKind,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub matcher: Option<String>,
    pub timeout_ms: u64,
    pub retries: u8,
    pub strict: bool,
    pub source: ExtensionSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryIssue {
    pub mechanism: String,
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionCatalog {
    pub agents: BTreeMap<String, AgentProfile>,
    pub skills: BTreeMap<String, SkillDefinition>,
    pub hooks: Vec<HookSpec>,
    pub prompts: BTreeMap<String, TextExtension>,
    pub commands: BTreeMap<String, TextExtension>,
    pub issues: Vec<DiscoveryIssue>,
}

#[derive(Debug, Clone, Default)]
pub struct DiscoveryRoots {
    pub configured: Vec<PathBuf>,
    pub project: Vec<PathBuf>,
    pub user: Vec<PathBuf>,
    pub project_trusted: bool,
}

impl DiscoveryRoots {
    fn ordered(&self) -> Vec<(ExtensionSource, PathBuf)> {
        let mut roots = Vec::new();
        roots.extend(
            self.configured
                .iter()
                .cloned()
                .map(|path| (ExtensionSource::Configured, path)),
        );
        if self.project_trusted {
            roots.extend(
                self.project
                    .iter()
                    .cloned()
                    .map(|path| (ExtensionSource::Project, path)),
            );
        }
        roots.extend(
            self.user
                .iter()
                .cloned()
                .map(|path| (ExtensionSource::User, path)),
        );
        roots
    }
}

pub fn discover_extensions(
    roots: &DiscoveryRoots,
    builtin_agents: BTreeMap<String, AgentProfile>,
    builtin_skills: BTreeMap<String, SkillDefinition>,
    builtin_prompts: BTreeMap<String, TextExtension>,
) -> ExtensionCatalog {
    let mut catalog = ExtensionCatalog {
        agents: builtin_agents,
        skills: builtin_skills,
        hooks: Vec::new(),
        prompts: builtin_prompts,
        commands: BTreeMap::new(),
        issues: Vec::new(),
    };
    let builtin_skill_names = catalog.skills.keys().cloned().collect::<BTreeSet<_>>();

    for (source, root) in roots.ordered() {
        discover_agents(&mut catalog, source, &root.join("agents"));
        discover_skills(
            &mut catalog,
            source,
            &root.join("skills"),
            &builtin_skill_names,
        );
        discover_text_extensions(
            &mut catalog.prompts,
            &mut catalog.issues,
            "prompts",
            source,
            &root.join("prompts"),
        );
        discover_text_extensions(
            &mut catalog.commands,
            &mut catalog.issues,
            "commands",
            source,
            &root.join("commands"),
        );
        discover_hooks(&mut catalog, source, &root.join("hooks.toml"));
    }
    catalog.hooks.sort_by(|left, right| {
        (
            left.kind,
            source_priority(left.source),
            &left.name,
            &left.program,
        )
            .cmp(&(
                right.kind,
                source_priority(right.source),
                &right.name,
                &right.program,
            ))
    });
    catalog
}

fn discover_agents(catalog: &mut ExtensionCatalog, source: ExtensionSource, directory: &Path) {
    for path in sorted_files(directory, "toml", &mut catalog.issues, "agents") {
        match parse_agent(&path, source) {
            Ok(profile) => {
                let replace_builtin = catalog
                    .agents
                    .get(&profile.name)
                    .is_some_and(|existing| existing.source == ExtensionSource::Builtin);
                if replace_builtin || !catalog.agents.contains_key(&profile.name) {
                    catalog.agents.insert(profile.name.clone(), profile);
                }
            }
            Err(error) => catalog.issues.push(DiscoveryIssue {
                mechanism: "agents".to_owned(),
                path,
                message: error.to_string(),
            }),
        }
    }
}

fn discover_skills(
    catalog: &mut ExtensionCatalog,
    source: ExtensionSource,
    directory: &Path,
    builtin_names: &BTreeSet<String>,
) {
    let mut directories = match fs::read_dir(directory) {
        Ok(entries) => entries.filter_map(Result::ok).collect::<Vec<_>>(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            catalog.issues.push(DiscoveryIssue {
                mechanism: "skills".to_owned(),
                path: directory.to_path_buf(),
                message: error.to_string(),
            });
            return;
        }
    };
    directories.sort_by_key(fs::DirEntry::file_name);
    for entry in directories {
        let path = entry.path().join("SKILL.md");
        if !path.is_file() {
            continue;
        }
        match parse_skill(&path, source) {
            Ok(skill) => {
                if !builtin_names.contains(&skill.name) && !catalog.skills.contains_key(&skill.name)
                {
                    catalog.skills.insert(skill.name.clone(), skill);
                }
            }
            Err(error) => catalog.issues.push(DiscoveryIssue {
                mechanism: "skills".to_owned(),
                path,
                message: error.to_string(),
            }),
        }
    }
}

fn discover_text_extensions(
    target: &mut BTreeMap<String, TextExtension>,
    issues: &mut Vec<DiscoveryIssue>,
    mechanism: &str,
    source: ExtensionSource,
    directory: &Path,
) {
    for path in sorted_files(directory, "md", issues, mechanism) {
        let Some(name) = path
            .file_stem()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        if target.contains_key(&name) {
            continue;
        }
        match read_bounded_text(&path) {
            Ok(content) => {
                target.insert(
                    name.clone(),
                    TextExtension {
                        name,
                        content: content.trim().to_owned(),
                        source,
                        path,
                    },
                );
            }
            Err(error) => issues.push(DiscoveryIssue {
                mechanism: mechanism.to_owned(),
                path,
                message: error.to_string(),
            }),
        }
    }
}

fn discover_hooks(catalog: &mut ExtensionCatalog, source: ExtensionSource, path: &Path) {
    let contents = match read_bounded_text(path) {
        Ok(contents) => contents,
        Err(ExtensionError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return;
        }
        Err(error) => {
            catalog.issues.push(DiscoveryIssue {
                mechanism: "hooks".to_owned(),
                path: path.to_path_buf(),
                message: error.to_string(),
            });
            return;
        }
    };
    let parsed = match contents.parse::<Table>() {
        Ok(parsed) => parsed,
        Err(error) => {
            catalog.issues.push(DiscoveryIssue {
                mechanism: "hooks".to_owned(),
                path: path.to_path_buf(),
                message: error.to_string(),
            });
            return;
        }
    };
    let Some(hooks) = parsed.get("hooks").and_then(toml::Value::as_array) else {
        catalog.issues.push(DiscoveryIssue {
            mechanism: "hooks".to_owned(),
            path: path.to_path_buf(),
            message: "hooks.toml must contain [[hooks]] entries".to_owned(),
        });
        return;
    };
    for (index, value) in hooks.iter().enumerate() {
        match parse_hook(value, source) {
            Ok(hook) => catalog.hooks.push(hook),
            Err(error) => catalog.issues.push(DiscoveryIssue {
                mechanism: "hooks".to_owned(),
                path: path.to_path_buf(),
                message: format!("hook {index}: {error}"),
            }),
        }
    }
}

fn parse_agent(path: &Path, source: ExtensionSource) -> Result<AgentProfile, ExtensionError> {
    let contents = read_bounded_text(path)?;
    let mut table = contents
        .parse::<Table>()
        .map_err(|source| ExtensionError::InvalidToml {
            path: path.to_path_buf(),
            source,
        })?;
    migrate_agent_table(&mut table);
    let name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ExtensionError::InvalidName(path.to_path_buf()))?
        .to_owned();
    let display_name =
        take_string(&mut table, "display_name").unwrap_or_else(|| title_from_name(&name));
    let description = take_string(&mut table, "description").unwrap_or_default();
    let safety = take_string(&mut table, "safety").unwrap_or_else(|| "neutral".to_owned());
    let kind = match take_string(&mut table, "agent_type")
        .unwrap_or_else(|| "agent".to_owned())
        .as_str()
    {
        "agent" => AgentKind::Agent,
        "subagent" => AgentKind::Subagent,
        value => return Err(ExtensionError::InvalidAgentKind(value.to_owned())),
    };
    Ok(AgentProfile {
        name,
        display_name,
        description,
        kind,
        safety,
        overrides: table,
        source,
        path: Some(path.to_path_buf()),
    })
}

fn migrate_agent_table(table: &mut Table) {
    if let Some(legacy) = table.remove("base_disabled_tools")
        && !table.contains_key("disabled_tools")
    {
        table.insert("disabled_tools".to_owned(), legacy);
    }
}

fn parse_skill(path: &Path, source: ExtensionSource) -> Result<SkillDefinition, ExtensionError> {
    let contents = read_bounded_text(path)?;
    let content = contents.trim_start_matches('\u{feff}');
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Err(ExtensionError::InvalidSkill(
            "frontmatter must start with `---`".to_owned(),
        ));
    }
    let mut metadata = BTreeMap::new();
    let mut ended = false;
    for line in &mut lines {
        if line.trim() == "---" {
            ended = true;
            break;
        }
        let (key, value) = line.split_once(':').ok_or_else(|| {
            ExtensionError::InvalidSkill(format!("invalid frontmatter line `{line}`"))
        })?;
        metadata.insert(
            key.trim().to_owned(),
            value.trim().trim_matches(['"', '\'']).to_owned(),
        );
    }
    if !ended {
        return Err(ExtensionError::InvalidSkill(
            "frontmatter has no closing `---`".to_owned(),
        ));
    }
    let name = metadata
        .remove("name")
        .filter(|name| valid_extension_name(name))
        .ok_or_else(|| ExtensionError::InvalidSkill("valid `name` is required".to_owned()))?;
    let description = metadata.remove("description").unwrap_or_default();
    let user_invocable = metadata
        .remove("user_invocable")
        .map(|value| value != "false")
        .unwrap_or(true);
    Ok(SkillDefinition {
        name,
        description,
        user_invocable,
        body: lines.collect::<Vec<_>>().join("\n").trim().to_owned(),
        source,
        path: path.to_path_buf(),
    })
}

fn parse_hook(value: &toml::Value, source: ExtensionSource) -> Result<HookSpec, ExtensionError> {
    let table = value
        .as_table()
        .ok_or_else(|| ExtensionError::InvalidHook("entry must be a table".to_owned()))?;
    let required = |key: &str| {
        table
            .get(key)
            .and_then(toml::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| ExtensionError::InvalidHook(format!("`{key}` is required")))
    };
    let name = required("name")?;
    let kind = match required("type")?.as_str() {
        "pre_tool" => HookKind::PreTool,
        "post_tool" => HookKind::PostTool,
        "post_agent" => HookKind::PostAgent,
        value => {
            return Err(ExtensionError::InvalidHook(format!(
                "unknown type `{value}`"
            )));
        }
    };
    let program = PathBuf::from(required("program")?);
    let args = table
        .get("args")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .map(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| ExtensionError::InvalidHook("args must be strings".to_owned()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let timeout_ms = table
        .get("timeout_ms")
        .and_then(toml::Value::as_integer)
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(60_000);
    if timeout_ms == 0 || timeout_ms > 300_000 {
        return Err(ExtensionError::InvalidHook(
            "timeout_ms must be between 1 and 300000".to_owned(),
        ));
    }
    let retries = table
        .get("retries")
        .and_then(toml::Value::as_integer)
        .and_then(|value| u8::try_from(value).ok())
        .unwrap_or_default();
    if retries > 3 {
        return Err(ExtensionError::InvalidHook(
            "retries must be between 0 and 3".to_owned(),
        ));
    }
    let strict = table
        .get("strict")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    if kind == HookKind::PostAgent && strict {
        return Err(ExtensionError::InvalidHook(
            "strict is unavailable for post_agent hooks".to_owned(),
        ));
    }
    Ok(HookSpec {
        name,
        kind,
        program,
        args,
        matcher: table
            .get("match")
            .and_then(toml::Value::as_str)
            .map(ToOwned::to_owned),
        timeout_ms,
        retries,
        strict,
        source,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInjection {
    pub name: String,
    pub content: String,
    pub base_directory: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SkillInjector {
    skills: BTreeMap<String, SkillDefinition>,
    injected: BTreeSet<String>,
}

impl SkillInjector {
    #[must_use]
    pub fn new(skills: BTreeMap<String, SkillDefinition>) -> Self {
        Self {
            skills,
            injected: BTreeSet::new(),
        }
    }

    pub fn invoke(&mut self, name: &str) -> Result<Option<SkillInjection>, ExtensionError> {
        let skill = self
            .skills
            .get(name)
            .ok_or_else(|| ExtensionError::MissingSkill(name.to_owned()))?;
        if !self.injected.insert(name.to_owned()) {
            return Ok(None);
        }
        Ok(Some(SkillInjection {
            name: skill.name.clone(),
            content: skill.body.clone(),
            base_directory: skill
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
        }))
    }
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

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookInvocation {
    pub kind: HookKind,
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub payload: Value,
    pub output_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookNotice {
    pub hook: String,
    pub warning: bool,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookChainResult {
    pub invocation: HookInvocation,
    pub denied: Option<String>,
    pub notices: Vec<HookNotice>,
}

#[derive(Clone)]
pub struct HookManager {
    hooks: Arc<Mutex<Vec<HookSpec>>>,
    working_directory: PathBuf,
}

impl HookManager {
    #[must_use]
    pub fn new(hooks: Vec<HookSpec>, working_directory: PathBuf) -> Self {
        Self {
            hooks: Arc::new(Mutex::new(hooks)),
            working_directory,
        }
    }

    pub fn reload(&self, hooks: Vec<HookSpec>) -> Result<(), ExtensionError> {
        *self
            .hooks
            .lock()
            .map_err(|_| ExtensionError::StatePoisoned)? = hooks;
        Ok(())
    }

    pub async fn run(
        &self,
        mut invocation: HookInvocation,
    ) -> Result<HookChainResult, ExtensionError> {
        let hooks = self
            .hooks
            .lock()
            .map_err(|_| ExtensionError::StatePoisoned)?
            .clone();
        let mut notices = Vec::new();
        let mut denied = None;
        for hook in hooks.into_iter().filter(|hook| {
            hook.kind == invocation.kind
                && hook.matcher.as_deref().is_none_or(|matcher| {
                    invocation
                        .tool_name
                        .as_deref()
                        .is_some_and(|name| matches_wildcard(matcher, name))
                })
        }) {
            let mut attempt = 0_u8;
            let execution = loop {
                let result = execute_hook(&hook, &invocation, &self.working_directory).await;
                if result.is_ok() || attempt >= hook.retries {
                    break result;
                }
                attempt = attempt.saturating_add(1);
            };
            match execution {
                Ok(response) => {
                    if let Some(message) = response.system_message {
                        notices.push(HookNotice {
                            hook: hook.name.clone(),
                            warning: false,
                            content: message,
                        });
                    }
                    if let Some(tool_input) = response.tool_input {
                        invocation.payload = tool_input;
                    }
                    if let Some(additional_context) = response.additional_context {
                        if !invocation.output_text.is_empty() {
                            invocation.output_text.push('\n');
                        }
                        invocation.output_text.push_str(&additional_context);
                    }
                    if response.decision == HookDecision::Deny {
                        denied =
                            Some(response.reason.unwrap_or_else(|| {
                                format!("hook `{}` denied execution", hook.name)
                            }));
                        break;
                    }
                }
                Err(error) => {
                    notices.push(HookNotice {
                        hook: hook.name.clone(),
                        warning: true,
                        content: error.to_string(),
                    });
                    if hook.strict {
                        denied = Some(format!("strict hook `{}` failed", hook.name));
                        break;
                    }
                }
            }
        }
        Ok(HookChainResult {
            invocation,
            denied,
            notices,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookDecision {
    Allow,
    Deny,
}

struct HookResponse {
    decision: HookDecision,
    reason: Option<String>,
    system_message: Option<String>,
    tool_input: Option<Value>,
    additional_context: Option<String>,
}

async fn execute_hook(
    hook: &HookSpec,
    invocation: &HookInvocation,
    working_directory: &Path,
) -> Result<HookResponse, ExtensionError> {
    let stdin = serde_json::to_vec(invocation).map_err(ExtensionError::Json)?;
    let mut command = Command::new(&hook.program);
    command
        .args(&hook.args)
        .current_dir(working_directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|source| ExtensionError::Process {
        program: hook.program.clone(),
        source,
    })?;
    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| ExtensionError::HookProtocol("stdin pipe is unavailable".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExtensionError::HookProtocol("stdout pipe is unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ExtensionError::HookProtocol("stderr pipe is unavailable".to_owned()))?;
    let stdout_task = tokio::spawn(drain_bounded(stdout, MAX_HOOK_OUTPUT_BYTES));
    let stderr_task = tokio::spawn(drain_bounded(stderr, MAX_HOOK_OUTPUT_BYTES));
    child_stdin
        .write_all(&stdin)
        .await
        .map_err(|source| ExtensionError::Process {
            program: hook.program.clone(),
            source,
        })?;
    child_stdin
        .write_all(b"\n")
        .await
        .map_err(|source| ExtensionError::Process {
            program: hook.program.clone(),
            source,
        })?;
    drop(child_stdin);
    let status = match timeout(Duration::from_millis(hook.timeout_ms), child.wait()).await {
        Ok(result) => result.map_err(|source| ExtensionError::Process {
            program: hook.program.clone(),
            source,
        })?,
        Err(_) => {
            child
                .kill()
                .await
                .map_err(|source| ExtensionError::Process {
                    program: hook.program.clone(),
                    source,
                })?;
            let _ = child.wait().await;
            return Err(ExtensionError::HookTimeout(hook.name.clone()));
        }
    };
    let (stdout, stdout_truncated) = stdout_task
        .await
        .map_err(|error| ExtensionError::HookProtocol(error.to_string()))?
        .map_err(|source| ExtensionError::Process {
            program: hook.program.clone(),
            source,
        })?;
    let (stderr, stderr_truncated) = stderr_task
        .await
        .map_err(|error| ExtensionError::HookProtocol(error.to_string()))?
        .map_err(|source| ExtensionError::Process {
            program: hook.program.clone(),
            source,
        })?;
    if stdout_truncated || stderr_truncated {
        return Err(ExtensionError::HookOutputLimit(hook.name.clone()));
    }
    if !status.success() {
        return Err(ExtensionError::HookFailed {
            name: hook.name.clone(),
            status: status.code(),
            stderr: bounded_stderr(&stderr),
        });
    }
    if stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(HookResponse {
            decision: HookDecision::Allow,
            reason: None,
            system_message: None,
            tool_input: None,
            additional_context: None,
        });
    }
    let value: Value = serde_json::from_slice(&stdout)
        .map_err(|error| ExtensionError::HookProtocol(error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| ExtensionError::HookProtocol("response must be an object".to_owned()))?;
    let decision = match object
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("allow")
    {
        "allow" => HookDecision::Allow,
        "deny" => HookDecision::Deny,
        value => {
            return Err(ExtensionError::HookProtocol(format!(
                "unknown decision `{value}`"
            )));
        }
    };
    Ok(HookResponse {
        decision,
        reason: object
            .get("reason")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        system_message: object
            .get("system_message")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        tool_input: object
            .get("hook_specific_output")
            .and_then(Value::as_object)
            .and_then(|output| output.get("tool_input"))
            .cloned(),
        additional_context: object
            .get("hook_specific_output")
            .and_then(Value::as_object)
            .and_then(|output| output.get("additional_context"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}

async fn drain_bounded(
    mut stream: impl AsyncRead + Unpin,
    maximum: usize,
) -> io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = maximum.saturating_sub(retained.len());
        let keep = remaining.min(read);
        retained.extend_from_slice(&buffer[..keep]);
        truncated |= keep < read;
    }
    Ok((retained, truncated))
}

fn bounded_stderr(bytes: &[u8]) -> String {
    truncate_utf8(&String::from_utf8_lossy(bytes), 1024).to_owned()
}

pub type SubagentFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

pub trait SubagentRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        context: ChildContext,
        cancellation: CancellationToken,
    ) -> SubagentFuture<'a>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildLoggingPolicy {
    Full,
    SummaryOnly,
    Disabled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DelegationRequest {
    pub parent_session_id: String,
    pub agent: AgentProfile,
    pub prompt: String,
    pub logging: ChildLoggingPolicy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChildContext {
    pub parent_session_id: String,
    pub child_session_id: String,
    pub depth: u8,
    pub agent: AgentProfile,
    pub prompt: String,
    pub config: BTreeMap<String, Value>,
    pub logging: ChildLoggingPolicy,
    pub working_directory: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStatus {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationEffect {
    pub parent_session_id: String,
    pub child_session_id: String,
    pub public_session_id: String,
    pub status: DelegationStatus,
    pub result: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildActivity {
    pub root_session_id: String,
    pub child_session_id: String,
    pub public_session_id: String,
    pub kind: String,
}

#[derive(Clone)]
pub struct SubagentManager {
    store: SessionStore,
    runner: Arc<dyn SubagentRunner>,
    active: Arc<tokio::sync::Mutex<BTreeMap<String, (String, CancellationToken)>>>,
}

impl SubagentManager {
    #[must_use]
    pub fn new(store: SessionStore, runner: Arc<dyn SubagentRunner>) -> Self {
        Self {
            store,
            runner,
            active: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
        }
    }

    pub async fn delegate(
        &self,
        request: DelegationRequest,
        now_ms: u64,
    ) -> Result<DelegationEffect, ExtensionError> {
        if request.agent.kind != AgentKind::Subagent {
            return Err(ExtensionError::AgentNotSubagent(request.agent.name));
        }
        let parent = self.store.load(&request.parent_session_id)?;
        let parent_depth = parent
            .metadata
            .agent_profile
            .as_ref()
            .and_then(|profile| profile.get("depth"))
            .and_then(Value::as_u64)
            .and_then(|depth| u8::try_from(depth).ok())
            .unwrap_or(0);
        let depth = parent_depth.saturating_add(1);
        if depth > MAX_DELEGATION_DEPTH {
            return Err(ExtensionError::DelegationDepth {
                maximum: MAX_DELEGATION_DEPTH,
            });
        }
        let (child_session_id, mut metadata) = {
            let mut created = None;
            for _ in 0..MAX_CHILD_ID_ATTEMPTS {
                let sequence = CHILD_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let candidate = format!(
                    "child-{now_ms:016x}-{:08x}-{sequence:016x}",
                    std::process::id()
                );
                match self.store.create_child(
                    &candidate,
                    &parent.metadata.working_directory,
                    request.parent_session_id.clone(),
                    now_ms,
                ) {
                    Ok(metadata) => {
                        created = Some((candidate, metadata));
                        break;
                    }
                    Err(StorageError::DuplicateSessionId(_)) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            created.ok_or(ExtensionError::ChildIdExhausted)?
        };
        metadata.config = parent.metadata.config.clone();
        metadata.agent_profile = Some(json!({
            "name": request.agent.name,
            "kind": "subagent",
            "depth": depth,
            "logging": request.logging,
        }));
        self.store.update_metadata(&metadata)?;
        let cancellation = CancellationToken::default();
        self.active.lock().await.insert(
            child_session_id.clone(),
            (request.parent_session_id.clone(), cancellation.clone()),
        );
        let finalizer = DelegationFinalizer::new(
            self.store.clone(),
            self.active.clone(),
            request.parent_session_id.clone(),
            child_session_id.clone(),
            cancellation.clone(),
            now_ms.saturating_add(1),
        );
        let context = ChildContext {
            parent_session_id: request.parent_session_id.clone(),
            child_session_id: child_session_id.clone(),
            depth,
            agent: request.agent,
            prompt: request.prompt,
            config: parent.metadata.config,
            logging: request.logging,
            working_directory: parent.metadata.working_directory,
        };
        let (status, result) = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                (DelegationStatus::Cancelled, "Subagent cancelled".to_owned())
            }
            outcome = timeout(
                MAX_DELEGATION_DURATION,
                self.runner.run(context, cancellation.clone()),
            ) => {
                match outcome {
                    Ok(Ok(result)) => (
                        DelegationStatus::Completed,
                        bounded_utf8(&result, MAX_DELEGATION_RESULT_BYTES, "…[truncated]"),
                    ),
                    Ok(Err(error)) => (
                        DelegationStatus::Failed,
                        bounded_utf8(&error, MAX_DELEGATION_RESULT_BYTES, "…[truncated]"),
                    ),
                    Err(_) => {
                        cancellation.cancel();
                        (DelegationStatus::Failed, "Subagent timed out".to_owned())
                    }
                }
            }
        };
        // Closing the child session is cleanup: its failure is reported with the
        // outcome rather than discarding work the subagent already completed.
        let result = match finalizer.finish().await {
            Ok(()) => result,
            Err(error) => format!("{result}\n\n[child session cleanup failed: {error}]"),
        };
        Ok(DelegationEffect {
            parent_session_id: request.parent_session_id,
            child_session_id: child_session_id.clone(),
            public_session_id: child_session_id,
            status,
            result,
        })
    }

    pub async fn cancel_parent(&self, parent_session_id: &str) {
        for (parent, cancellation) in self.active.lock().await.values() {
            if parent == parent_session_id {
                cancellation.cancel();
            }
        }
    }

    #[must_use]
    pub fn activity(effect: &DelegationEffect, kind: &str) -> ChildActivity {
        ChildActivity {
            root_session_id: effect.parent_session_id.clone(),
            child_session_id: effect.child_session_id.clone(),
            public_session_id: effect.public_session_id.clone(),
            kind: kind.to_owned(),
        }
    }
}

struct DelegationFinalizer {
    store: SessionStore,
    active: Arc<tokio::sync::Mutex<BTreeMap<String, (String, CancellationToken)>>>,
    parent_session_id: String,
    child_session_id: String,
    cancellation: CancellationToken,
    close_at_ms: u64,
    finished: bool,
}

impl DelegationFinalizer {
    fn new(
        store: SessionStore,
        active: Arc<tokio::sync::Mutex<BTreeMap<String, (String, CancellationToken)>>>,
        parent_session_id: String,
        child_session_id: String,
        cancellation: CancellationToken,
        close_at_ms: u64,
    ) -> Self {
        Self {
            store,
            active,
            parent_session_id,
            child_session_id,
            cancellation,
            close_at_ms,
            finished: false,
        }
    }

    async fn finish(mut self) -> Result<(), StorageError> {
        self.active.lock().await.remove(&self.child_session_id);
        self.store.close(&self.child_session_id, self.close_at_ms)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for DelegationFinalizer {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.cancellation.cancel();
        let _ = self.store.close(&self.child_session_id, self.close_at_ms);
        if let Ok(mut active) = self.active.try_lock() {
            active.remove(&self.child_session_id);
            return;
        }
        let active = self.active.clone();
        let child_session_id = self.child_session_id.clone();
        let parent_session_id = self.parent_session_id.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut active = active.lock().await;
                if active
                    .get(&child_session_id)
                    .is_some_and(|(parent, _)| parent == &parent_session_id)
                {
                    active.remove(&child_session_id);
                }
            });
        }
    }
}

fn sorted_files(
    directory: &Path,
    extension: &str,
    issues: &mut Vec<DiscoveryIssue>,
    mechanism: &str,
) -> Vec<PathBuf> {
    let mut files = match fs::read_dir(directory) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path.extension().and_then(|value| value.to_str()) == Some(extension)
            })
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            issues.push(DiscoveryIssue {
                mechanism: mechanism.to_owned(),
                path: directory.to_path_buf(),
                message: error.to_string(),
            });
            return Vec::new();
        }
    };
    files.sort();
    files
}

fn read_bounded_text(path: &Path) -> Result<String, ExtensionError> {
    let metadata = fs::metadata(path).map_err(|source| ExtensionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_EXTENSION_FILE_BYTES {
        return Err(ExtensionError::FileTooLarge(path.to_path_buf()));
    }
    fs::read_to_string(path).map_err(|source| ExtensionError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn take_string(table: &mut Table, key: &str) -> Option<String> {
    table
        .remove(key)
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
}

fn title_from_name(name: &str) -> String {
    let mut title = String::new();
    for (index, part) in name.split('-').enumerate() {
        if index > 0 {
            title.push(' ');
        }
        let mut characters = part.chars();
        if let Some(first) = characters.next() {
            title.extend(first.to_uppercase());
            title.extend(characters);
        }
    }
    title
}

fn valid_extension_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

const fn source_priority(source: ExtensionSource) -> u8 {
    match source {
        ExtensionSource::Builtin => 0,
        ExtensionSource::Configured => 1,
        ExtensionSource::Project => 2,
        ExtensionSource::User => 3,
    }
}

#[derive(Debug, Error)]
pub enum ExtensionError {
    #[error("extension state lock is poisoned")]
    StatePoisoned,
    #[error("extension file exceeds the 2 MiB limit: `{0}`")]
    FileTooLarge(PathBuf),
    #[error("extension I/O failed at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid TOML at `{path}`")]
    InvalidToml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("extension filename is invalid: `{0}`")]
    InvalidName(PathBuf),
    #[error("invalid agent type `{0}`")]
    InvalidAgentKind(String),
    #[error("invalid skill: {0}")]
    InvalidSkill(String),
    #[error("invalid hook: {0}")]
    InvalidHook(String),
    #[error("skill `{0}` was not found")]
    MissingSkill(String),
    #[error("agent `{0}` was not found")]
    MissingAgent(String),
    #[error("subagent `{0}` cannot be selected as the primary agent")]
    SubagentCannotBePrimary(String),
    #[error("agent `{0}` is not owned by the user install directory")]
    AgentNotUserOwned(String),
    #[error("agent `{0}` is not a subagent")]
    AgentNotSubagent(String),
    #[error("delegation depth exceeds the maximum of {maximum}")]
    DelegationDepth { maximum: u8 },
    #[error("could not allocate a unique child session ID")]
    ChildIdExhausted,
    #[error("hook `{0}` timed out")]
    HookTimeout(String),
    #[error("hook `{0}` exceeded its output limit")]
    HookOutputLimit(String),
    #[error("hook `{name}` failed with status {status:?}: {stderr}")]
    HookFailed {
        name: String,
        status: Option<i32>,
        stderr: String,
    },
    #[error("hook protocol error: {0}")]
    HookProtocol(String),
    #[error("process `{program}` failed: {source}")]
    Process {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("JSON serialization failed: {0}")]
    Json(serde_json::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin_agent(name: &str, kind: AgentKind) -> AgentProfile {
        AgentProfile {
            name: name.to_owned(),
            display_name: title_from_name(name),
            description: String::new(),
            kind,
            safety: "neutral".to_owned(),
            overrides: Table::new(),
            source: ExtensionSource::Builtin,
            path: None,
        }
    }

    /// The rename moved the profile vocabulary onto the reference names, and
    /// the permission scope each one maps to has to survive that move.
    ///
    /// US-105 replaced the invented scope strings with the four reference
    /// scopes, so a file tool's path glob answers for `outside_directory` and
    /// every other tool's entry for `command_pattern`.
    #[test]
    fn reference_tool_names_keep_the_permission_scope_the_invented_names_produced() {
        for tool in ["read_file", "grep", "edit", "write_file"] {
            assert_eq!(
                profile_permission_scope(tool),
                PermissionScope::OutsideDirectory,
                "`{tool}` allowlists paths"
            );
        }

        // Nothing rewrites the reference file-tool names any more.
        assert_eq!(canonical_tool_name("read_file"), "read_file");
        assert_eq!(canonical_tool_name("grep"), "grep");
        // `bash` is the published name now, so a profile naming it must reach
        // the tool the registry serves rather than the manual shell resource.
        assert_eq!(canonical_tool_name("bash"), "bash");
        assert_eq!(
            profile_permission_scope("bash"),
            PermissionScope::CommandPattern
        );
    }

    /// `_plan_overrides` and the accept-edits profile both name `write_file`
    /// and `edit`, so auto-approval has to resolve against those names.
    #[test]
    fn auto_approval_resolves_against_the_reference_mutating_tool_names() {
        let always = |tool: &str| {
            Table::from_iter([(
                "tools".to_owned(),
                toml::Value::Table(Table::from_iter([(
                    tool.to_owned(),
                    toml::Value::Table(Table::from_iter([(
                        "permission".to_owned(),
                        toml::Value::String("always".to_owned()),
                    )])),
                )])),
            )])
        };

        assert!(auto_approves_edits(&always("edit")));
        assert!(auto_approves_edits(&always("write_file")));
        assert!(!auto_approves_edits(&always("read_file")));
        assert!(!auto_approves_edits(&always("grep")));
    }

    #[test]
    fn agent_runtime_settings_resolve_profile_policy_without_name_conventions() {
        let mut profile = builtin_agent("custom-reviewer", AgentKind::Agent);
        profile.overrides.insert(
            "enabled_tools".to_owned(),
            toml::Value::Array(vec![
                toml::Value::String("read_file".to_owned()),
                toml::Value::String("write_file".to_owned()),
            ]),
        );
        profile.overrides.insert(
            "disabled_tools".to_owned(),
            toml::Value::Array(vec![toml::Value::String("grep".to_owned())]),
        );
        profile.overrides.insert(
            "tools".to_owned(),
            toml::Value::Table(Table::from_iter([(
                "write_file".to_owned(),
                toml::Value::Table(Table::from_iter([(
                    "permission".to_owned(),
                    toml::Value::String("always".to_owned()),
                )])),
            )])),
        );
        profile.overrides.insert(
            "models".to_owned(),
            toml::Value::Array(vec![toml::Value::Table(Table::from_iter([
                ("alias".to_owned(), toml::Value::String("review".to_owned())),
                (
                    "name".to_owned(),
                    toml::Value::String("mistral-review".to_owned()),
                ),
                (
                    "thinking".to_owned(),
                    toml::Value::String("high".to_owned()),
                ),
            ]))]),
        );
        profile.overrides.insert(
            "active_model".to_owned(),
            toml::Value::String("review".to_owned()),
        );
        profile
            .overrides
            .insert("mode".to_owned(), toml::Value::String("plan".to_owned()));

        let settings = profile.runtime_settings();

        // `read_file` and `grep` are published verbatim now, so a profile
        // naming them resolves to itself rather than to an invented local name.
        assert_eq!(settings.enabled_tools, ["read_file", "edit"]);
        assert_eq!(settings.disabled_tools, ["grep"]);
        assert_eq!(settings.approval, AgentApproval::Edits);
        assert_eq!(settings.model.as_deref(), Some("mistral-review"));
        assert_eq!(settings.thinking, Some(true));
        assert_eq!(settings.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(settings.mode.as_deref(), Some("plan"));
        assert_eq!(settings.system_prompt_id, None);
    }

    #[test]
    fn agent_runtime_settings_enforce_never_permissions_and_prompt_selection() {
        let mut profile = builtin_agent("planner", AgentKind::Agent);
        profile.overrides.insert(
            "tools".to_owned(),
            toml::Value::Table(Table::from_iter([(
                "write_file".to_owned(),
                toml::Value::Table(Table::from_iter([(
                    "permission".to_owned(),
                    toml::Value::String("never".to_owned()),
                )])),
            )])),
        );
        profile.overrides.insert(
            "system_prompt_id".to_owned(),
            toml::Value::String("plan".to_owned()),
        );

        let settings = profile.runtime_settings();

        assert_eq!(settings.disabled_tools, ["edit"]);
        assert_eq!(settings.system_prompt_id.as_deref(), Some("plan"));
    }

    #[test]
    fn agent_runtime_settings_preserve_never_policy_allowlist_exceptions() {
        let mut profile = builtin_agent("planner", AgentKind::Agent);
        profile.overrides.insert(
            "tools".to_owned(),
            toml::Value::Table(Table::from_iter([(
                "write_file".to_owned(),
                toml::Value::Table(Table::from_iter([
                    (
                        "permission".to_owned(),
                        toml::Value::String("never".to_owned()),
                    ),
                    (
                        "allowlist".to_owned(),
                        toml::Value::Array(vec![toml::Value::String(
                            "/workspace/plans/*".to_owned(),
                        )]),
                    ),
                ])),
            )])),
        );

        let settings = profile.runtime_settings();

        assert!(settings.disabled_tools.is_empty());
        assert!(settings.permission_rules.iter().any(|rule| {
            rule.tool == "edit" && rule.scope.is_none() && rule.mode == PermissionMode::Never
        }));
        assert!(settings.permission_rules.iter().any(|rule| {
            rule.tool == "edit"
                && rule.scope == Some(PermissionScope::OutsideDirectory)
                && rule.pattern == "/workspace/plans/*"
                && rule.mode == PermissionMode::Always
        }));
    }

    #[test]
    fn allowlisted_edit_approval_does_not_expand_to_every_path() {
        let mut profile = builtin_agent("scoped-editor", AgentKind::Agent);
        profile.overrides.insert(
            "tools".to_owned(),
            toml::Value::Table(Table::from_iter([(
                "edit".to_owned(),
                toml::Value::Table(Table::from_iter([
                    (
                        "permission".to_owned(),
                        toml::Value::String("always".to_owned()),
                    ),
                    (
                        "allowlist".to_owned(),
                        toml::Value::Array(vec![toml::Value::String(
                            "/workspace/generated/*".to_owned(),
                        )]),
                    ),
                ])),
            )])),
        );

        let settings = profile.runtime_settings();

        assert_eq!(settings.approval, AgentApproval::Prompt);
        assert_eq!(settings.permission_rules.len(), 1);
        assert_eq!(
            settings.permission_rules[0].scope,
            Some(PermissionScope::OutsideDirectory)
        );
        assert_eq!(
            settings.permission_rules[0].pattern,
            "/workspace/generated/*"
        );
    }

    #[test]
    fn discovery_is_deterministic_first_wins_and_untrusted_project_is_excluded() {
        let temporary = tempfile::tempdir().expect("temporary roots");
        let configured = temporary.path().join("configured");
        let project = temporary.path().join("project");
        let user = temporary.path().join("user");
        for root in [&configured, &project, &user] {
            fs::create_dir_all(root.join("agents")).expect("agent directory");
            fs::create_dir_all(root.join("skills/probe")).expect("skill directory");
        }
        fs::write(
            configured.join("agents/default.toml"),
            "description = \"configured\"\nagent_type = \"agent\"\n",
        )
        .expect("configured agent");
        fs::write(
            project.join("agents/project.toml"),
            "description = \"project\"\n",
        )
        .expect("project agent");
        fs::write(user.join("agents/default.toml"), "description = \"user\"\n")
            .expect("user agent");
        fs::write(
            configured.join("skills/probe/SKILL.md"),
            "---\nname: probe\ndescription: configured\n---\nconfigured body",
        )
        .expect("configured skill");
        fs::write(
            user.join("skills/probe/SKILL.md"),
            "---\nname: probe\ndescription: user\n---\nuser body",
        )
        .expect("user skill");
        let roots = DiscoveryRoots {
            configured: vec![configured],
            project: vec![project],
            user: vec![user],
            project_trusted: false,
        };
        let catalog = discover_extensions(
            &roots,
            BTreeMap::from([(
                "default".to_owned(),
                builtin_agent("default", AgentKind::Agent),
            )]),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        assert_eq!(catalog.agents["default"].description, "configured");
        assert!(!catalog.agents.contains_key("project"));
        assert_eq!(catalog.skills["probe"].body, "configured body");
    }

    #[test]
    fn malformed_entries_are_reported_without_stopping_safe_mechanisms() {
        let temporary = tempfile::tempdir().expect("temporary roots");
        let user = temporary.path().join("user");
        fs::create_dir_all(user.join("agents")).expect("agent directory");
        fs::create_dir_all(user.join("skills/good")).expect("skill directory");
        fs::create_dir_all(user.join("skills/bad")).expect("skill directory");
        fs::write(user.join("agents/broken.toml"), "broken = [").expect("bad agent");
        fs::write(
            user.join("skills/good/SKILL.md"),
            "---\nname: good\ndescription: valid\n---\nbody",
        )
        .expect("good skill");
        fs::write(user.join("skills/bad/SKILL.md"), "no frontmatter").expect("bad skill");
        fs::write(user.join("hooks.toml"), "[[hooks]]\nname = \"bad\"\n").expect("bad hook");
        let catalog = discover_extensions(
            &DiscoveryRoots {
                user: vec![user],
                ..DiscoveryRoots::default()
            },
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        assert!(catalog.skills.contains_key("good"));
        assert_eq!(catalog.issues.len(), 3);
    }

    #[tokio::test]
    async fn hooks_apply_typed_rewrite_replacement_and_failure_isolation() {
        let temporary = tempfile::tempdir().expect("temporary root");
        #[cfg(unix)]
        let (program, args) = (
            PathBuf::from("/bin/sh"),
            vec![
                "-c".to_owned(),
                "printf '%s' '{\"decision\":\"allow\",\"system_message\":\"notice\",\"hook_specific_output\":{\"tool_input\":{\"rewritten\":true},\"additional_context\":\"extra\"}}'"
                    .to_owned(),
            ],
        );
        #[cfg(windows)]
        let (program, args) = (
            PathBuf::from("cmd.exe"),
            vec![
                "/C".to_owned(),
                "echo {\"decision\":\"allow\",\"system_message\":\"notice\",\"hook_specific_output\":{\"tool_input\":{\"rewritten\":true},\"additional_context\":\"extra\"}}"
                    .to_owned(),
            ],
        );
        let manager = HookManager::new(
            vec![
                HookSpec {
                    name: "rewrite".to_owned(),
                    kind: HookKind::PreTool,
                    program,
                    args,
                    matcher: Some("read_*".to_owned()),
                    timeout_ms: 1_000,
                    retries: 0,
                    strict: false,
                    source: ExtensionSource::User,
                },
                HookSpec {
                    name: "missing".to_owned(),
                    kind: HookKind::PreTool,
                    program: temporary.path().join("missing"),
                    args: Vec::new(),
                    matcher: Some("*".to_owned()),
                    timeout_ms: 100,
                    retries: 1,
                    strict: false,
                    source: ExtensionSource::User,
                },
            ],
            temporary.path().to_path_buf(),
        );
        let result = manager
            .run(HookInvocation {
                kind: HookKind::PreTool,
                session_id: "session".to_owned(),
                parent_session_id: None,
                tool_name: Some("read_file".to_owned()),
                tool_call_id: Some("call".to_owned()),
                payload: json!({"old": true}),
                output_text: "base".to_owned(),
            })
            .await
            .expect("hook chain completes");
        assert_eq!(result.invocation.payload, json!({"rewritten": true}));
        assert_eq!(result.invocation.output_text, "base\nextra");
        assert_eq!(result.notices.len(), 2);
        assert!(result.denied.is_none());
    }

    struct FakeSubagent;

    impl SubagentRunner for FakeSubagent {
        fn run<'a>(
            &'a self,
            context: ChildContext,
            _cancellation: CancellationToken,
        ) -> SubagentFuture<'a> {
            Box::pin(async move { Ok(format!("{}:{}", context.agent.name, context.prompt)) })
        }
    }

    struct HangingSubagent {
        entered: Arc<tokio::sync::Notify>,
    }

    impl SubagentRunner for HangingSubagent {
        fn run<'a>(
            &'a self,
            _context: ChildContext,
            _cancellation: CancellationToken,
        ) -> SubagentFuture<'a> {
            Box::pin(async move {
                self.entered.notify_one();
                std::future::pending().await
            })
        }
    }

    #[tokio::test]
    async fn delegation_uses_independent_child_sessions_and_bounded_depth() {
        let temporary = tempfile::tempdir().expect("temporary sessions");
        let store = SessionStore::new(temporary.path());
        let mut parent = store
            .create("parent", "/workspace", None, 1)
            .expect("parent session");
        parent.config.insert("model".to_owned(), json!("child"));
        store.update_metadata(&parent).expect("parent config");
        let manager = SubagentManager::new(store.clone(), Arc::new(FakeSubagent));
        let agent = builtin_agent("explore", AgentKind::Subagent);
        let effect = manager
            .delegate(
                DelegationRequest {
                    parent_session_id: "parent".to_owned(),
                    agent: agent.clone(),
                    prompt: "inspect".to_owned(),
                    logging: ChildLoggingPolicy::SummaryOnly,
                },
                10,
            )
            .await
            .expect("delegation completes");
        assert_eq!(effect.status, DelegationStatus::Completed);
        assert_ne!(effect.child_session_id, effect.parent_session_id);
        let child = store
            .load(&effect.child_session_id)
            .expect("child persisted");
        assert_eq!(child.metadata.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(child.metadata.config["model"], "child");
        assert_eq!(
            SubagentManager::activity(&effect, "tool").root_session_id,
            "parent"
        );
        let second = manager
            .delegate(
                DelegationRequest {
                    parent_session_id: "parent".to_owned(),
                    agent: agent.clone(),
                    prompt: "inspect again".to_owned(),
                    logging: ChildLoggingPolicy::SummaryOnly,
                },
                10,
            )
            .await
            .expect("same-millisecond delegation completes");
        assert_ne!(effect.child_session_id, second.child_session_id);
        assert_eq!(
            store
                .list(None, 0, 10)
                .expect("distinct child directories")
                .sessions
                .iter()
                .filter(|session| session.parent_session_id.as_deref() == Some("parent"))
                .count(),
            2
        );

        let mut parent = store.load("parent").expect("parent loads").metadata;
        parent.agent_profile = Some(json!({"depth": MAX_DELEGATION_DEPTH}));
        store.update_metadata(&parent).expect("parent depth");
        assert!(matches!(
            manager
                .delegate(
                    DelegationRequest {
                        parent_session_id: "parent".to_owned(),
                        agent,
                        prompt: "recursive".to_owned(),
                        logging: ChildLoggingPolicy::Disabled,
                    },
                    20,
                )
                .await,
            Err(ExtensionError::DelegationDepth { .. })
        ));
    }

    #[tokio::test]
    async fn parent_cancellation_forces_child_finalization() {
        let temporary = tempfile::tempdir().expect("temporary sessions");
        let store = SessionStore::new(temporary.path());
        store
            .create("parent", "/workspace", None, 1)
            .expect("parent session");
        let entered = Arc::new(tokio::sync::Notify::new());
        let manager = SubagentManager::new(
            store.clone(),
            Arc::new(HangingSubagent {
                entered: entered.clone(),
            }),
        );
        let delegation = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .delegate(
                        DelegationRequest {
                            parent_session_id: "parent".to_owned(),
                            agent: builtin_agent("explore", AgentKind::Subagent),
                            prompt: "wait".to_owned(),
                            logging: ChildLoggingPolicy::SummaryOnly,
                        },
                        10,
                    )
                    .await
            }
        });
        entered.notified().await;
        manager.cancel_parent("parent").await;
        let effect = tokio::time::timeout(Duration::from_millis(250), delegation)
            .await
            .expect("delegation cancellation")
            .expect("delegation task")
            .expect("delegation effect");
        assert_eq!(effect.status, DelegationStatus::Cancelled);
        assert!(
            store
                .load(&effect.child_session_id)
                .expect("child remains auditable")
                .metadata
                .end_time
                .is_some()
        );
        assert!(manager.active.lock().await.is_empty());
    }
}
