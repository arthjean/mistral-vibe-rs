use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::{fs, io};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml::Table;

use crate::atomic_file::write_atomically;
use crate::skills::parser::parse_skill_markdown;
use crate::skills::schema::SkillMetadata;
use crate::skills::{RegistryRef, SkillDiscovery, SkillScope, SkillSource};
use crate::storage::StorageError;

mod agents;
mod subagents;

#[cfg(test)]
use agents::{auto_approves_edits, canonical_tool_name, profile_permission_scope};

pub use agents::{AgentApproval, AgentKind, AgentProfile, AgentRegistry};
pub use subagents::{
    ChildContext, ChildLoggingPolicy, DelegationRequest, DelegationSignal, DelegationStatus,
    DelegationUpdate, SubagentFuture, SubagentManager, SubagentRun, SubagentRunner,
};

const MAX_EXTENSION_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// One level of delegation.
///
/// Reference `TaskTool.run` (`vibe/core/tools/builtins/task.py`) refuses the
/// call outright when the agent asking is itself a subagent, so a child never
/// forks a grandchild. This port states the same rule as a ceiling on the depth
/// a child session records, which [`SubagentManager::delegate`] reads before it
/// creates anything: a top-level turn delegates, and a subagent asking again is
/// refused with no child started.
const MAX_DELEGATION_DEPTH: u8 = 1;

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
    /// Whether the model may see and load the skill. Reference
    /// `SkillInfo.model_invocable`: false when the frontmatter sets
    /// `disable-model-invocation` or `agents/openai.yaml` disallows implicit
    /// invocation. A user can still invoke it by name.
    #[serde(skip)]
    pub model_invocable: bool,
    pub body: String,
    pub source: SkillSource,
    pub scope: SkillScope,
    /// The resolved absolute path of the `SKILL.md` on disk, absent for a
    /// skill that ships without one; serialization omits it rather than
    /// spelling an empty path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The registry version a skill materialized from the registry was
    /// loaded from; absent for every other source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<RegistryRef>,
}

/// The mechanism a skill load failure is reported under. Reference
/// `SkillManager._try_load_skill` records it as `Failed to load: <reason>`.
pub const SKILL_LOAD_MECHANISM: &str = "skills";
/// The mechanism a skill whose `agents/openai.yaml` disabled model invocation
/// is reported under. The skill still loads; reference
/// `_openai_allows_implicit_invocation` records the issue on the metadata
/// file as `Model invocation disabled: <reason>`.
pub const SKILL_POLICY_MECHANISM: &str = "skill-policy";

