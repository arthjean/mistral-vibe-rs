use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::{fs, io};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml::Table;

use crate::atomic_file::{FileMode, write_atomically};
use crate::pycompat;
use crate::skills::parser::parse_skill_markdown;
use crate::skills::schema::SkillMetadata;
use crate::skills::{RegistryRef, SkillDiscovery, SkillScope, SkillSource};
use crate::storage::StorageError;

mod agents;
mod subagents;

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
        let resolved = crate::platform::canonicalize(&candidate).unwrap_or(candidate);
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
    let _ = write_atomically(path, "agent", encoded.as_bytes(), FileMode::Private);
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
    Ok(SkillDefinition::from_metadata(
        metadata,
        &body,
        policy_allows,
        source,
        scope,
        path,
    ))
}

impl SkillDefinition {
    /// The skill the `SKILL.md` at `path` declares: its validated
    /// frontmatter and its trimmed body, model invocable unless the
    /// frontmatter or `policy_allows`, what `agents/openai.yaml` decided,
    /// rules it out. Reference `SkillInfo.from_metadata`.
    pub(crate) fn from_metadata(
        metadata: SkillMetadata,
        body: &str,
        policy_allows: bool,
        source: SkillSource,
        scope: SkillScope,
        path: &Path,
    ) -> Self {
        Self {
            name: metadata.name,
            description: metadata.description,
            license: metadata.license,
            compatibility: metadata.compatibility,
            metadata: metadata.metadata,
            allowed_tools: metadata.allowed_tools,
            user_invocable: metadata.user_invocable,
            model_invocable: policy_allows && !metadata.disable_model_invocation,
            body: body.trim().to_owned(),
            source,
            scope,
            path: Some(skill_file_path(path)),
            registry: None,
        }
    }
}

/// The path of a skill read from disk, with its directory resolved and its
/// file name kept, so a `SKILL.md` that is itself a link still names the
/// directory it was found in. Reference `SkillInfo.from_metadata`.
pub(crate) fn skill_file_path(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => crate::platform::resolve_lenient(parent).join(name),
        _ => path.to_path_buf(),
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

/// The display name a profile that declares none is published under:
/// reference `path.stem.replace("-", " ").title()`.
fn title_from_name(name: &str) -> String {
    pycompat::title(&name.replace('-', " "))
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
mod extensions_tests;
