use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, io};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml::Table;

use crate::skills::parser::parse_skill_markdown;
use crate::skills::schema::SkillMetadata;
use crate::skills::{SkillDiscovery, SkillScope, SkillSource};
use crate::storage::StorageError;

mod agents;
mod hooks;
mod subagents;

#[cfg(test)]
use agents::{auto_approves_edits, canonical_tool_name, profile_permission_scope};

pub use agents::{AgentApproval, AgentKind, AgentProfile, AgentRegistry, AgentRuntimeSettings};
pub use hooks::{HookChainResult, HookInvocation, HookManager, HookNotice};
pub use subagents::{
    ChildActivity, ChildContext, ChildLoggingPolicy, DelegationEffect, DelegationRequest,
    DelegationStatus, SubagentFuture, SubagentManager, SubagentRun, SubagentRunner,
};

const MAX_EXTENSION_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_HOOK_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_DELEGATION_RESULT_BYTES: usize = 64 * 1024;
const MAX_DELEGATION_DEPTH: u8 = 3;
const MAX_DELEGATION_DURATION: Duration = Duration::from_secs(60);
const MAX_CHILD_ID_ATTEMPTS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionSource {
    Builtin,
    Configured,
    Project,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub allowed_tools: Vec<String>,
    pub user_invocable: bool,
    pub body: String,
    pub source: SkillSource,
    pub scope: SkillScope,
    /// The resolved absolute path of the `SKILL.md` on disk, absent for a
    /// skill that ships without one; serialization omits it rather than
    /// spelling an empty path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
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

impl HookKind {
    /// The kind as a hook file writes it, which is also what a hook span names
    /// itself after. Reference `hook_span(hook_type=...)`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PreTool => "pre_tool",
            Self::PostTool => "post_tool",
            Self::PostAgent => "post_agent",
        }
    }
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
    /// Where skills come from, which is not `{root}/skills` for any of the
    /// roots above: the reference reads five directories that do not share a
    /// parent, so [`crate::skills::search_paths`] resolves them and the trust
    /// gate is applied there rather than by [`DiscoveryRoots::ordered`].
    pub skills: SkillDiscovery,
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

    // The skill roots are their own ordered list rather than a subdirectory of
    // each extension root, and they are walked before the rest so precedence
    // reads in one place.
    for directory in &roots.skills.roots {
        discover_skills(&mut catalog, directory, &builtin_skill_names);
    }
    crate::skills::apply_filters(&mut catalog.skills, &roots.skills);

    for (source, root) in roots.ordered() {
        discover_agents(&mut catalog, source, &root.join("agents"));
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
        match parse_skill(&path) {
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

pub(super) fn parse_agent(
    path: &Path,
    source: ExtensionSource,
) -> Result<AgentProfile, ExtensionError> {
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

fn parse_skill(path: &Path) -> Result<SkillDefinition, ExtensionError> {
    let contents = read_bounded_text(path)?;
    let (frontmatter, body) = parse_skill_markdown(&contents)
        .map_err(|error| ExtensionError::InvalidSkill(error.to_string()))?;
    let metadata = SkillMetadata::validate(&frontmatter)
        .map_err(|error| ExtensionError::InvalidSkill(error.to_string()))?;
    // A frontmatter name that differs from the directory name is a log-only
    // warning upstream, never a rejection or a diagnostic: the frontmatter
    // name wins and nothing else is observable.
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Ok(SkillDefinition {
        name: metadata.name,
        description: metadata.description,
        license: metadata.license,
        compatibility: metadata.compatibility,
        metadata: metadata.metadata,
        allowed_tools: metadata.allowed_tools,
        user_invocable: metadata.user_invocable,
        body: body.trim().to_owned(),
        source: SkillSource::Local,
        scope: SkillScope::Global,
        path: Some(resolved),
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
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_default(),
        }))
    }
}

impl crate::tracing::TracedError for ExtensionError {
    fn error_type(&self) -> &'static str {
        "ExtensionError"
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

pub(super) const fn source_priority(source: ExtensionSource) -> u8 {
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
    use serde_json::json;
    use std::sync::Arc;

    use super::agents::AgentKind;
    use super::hooks::{HookInvocation, HookManager};
    use super::subagents::{
        ChildContext, ChildLoggingPolicy, DelegationRequest, DelegationStatus, SubagentFuture,
        SubagentManager, SubagentRun, SubagentRunner,
    };
    use crate::engine::CancellationToken;
    use crate::policy::{PermissionMode, PermissionScope};
    use crate::storage::SessionStore;

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
        // The skill roots are their own ordered list: the agent roots above
        // decide nothing about where a skill is read from.
        let skill_roots = crate::skills::SkillDiscovery {
            roots: vec![configured.join("skills"), user.join("skills")],
            ..crate::skills::SkillDiscovery::default()
        };
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
            skills: skill_roots,
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
                user: vec![user.clone()],
                skills: crate::skills::SkillDiscovery {
                    roots: vec![user.join("skills")],
                    ..crate::skills::SkillDiscovery::default()
                },
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
                    // Generous, because this hook is measured for the rewrite
                    // it applies and not for a deadline: a short budget makes
                    // the assertion read the spawn latency of a loaded machine
                    // rather than the chain's semantics. The `missing` hook
                    // below keeps a short one, which is what times out on
                    // purpose.
                    timeout_ms: 30_000,
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

    /// A hook that closes its stdin without reading it is still honored.
    ///
    /// Reference `_run_process` swallows `BrokenPipeError` and
    /// `ConnectionResetError` around the stdin write, because a hook answers
    /// through its stdout and its exit status and is under no obligation to
    /// consume the invocation. The payload here is deliberately larger than a
    /// pipe buffer, so the write cannot quietly fit into a buffer nobody reads:
    /// it reaches the closed read end every time, on every machine.
    ///
    /// POSIX only: the case is built on `exec 0<&-`, and the behavior under
    /// test is the parent's, not the shell's.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hook_that_never_reads_its_stdin_is_still_honored() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let manager = HookManager::new(
            vec![HookSpec {
                name: "deaf".to_owned(),
                kind: HookKind::PreTool,
                program: PathBuf::from("/bin/sh"),
                args: vec![
                    "-c".to_owned(),
                    "exec 0<&-; printf '%s' \
                     '{\"decision\":\"allow\",\"hook_specific_output\":{\"tool_input\":{\"rewritten\":true}}}'"
                        .to_owned(),
                ],
                matcher: Some("*".to_owned()),
                timeout_ms: 30_000,
                retries: 0,
                strict: false,
                source: ExtensionSource::User,
            }],
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
                // Past any platform's pipe buffer, so the write has to reach
                // the read end the hook already closed.
                output_text: "x".repeat(256 * 1024),
            })
            .await
            .expect("the hook chain completes");
        assert_eq!(
            result.invocation.payload,
            json!({"rewritten": true}),
            "a broken stdin pipe swallowed the hook's rewrite: {:?}",
            result.notices
        );
        assert!(result.notices.is_empty(), "{:?}", result.notices);
        assert!(result.denied.is_none());
    }

    /// US-016: one hook run is one span, named after the kind and the hook and
    /// carrying the tool it guards. Reference `hooks/manager.py` opens
    /// `hook_span` around the run, so a chain of two hooks is two spans.
    #[tokio::test]
    async fn every_hook_run_opens_its_own_span() {
        let _exclusive = crate::tracing::harness::exclusive();
        let harness = crate::tracing::harness::Harness::install();
        let temporary = tempfile::tempdir().expect("temporary root");
        #[cfg(unix)]
        let (program, args) = (
            PathBuf::from("/bin/sh"),
            vec![
                "-c".to_owned(),
                "printf '%s' '{\"decision\":\"allow\"}'".to_owned(),
            ],
        );
        #[cfg(windows)]
        let (program, args) = (
            PathBuf::from("cmd.exe"),
            vec!["/C".to_owned(), "echo {\"decision\":\"allow\"}".to_owned()],
        );
        let manager = HookManager::new(
            vec![HookSpec {
                name: "guard".to_owned(),
                kind: HookKind::PreTool,
                program,
                args,
                matcher: Some("read_*".to_owned()),
                timeout_ms: 30_000,
                retries: 0,
                strict: false,
                source: ExtensionSource::User,
            }],
            temporary.path().to_path_buf(),
        );
        manager
            .run(HookInvocation {
                kind: HookKind::PreTool,
                session_id: "session".to_owned(),
                parent_session_id: None,
                tool_name: Some("read_file".to_owned()),
                tool_call_id: Some("call".to_owned()),
                payload: json!({}),
                output_text: String::new(),
            })
            .await
            .expect("hook chain completes");
        let spans = harness.drain();
        drop(harness);
        let span = spans
            .iter()
            .find(|span| span.name == "hook pre_tool guard")
            .expect("the hook run opened a span");
        let attribute = |key: &str| {
            span.attributes
                .iter()
                .find(|attribute| attribute.key.as_str() == key)
                .map(|attribute| attribute.value.to_string())
        };
        assert_eq!(attribute("vibe.hook.type"), Some("pre_tool".to_owned()));
        assert_eq!(attribute("gen_ai.tool.name"), Some("read_file".to_owned()));
        assert_eq!(attribute("gen_ai.tool.call.id"), Some("call".to_owned()));
    }

    struct FakeSubagent;

    impl SubagentRunner for FakeSubagent {
        fn run<'a>(
            &'a self,
            context: ChildContext,
            _cancellation: CancellationToken,
        ) -> SubagentFuture<'a> {
            Box::pin(async move {
                Ok(SubagentRun {
                    response: format!("{}:{}", context.agent.name, context.prompt),
                    turns_used: 1,
                    completed: true,
                })
            })
        }
    }

    /// A subagent that opens the span its own turn would open, which is what
    /// makes the delegation path itself measurable.
    struct TracingSubagent;

    impl SubagentRunner for TracingSubagent {
        fn run<'a>(
            &'a self,
            context: ChildContext,
            _cancellation: CancellationToken,
        ) -> SubagentFuture<'a> {
            Box::pin(async move {
                let outcome: Result<(), String> = crate::tracing::agent_span(
                    crate::tracing::AgentSpan {
                        model: None,
                        session_id: Some(&context.child_session_id),
                    },
                    async { Ok(()) },
                )
                .await;
                outcome.map(|()| SubagentRun {
                    response: "delegated".to_owned(),
                    turns_used: 1,
                    completed: true,
                })
            })
        }
    }

    /// US-016: a delegation is awaited inside the tool span that asked for it,
    /// so the child's own agent span hangs off that span rather than opening a
    /// second trace, and it publishes the child conversation id the way
    /// reference `_loop.py` does, one `agent_span` per loop.
    #[tokio::test]
    async fn a_delegated_turn_hangs_off_the_tool_span_that_asked_for_it() {
        let _exclusive = crate::tracing::harness::exclusive();
        let harness = crate::tracing::harness::Harness::install();
        let temporary = tempfile::tempdir().expect("temporary sessions");
        let store = SessionStore::new(temporary.path());
        store
            .create("parent", "/workspace", None, 1)
            .expect("parent session");
        let manager = SubagentManager::new(store, Arc::new(TracingSubagent));
        let effect = crate::tracing::tool_span(
            crate::tracing::ToolSpan {
                tool_name: "task",
                call_id: "call",
                arguments: "{}",
            },
            async {
                manager
                    .delegate(
                        DelegationRequest {
                            parent_session_id: "parent".to_owned(),
                            agent: builtin_agent("explore", AgentKind::Subagent),
                            prompt: "inspect".to_owned(),
                            logging: ChildLoggingPolicy::SummaryOnly,
                        },
                        10,
                    )
                    .await
            },
        )
        .await
        .expect("delegation completes");
        let spans = harness.drain();
        drop(harness);
        let tool = spans
            .iter()
            .find(|span| span.name == "execute_tool task")
            .expect("the delegating tool span was exported");
        let child = spans
            .iter()
            .find(|span| span.name == "invoke_agent mistral-vibe")
            .expect("the delegated turn opened its own agent span");
        assert_eq!(
            child.span_context.trace_id(),
            tool.span_context.trace_id(),
            "the delegated turn stays inside the trace that asked for it"
        );
        assert_eq!(child.parent_span_id, tool.span_context.span_id());
        assert_eq!(
            child
                .attributes
                .iter()
                .find(|attribute| attribute.key.as_str() == "gen_ai.conversation.id")
                .map(|attribute| attribute.value.to_string()),
            Some(effect.child_session_id),
            "the child publishes its own conversation id, as the reference does"
        );
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