impl DiscoveryIssue {
    /// The message a configuration issue carries for a skill issue, prefixed
    /// the way the reference records each mechanism; `None` for any other
    /// mechanism.
    #[must_use]
    pub fn skill_issue_message(&self) -> Option<String> {
        match self.mechanism.as_str() {
            SKILL_LOAD_MECHANISM => Some(format!("Failed to load: {}", self.message)),
            SKILL_POLICY_MECHANISM => Some(format!("Model invocation disabled: {}", self.message)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextExtension {
    pub name: String,
    pub content: String,
    pub source: ExtensionSource,
    pub path: PathBuf,
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
    /// The directories agent profiles are read from, in search order.
    ///
    /// Reference `AgentRegistry._compute_search_paths`: the configured
    /// `agent_paths` that are directories, each open project's
    /// `.vibe/agents`, then the user's `~/.vibe/agents`, deduplicated. They
    /// are directories of profile files rather than extension roots, so they
    /// are listed as they are searched instead of derived from the roots above.
    pub agents: Vec<(ExtensionSource, PathBuf)>,
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

/// Where agent profiles are read from, in search order.
///
/// Reference `AgentRegistry._compute_search_paths`: each configured
/// `agent_paths` entry that is a directory, every open project's
/// `.vibe/agents`, then the user's `{vibe_home}/agents`, with a directory
/// reached twice searched once. `{vibe_home}/extensions/agents` ranks last:
/// it is this port's own, where `agents/install` copies a profile file.
#[must_use]
pub fn agent_search_paths(
    configured: &[String],
    harness: &crate::config::HarnessFiles,
    user_home: Option<&Path>,
    working_directory: &Path,
) -> Vec<(ExtensionSource, PathBuf)> {
    let mut candidates = configured
        .iter()
        .map(|entry| {
            (
                ExtensionSource::Configured,
                crate::skills::anchor(entry, user_home, working_directory),
            )
        })
        .collect::<Vec<_>>();
    candidates.extend(
        harness
            .project_agents_dirs()
            .into_iter()
            .map(|directory| (ExtensionSource::Project, directory)),
    );
    candidates.extend(
        harness
            .user_agents_dirs()
            .into_iter()
            .map(|directory| (ExtensionSource::User, directory)),
    );
    candidates.push((
        ExtensionSource::User,
        harness.vibe_home().join("extensions").join("agents"),
    ));
    let mut unique: Vec<(ExtensionSource, PathBuf)> = Vec::new();
    for (source, candidate) in candidates {
        if !candidate.is_dir() {
            continue;
        }
        let resolved = std::fs::canonicalize(&candidate).unwrap_or(candidate);
        if !unique.iter().any(|(_, seen)| *seen == resolved) {
            unique.push((source, resolved));
        }
    }
    unique
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
        prompts: builtin_prompts,
        commands: BTreeMap::new(),
        issues: Vec::new(),
    };
    let builtin_skill_names = catalog.skills.keys().cloned().collect::<BTreeSet<_>>();

    // The skill roots are their own ordered list rather than a subdirectory of
    // each extension root, and they are walked before the rest so precedence
    // reads in one place.
    for (directory, scope) in &roots.skills.roots {
        discover_skills(&mut catalog, directory, *scope, &builtin_skill_names);
    }
    // Reference `_discover_skills`: the registry's active set is read after
    // every root, and a builtin or disk skill of the same name wins.
    if let Some(sources) = &roots.skills.registry {
        let loaded = crate::skills::registry::loader::active_skills(sources);
        catalog.issues.extend(loaded.issues);
        for (name, skill) in loaded.skills {
            catalog.skills.entry(name).or_insert(skill);
        }
    }
    crate::skills::apply_filters(&mut catalog.skills, &roots.skills);

    let builtin_agents = catalog
        .agents
        .values()
        .filter(|profile| profile.source == ExtensionSource::Builtin)
        .map(|profile| profile.name.clone())
        .collect::<BTreeSet<_>>();
    let mut searched = BTreeSet::new();
    for (source, directory) in &roots.agents {
        if searched.insert(directory.clone()) {
            discover_agents(&mut catalog, &builtin_agents, *source, directory);
        }
    }
    for (source, root) in roots.ordered() {
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
    }
    catalog
}

/// Reads every profile file of `directory` into the catalog.
///
/// Reference `AgentRegistry._discover` and `_try_load`: a legacy file is
/// migrated on disk first, a file that does not load as a profile is skipped
/// (the reference only logs it), a builtin name is taken over by every file
/// that declares it, so the last directory searched wins, and any other name
/// keeps the first file that declared it.
fn discover_agents(
    catalog: &mut ExtensionCatalog,
    builtins: &BTreeSet<String>,
    source: ExtensionSource,
    directory: &Path,
) {
    if !directory.is_dir() {
        return;
    }
    let mut ignored = Vec::new();
    for path in sorted_files(directory, "toml", &mut ignored, "agents") {
        migrate_agent_file(&path);
        let Ok(profile) = parse_agent(&path, source) else {
            continue;
        };
        if builtins.contains(&profile.name) || !catalog.agents.contains_key(&profile.name) {
            catalog.agents.insert(profile.name.clone(), profile);
        }
    }
}

/// Rewrites a profile that still carries `base_disabled` in the shape the
/// reference migrates it to. Reference `_migrate_agent_profile_file`: a file
/// that is unreadable, not TOML or already current is left alone, and a write
/// that fails leaves the original in place.
fn migrate_agent_file(path: &Path) {
    let Ok(contents) = read_bounded_text(path) else {
        return;
    };
    let Ok(mut table) = contents.parse::<Table>() else {
        return;
    };
    if !migrate_agent_table(&mut table) {
        return;
    }
    let Ok(encoded) = toml::to_string(&table) else {
        return;
    };
    // The migration is best effort, as the reference's is: a profile that
    // cannot be rewritten is still read through the same migration.
    let _ = write_atomically(path, "agent", encoded.as_bytes());
}

fn discover_skills(
    catalog: &mut ExtensionCatalog,
    directory: &Path,
    scope: SkillScope,
    builtin_names: &BTreeSet<String>,
) {
    let (skills, issues) = skills_in_directory(directory, scope, builtin_names);
    catalog.issues.extend(issues);
    for (name, skill) in skills {
        catalog.skills.entry(name).or_insert(skill);
    }
}

/// Every skill one root publishes, first directory wins within the root, and
/// the issues its files raised. Reference `_discover_skills_in_dir`: a
/// directory without a `SKILL.md` is ignored, a file that does not load is an
/// issue, and a reserved builtin name is skipped.
pub(crate) fn skills_in_directory(
    directory: &Path,
    scope: SkillScope,
    builtin_names: &BTreeSet<String>,
) -> (BTreeMap<String, SkillDefinition>, Vec<DiscoveryIssue>) {
    let mut skills = BTreeMap::new();
    let mut issues = Vec::new();
    let mut directories = match fs::read_dir(directory) {
        Ok(entries) => entries.filter_map(Result::ok).collect::<Vec<_>>(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (skills, issues),
        Err(error) => {
            issues.push(DiscoveryIssue {
                mechanism: SKILL_LOAD_MECHANISM.to_owned(),
                path: directory.to_path_buf(),
                message: error.to_string(),
            });
            return (skills, issues);
        }
    };
    directories.sort_by_key(fs::DirEntry::file_name);
    for entry in directories {
        let path = entry.path().join("SKILL.md");
        if !path.is_file() {
            continue;
        }
        match load_skill(&path, SkillSource::Local, scope, &mut issues) {
            Some(skill) => {
                if !builtin_names.contains(&skill.name) && !skills.contains_key(&skill.name) {
                    skills.insert(skill.name.clone(), skill);
                }
            }
            None => continue,
        }
    }
    (skills, issues)
}

/// Reference `SkillManager._try_load_skill`: the skill at `path`, or
/// [`None`] with the reason recorded as an issue.
pub(crate) fn load_skill(
    path: &Path,
    source: SkillSource,
    scope: SkillScope,
    issues: &mut Vec<DiscoveryIssue>,
) -> Option<SkillDefinition> {
    match parse_skill(path, source, scope, issues) {
        Ok(skill) => Some(skill),
        Err(error) => {
            issues.push(DiscoveryIssue {
                mechanism: SKILL_LOAD_MECHANISM.to_owned(),
                path: path.to_path_buf(),
                message: error.to_string(),
            });
            None
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
    // Reference `AgentProfile.from_toml` pops the profile's own fields and
    // keeps the rest as the configuration overrides the profile applies.
    let display_name =
        take_string(&mut table, "display_name").unwrap_or_else(|| title_from_name(&name));
    let description = take_string(&mut table, "description").unwrap_or_default();
    let safety = take_string(&mut table, "safety").unwrap_or_else(|| "neutral".to_owned());
    if !AGENT_SAFETIES.contains(&safety.as_str()) {
        return Err(ExtensionError::InvalidAgentSafety(safety));
    }
    let kind = match take_string(&mut table, "agent_type")
        .unwrap_or_else(|| "agent".to_owned())
        .as_str()
    {
        "agent" => AgentKind::Agent,
        "subagent" => AgentKind::Subagent,
        value => return Err(ExtensionError::InvalidAgentKind(value.to_owned())),
    };
    table.remove("instructions");
    // Reference `_try_load` folds the overrides onto a copy of the
    // configuration, so a profile whose overrides the configuration refuses
    // is dropped at discovery rather than at selection.
    crate::config::registry::validate_field_types(&table)
        .map_err(ExtensionError::InvalidAgentOverrides)?;
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

/// The safeties a profile may declare. Reference `AgentSafety`.
const AGENT_SAFETIES: [&str; 5] = ["safe", "neutral", "destructive", "smart", "yolo"];

/// The key a legacy profile listed the tools it disabled under.
const LEGACY_BASE_DISABLED_KEY: &str = "base_disabled";

/// Folds a legacy `base_disabled` list into `disabled_tools`, keeping the
/// order the two lists give and dropping repeats, and answers whether the
/// table changed. Reference `migrate_agent_profile_config`: a key that is not
/// a list is left in place.
fn migrate_agent_table(table: &mut Table) -> bool {
    let Some(toml::Value::Array(legacy)) = table.get(LEGACY_BASE_DISABLED_KEY).cloned() else {
        return false;
    };
    table.remove(LEGACY_BASE_DISABLED_KEY);
    let merged = match table.get("disabled_tools") {
        Some(toml::Value::Array(current)) => {
            let mut merged: Vec<toml::Value> = Vec::new();
            for value in current.iter().chain(legacy.iter()) {
                if !merged.contains(value) {
                    merged.push(value.clone());
                }
            }
            merged
        }
        _ => legacy,
    };
    table.insert("disabled_tools".to_owned(), toml::Value::Array(merged));
    true
}

fn parse_skill(
    path: &Path,
    source: SkillSource,
    scope: SkillScope,
    issues: &mut Vec<DiscoveryIssue>,
) -> Result<SkillDefinition, ExtensionError> {
    let contents = read_bounded_text(path)?;
    let (frontmatter, body) = parse_skill_markdown(&contents)
        .map_err(|error| ExtensionError::InvalidSkill(error.to_string()))?;
    let metadata = SkillMetadata::validate(&frontmatter)
        .map_err(|error| ExtensionError::InvalidSkill(error.to_string()))?;
    // A frontmatter name that differs from the directory name is a log-only
    // warning upstream, never a rejection or a diagnostic: the frontmatter
    // name wins and nothing else is observable.
    //
    // Reference `SkillInfo.from_metadata` resolves the directory and keeps the
    // file name, so a `SKILL.md` that is itself a symlink still names the
    // directory it was configured in.
    let resolved = match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => fs::canonicalize(parent)
            .unwrap_or_else(|_| parent.to_path_buf())
            .join(name),
        _ => path.to_path_buf(),
    };
    let policy_allows = match crate::skills::openai_invocation_policy(path) {
        Ok(allowed) => allowed,
        Err(reason) => {
            issues.push(DiscoveryIssue {
                mechanism: SKILL_POLICY_MECHANISM.to_owned(),
                path: crate::skills::openai_metadata_path(path).unwrap_or_default(),
                message: reason,
            });
            false
        }
    };
    Ok(SkillDefinition {
        name: metadata.name,
        description: metadata.description,
        license: metadata.license,
        compatibility: metadata.compatibility,
        metadata: metadata.metadata,
        allowed_tools: metadata.allowed_tools,
        user_invocable: metadata.user_invocable,
        model_invocable: !metadata.disable_model_invocation && policy_allows,
        body: body.trim().to_owned(),
        source,
        scope,
        path: Some(resolved),
        registry: None,
    })
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

/// The display name a profile that declares none is published under.
///
/// Reference `path.stem.replace("-", " ").title()`: every run of cased
/// letters starts upper case and continues lower case, and anything that is
/// not a cased letter (a digit, a space, an underscore) starts a new run.
fn title_from_name(name: &str) -> String {
    let mut title = String::with_capacity(name.len());
    let mut previous_cased = false;
    for character in name.chars() {
        let character = if character == '-' { ' ' } else { character };
        let cased = character.is_uppercase() || character.is_lowercase();
        if cased && previous_cased {
            title.extend(character.to_lowercase());
        } else if cased {
            title.extend(character.to_uppercase());
        } else {
            title.push(character);
        }
        previous_cased = cased;
    }
    title
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
    #[error("invalid agent safety `{0}`")]
    InvalidAgentSafety(String),
    #[error("invalid agent overrides: {0}")]
    InvalidAgentOverrides(String),
    #[error("invalid skill: {0}")]
    InvalidSkill(String),
    #[error("agent `{0}` was not found")]
    MissingAgent(String),
    #[error("agent `{0}` is not owned by the user install directory")]
    AgentNotUserOwned(String),
    #[error("agent `{0}` is not a subagent")]
    AgentNotSubagent(String),
    #[error("delegation depth exceeds the maximum of {maximum}")]
    DelegationDepth { maximum: u8 },
    #[error("could not allocate a unique child session ID")]
    ChildIdExhausted,
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

    /// Reference `AgentRegistry._discover`: every directory searched takes a
    /// builtin name over, so the last one wins, while a custom name keeps the
    /// first file that declared it. The skill roots are their own list.
    #[test]
    fn a_builtin_name_goes_to_the_last_directory_and_a_custom_name_to_the_first() {
        let temporary = tempfile::tempdir().expect("temporary roots");
        let configured = temporary.path().join("configured");
        let user = temporary.path().join("user");
        for root in [&configured, &user] {
            fs::create_dir_all(root.join("agents")).expect("agent directory");
            fs::create_dir_all(root.join("skills/probe")).expect("skill directory");
        }
        let skill_roots = crate::skills::SkillDiscovery {
            roots: vec![
                (configured.join("skills"), crate::skills::SkillScope::Global),
                (user.join("skills"), crate::skills::SkillScope::Global),
            ],
            ..crate::skills::SkillDiscovery::default()
        };
        for (root, origin) in [(&configured, "configured"), (&user, "user")] {
            for name in ["default", "custom"] {
                fs::write(
                    root.join(format!("agents/{name}.toml")),
                    format!("description = \"{origin}\"\n"),
                )
                .expect("agent file");
            }
        }
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
            agents: vec![
                (ExtensionSource::Configured, configured.join("agents")),
                (ExtensionSource::User, user.join("agents")),
            ],
            skills: skill_roots,
            ..DiscoveryRoots::default()
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
        assert_eq!(catalog.agents["default"].description, "user");
        assert_eq!(catalog.agents["custom"].description, "configured");
        assert_eq!(catalog.skills["probe"].body, "configured body");
    }

    /// A profile that does not load is skipped without an issue, as the
    /// reference only logs it, and a legacy `base_disabled` list is folded
    /// into `disabled_tools` on disk.
    #[test]
    fn unloadable_profiles_are_skipped_and_legacy_ones_rewritten() {
        let temporary = tempfile::tempdir().expect("temporary roots");
        let agents = temporary.path().join("agents");
        fs::create_dir_all(&agents).expect("agent directory");
        fs::write(agents.join("broken.toml"), "broken = [").expect("bad agent");
        fs::write(agents.join("reckless.toml"), "safety = \"reckless\"\n").expect("bad safety");
        fs::write(
            agents.join("legacy.toml"),
            "base_disabled = [\"bash\", \"grep\"]\ndisabled_tools = [\"grep\"]\n",
        )
        .expect("legacy agent");
        let catalog = discover_extensions(
            &DiscoveryRoots {
                agents: vec![(ExtensionSource::User, agents.clone())],
                ..DiscoveryRoots::default()
            },
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        assert_eq!(catalog.agents.keys().collect::<Vec<_>>(), ["legacy"]);
        assert!(catalog.issues.is_empty());
        let rewritten = fs::read_to_string(agents.join("legacy.toml"))
            .expect("rewritten")
            .parse::<Table>()
            .expect("still TOML");
        assert!(!rewritten.contains_key("base_disabled"));
        assert_eq!(
            rewritten["disabled_tools"],
            toml::Value::Array(vec!["grep".into(), "bash".into()])
        );
    }

    #[test]
    fn a_display_name_defaults_to_the_title_cased_file_stem() {
        assert_eq!(title_from_name("code_review-v2x"), "Code_Review V2X");
        assert_eq!(title_from_name("my-AGENT"), "My Agent");
    }

    /// Writes the child's prompt to its transcript, which is what a running
    /// child does first and what saves its session.
    fn record_prompt(context: &ChildContext) {
        let store = &context.store;
        let mut metadata = store
            .open(&context.child_session_id)
            .expect("the child is held")
            .metadata;
        store
            .append_message(
                &mut metadata,
                &crate::events::ModelMessage::user(context.prompt.clone()),
                10,
            )
            .expect("the child prompt is saved");
    }

    struct FakeSubagent;

    impl SubagentRunner for FakeSubagent {
        fn run<'a>(
            &'a self,
            context: ChildContext,
            _cancellation: CancellationToken,
        ) -> SubagentFuture<'a> {
            Box::pin(async move {
                record_prompt(&context);
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
                            tool_call_id: "call-1".to_owned(),
                            signal: None,
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
            context: ChildContext,
            _cancellation: CancellationToken,
        ) -> SubagentFuture<'a> {
            Box::pin(async move {
                record_prompt(&context);
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
                    tool_call_id: "call-1".to_owned(),
                    signal: None,
                },
                10,
            )
            .await
            .expect("delegation completes");
        assert_eq!(effect.status, DelegationStatus::Completed);
        assert_ne!(effect.child_session_id, effect.parent_session_id);
        let parent_record = store.open("parent").expect("parent loads").metadata;
        let children = store.child_store(&parent_record, "explore");
        let child = children
            .open(&effect.child_session_id)
            .expect("child persisted beneath its parent");
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
                    tool_call_id: "call-1".to_owned(),
                    signal: None,
                },
                10,
            )
            .await
            .expect("same-millisecond delegation completes");
        assert_ne!(effect.child_session_id, second.child_session_id);
        // Both children are linked into the parent's record, and neither is a
        // session of its own in the parent's store.
        assert_eq!(
            store
                .open("parent")
                .expect("parent loads")
                .metadata
                .child_sessions
                .len(),
            1,
            "the second delegation reused the first call's identifier"
        );
        assert!(
            store
                .sessions(None)
                .expect("sessions list")
                .iter()
                .all(|session| session.parent_session_id.is_none())
        );

        let mut parent = store.open("parent").expect("parent loads").metadata;
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
                        tool_call_id: "call-3".to_owned(),
                        signal: None,
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
                            tool_call_id: "call-1".to_owned(),
                            signal: None,
                        },
                        10,
                    )
                    .await
            }
        });
        entered.notified().await;
        manager.cancel_parent("parent").await;
        let effect = tokio::time::timeout(std::time::Duration::from_millis(250), delegation)
            .await
            .expect("delegation cancellation")
            .expect("delegation task")
            .expect("delegation effect");
        assert_eq!(effect.status, DelegationStatus::Cancelled);
        let parent_record = store.open("parent").expect("parent loads").metadata;
        assert!(
            store
                .child_store(&parent_record, "explore")
                .open(&effect.child_session_id)
                .expect("child remains auditable")
                .metadata
                .end_time
                .is_some()
        );
        assert!(manager.active.lock().await.is_empty());
    }
}
