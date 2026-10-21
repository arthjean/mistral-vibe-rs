//! Discovering and reading the plugins installed for a session.
//!
//! Reference `PluginResolver` in `vibe/core/plugins/_native.py`. Three scopes
//! are scanned, project roots first, then the user root, then the builtin
//! roots, and a plugin name resolves to the highest scope that holds it. Each
//! directory is detected as a native Agent Plugins 1.0 package or handed to
//! the compatibility adapter of its foreign format. A broken plugin is dropped
//! and reported, never fatal to the resolve.
//!
//! This module discovers the plugin directories and selects among them; each
//! component a native package declares is read by its own child module, and
//! `adapted` carries over what a foreign adapter produced.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde_json::{Map, Value};

use super::canonical::canonical_json_digest;
use super::compatibility::{
    DetectedPluginFormat, PluginMcpServer, PluginToolOverride, author_display_name,
    detect_plugin_source_format, plugin_runtime_state_names, relative_plugin_path,
    typescript_identifier,
};
use super::content::digest_plugin_tree;
use super::diagnostics::{self as codes, PluginConfigIssue};
use super::paths::{file_name, is_relative_to, resolve_strict};
use crate::extensions::SkillDefinition;
use crate::hooks::{HookConfig, HookProtocol};
use crate::platform::resolve_lenient;
use crate::pycompat;
use crate::skills::SkillScope;

mod adapted;
mod agents;
mod contained;
mod knowledge;
mod libraries;
mod manifest;
mod mcp;
mod runtime_hooks;
mod skills;

use manifest::parse_manifest;

const RESERVED_NAMESPACES: [&str; 6] = [
    "file_system",
    "self",
    "process",
    "skill",
    "subagent",
    "vibe",
];
pub const VIBE_EXTENSION: &str = "ai.mistral.vibe";
const VIBE_EXTENSION_DIRECTORY: &str = "ai.mistral.vibe";
pub const MCP_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json";
/// Where a translated skill's runtime file is recorded in its metadata.
pub const RUNTIME_SKILL_PATH_METADATA: &str = "unified-harness.runtime-skill-path";
/// Where a translated skill's translation kind is recorded in its metadata.
pub const SKILL_TRANSLATION_METADATA: &str = "unified-harness.translation";
#[expect(clippy::expect_used, reason = "compile-time constant pattern")]
static COMPONENT_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$").expect("component name pattern compiles")
});
/// The agent type of every plugin agent: the only one a plugin document may
/// declare. Reference `_PluginAgentDocument.agent_type`.
pub const PLUGIN_AGENT_TYPE: &str = "subagent";

/// One plugin that survived resolution. Reference `PluginDescriptor`.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginDescriptor {
    pub name: String,
    pub version: Option<String>,
    pub description: String,
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub data_root: PathBuf,
    pub namespace: String,
    pub scope: SkillScope,
    pub source_format: DetectedPluginFormat,
    pub tool_overrides: BTreeMap<String, PluginToolOverride>,
    pub private_metadata: BTreeMap<String, Value>,
    pub manifest_digest: String,
    pub content_digest: String,
    pub author: Option<String>,
}

/// An MCP server a plugin declares. Reference `PluginMCPServerDefinition`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMcpServerDefinition {
    pub plugin_name: String,
    pub plugin_namespace: String,
    pub source_id: String,
    pub private_alias: String,
    pub server: PluginMcpServer,
    pub config_file: PathBuf,
}

/// A knowledge folder a plugin publishes. Reference
/// `PluginKnowledgeDefinition`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginKnowledgeDefinition {
    pub plugin_name: String,
    pub name: String,
    pub source_name: String,
    pub description: String,
    pub display_name: Option<String>,
    pub icon: Option<String>,
    pub source_root: PathBuf,
    pub source_entrypoint: PathBuf,
    pub runtime_root: PathBuf,
    pub runtime_entrypoint: PathBuf,
}

/// A subagent type a plugin declares. Reference `PluginAgentDefinition`, with
/// the profile fields the snapshot and the catalog read.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginAgentDefinition {
    pub plugin_name: String,
    pub name: String,
    pub source_name: String,
    pub source_file: PathBuf,
    pub display_name: String,
    pub description: String,
    pub safety: String,
    pub instructions: Option<String>,
    pub overrides: Map<String, Value>,
}

/// A library a plugin vendors. Reference `PluginLibraryDefinition`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLibraryDefinition {
    pub plugin_name: String,
    /// `node` or `python`.
    pub language: String,
    pub alias: String,
    pub source_path: PathBuf,
    pub runtime_path: PathBuf,
    pub config_file: PathBuf,
}

/// A managed connector a plugin requires. Reference
/// `PluginConnectorDefinition`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginConnectorDefinition {
    pub plugin_name: String,
    pub source_id: String,
    pub tools: Vec<String>,
    pub config_file: PathBuf,
}

/// A hook a plugin declares. Reference `RuntimeHookDefinition` for a plugin
/// source, whose visibility is always `private`: a plugin hook is never
/// targeted by name from outside.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeHookDefinition {
    pub config: HookConfig,
    pub source: PluginHookSource,
    pub order: usize,
    pub cwd: Option<PathBuf>,
    pub environment: BTreeMap<String, String>,
    pub protocol: HookProtocol,
    pub plugin_name: String,
    pub declared_name: String,
    pub config_file: PathBuf,
}

/// The source a plugin hook runs under. Reference `HookSource.PROJECT_PLUGIN`
/// and `HookSource.GLOBAL_PLUGIN`: a built-in plugin's hooks count as global.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginHookSource {
    ProjectPlugin,
    GlobalPlugin,
}

impl PluginHookSource {
    const fn of(scope: SkillScope) -> Self {
        match scope {
            SkillScope::Project => Self::ProjectPlugin,
            SkillScope::Global | SkillScope::Builtin => Self::GlobalPlugin,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProjectPlugin => "project_plugin",
            Self::GlobalPlugin => "global_plugin",
        }
    }
}

/// A component a foreign adapter could not carry, attributed to its plugin.
/// Reference `PluginUnsupportedComponent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUnsupportedComponent {
    pub plugin_name: String,
    pub source_format: DetectedPluginFormat,
    pub kind: String,
    pub path: PathBuf,
    pub reason: String,
}

/// Everything one resolve produced. Reference `ResolvedPluginSet`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedPluginSet {
    pub plugins: Vec<PluginDescriptor>,
    /// Keyed by `namespace:name`, in plugin order then discovery order.
    pub skills: Vec<(String, SkillDefinition)>,
    pub mcp_servers: Vec<PluginMcpServerDefinition>,
    pub runtime_hooks: Vec<RuntimeHookDefinition>,
    pub knowledge: Vec<PluginKnowledgeDefinition>,
    pub agents: Vec<PluginAgentDefinition>,
    pub libraries: Vec<PluginLibraryDefinition>,
    pub connectors: Vec<PluginConnectorDefinition>,
    pub issues: Vec<PluginConfigIssue>,
    pub unsupported_components: Vec<PluginUnsupportedComponent>,
}

/// Validates a plugin agent's overrides against the session configuration.
/// Reference `apply_profile_overrides` on a copy of the orchestrator.
pub type AgentOverridesCheck = Arc<dyn Fn(&Map<String, Value>) -> Result<(), String> + Send + Sync>;

/// Where a resolve reads from. Reference `PluginResolver.__init__`.
#[derive(Clone, Default)]
pub struct PluginResolver {
    pub project_roots: Vec<PathBuf>,
    pub user_roots: Vec<PathBuf>,
    /// Plugin directories named outright, skipping discovery, each scoped by
    /// `plugin_scopes` on its resolved path (`project` when absent).
    pub plugin_dirs: Vec<PathBuf>,
    pub builtin_roots: Vec<PathBuf>,
    pub plugin_scopes: HashMap<PathBuf, SkillScope>,
    pub data_root_base: Option<PathBuf>,
    /// The names configured MCP servers hold. Set when a configuration backs
    /// the resolve, which is what lets a plugin server sharing one be dropped.
    pub configured_mcp_names: Option<BTreeSet<String>>,
    pub agent_overrides_check: Option<AgentOverridesCheck>,
    /// The vibe home, which a Kimi Code hook reads as `KIMI_CODE_HOME`.
    pub vibe_home: PathBuf,
}

/// One plugin loaded from its directory, before cross-plugin selection.
#[derive(Debug, Clone)]
struct Candidate {
    descriptor: PluginDescriptor,
    skills: Vec<(String, SkillDefinition)>,
    mcp_servers: Vec<PluginMcpServerDefinition>,
    runtime_hooks: Vec<RuntimeHookDefinition>,
    knowledge: Vec<PluginKnowledgeDefinition>,
    agents: Vec<PluginAgentDefinition>,
    libraries: Vec<PluginLibraryDefinition>,
    connectors: Vec<PluginConnectorDefinition>,
    manifest_path: PathBuf,
}

struct Resolve<'a> {
    resolver: &'a PluginResolver,
    issues: Vec<PluginConfigIssue>,
    unsupported: Vec<PluginUnsupportedComponent>,
}

impl PluginResolver {
    /// Resolves every installed plugin. Reference `PluginResolver.resolve`.
    #[must_use]
    pub fn resolve(&self) -> ResolvedPluginSet {
        let mut state = Resolve {
            resolver: self,
            issues: Vec::new(),
            unsupported: Vec::new(),
        };
        state.run()
    }
}

impl Resolve<'_> {
    fn run(&mut self) -> ResolvedPluginSet {
        let named = self.named_dirs_by_scope();
        let mut project_dirs = named.get(&SkillScope::Project).cloned().unwrap_or_default();
        project_dirs.extend(self.child_dirs(&self.resolver.project_roots.clone()));
        let project = self.load_scope(&project_dirs, SkillScope::Project);
        let mut user_dirs = named.get(&SkillScope::Global).cloned().unwrap_or_default();
        user_dirs.extend(self.child_dirs(&self.resolver.user_roots.clone()));
        let user = self.load_scope(&user_dirs, SkillScope::Global);
        let mut builtin_dirs = named.get(&SkillScope::Builtin).cloned().unwrap_or_default();
        builtin_dirs.extend(self.builtin_plugin_dirs());
        let builtin = self.load_scope(&builtin_dirs, SkillScope::Builtin);

        let selected = select_by_precedence(project, user, builtin);
        let mut selected = self.remove_namespace_collisions(selected);
        selected.sort_by(|left, right| left.descriptor.name.cmp(&right.descriptor.name));

        let plugins: Vec<PluginDescriptor> = selected
            .iter()
            .map(|candidate| candidate.descriptor.clone())
            .collect();
        let skills = selected
            .iter()
            .flat_map(|candidate| candidate.skills.clone())
            .collect();
        let descriptors: BTreeMap<String, PluginDescriptor> = plugins
            .iter()
            .map(|plugin| (plugin.name.clone(), plugin.clone()))
            .collect();
        let mcp_servers = self.remove_config_shadowed_mcp_servers(
            selected
                .iter()
                .flat_map(|candidate| candidate.mcp_servers.clone())
                .collect(),
            &descriptors,
        );
        let mut by_hook_scope = selected.clone();
        by_hook_scope.sort_by(|left, right| {
            (scope_rank(left.descriptor.scope), &left.descriptor.name)
                .cmp(&(scope_rank(right.descriptor.scope), &right.descriptor.name))
        });
        let runtime_hooks = by_hook_scope
            .iter()
            .flat_map(|candidate| candidate.runtime_hooks.clone())
            .collect();
        let knowledge = selected
            .iter()
            .flat_map(|candidate| candidate.knowledge.clone())
            .collect();
        let agents = selected
            .iter()
            .flat_map(|candidate| candidate.agents.clone())
            .collect();
        let libraries = self.remove_library_collisions(
            selected
                .iter()
                .flat_map(|candidate| candidate.libraries.clone())
                .collect(),
        );
        let connectors = selected
            .iter()
            .flat_map(|candidate| candidate.connectors.clone())
            .collect();
        ResolvedPluginSet {
            plugins,
            skills,
            mcp_servers,
            runtime_hooks,
            knowledge,
            agents,
            libraries,
            connectors,
            issues: std::mem::take(&mut self.issues),
            unsupported_components: std::mem::take(&mut self.unsupported),
        }
    }

    fn builtin_plugin_dirs(&mut self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        for root in &self.resolver.builtin_roots {
            match fs::read_dir(root) {
                Ok(entries) => {
                    let mut found: Vec<PathBuf> = entries
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| {
                            path.is_dir()
                                && !file_name(path).starts_with("__")
                                && path.join("plugin.json").is_file()
                        })
                        .collect();
                    found.sort_by_key(|path| file_name(path));
                    dirs.extend(found);
                }
                Err(error) => self.issues.push(PluginConfigIssue::plain(
                    root,
                    format!("Could not list the plugin directory: {error}"),
                )),
            }
        }
        dirs
    }

    fn child_dirs(&mut self, roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        for root in roots {
            match fs::read_dir(root) {
                Ok(entries) => {
                    let mut found: Vec<PathBuf> = entries
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| path.is_dir())
                        .collect();
                    found.sort_by_key(|path| file_name(path));
                    dirs.extend(found);
                }
                Err(error) => self.issues.push(PluginConfigIssue::plain(
                    root,
                    format!("Could not list the plugin directory: {error}"),
                )),
            }
        }
        dirs
    }

    fn named_dirs_by_scope(&self) -> BTreeMap<SkillScope, Vec<PathBuf>> {
        let mut buckets: BTreeMap<SkillScope, Vec<PathBuf>> = BTreeMap::new();
        for plugin_dir in &self.resolver.plugin_dirs {
            let scope = self
                .resolver
                .plugin_scopes
                .get(&resolve_lenient(plugin_dir))
                .copied()
                .unwrap_or(SkillScope::Project);
            buckets.entry(scope).or_default().push(plugin_dir.clone());
        }
        buckets
    }

    fn load_scope(&mut self, plugin_dirs: &[PathBuf], scope: SkillScope) -> Vec<Candidate> {
        let candidates: Vec<Candidate> = plugin_dirs
            .iter()
            .filter_map(|plugin_dir| self.load_plugin(plugin_dir, scope))
            .collect();
        self.remove_same_scope_duplicates(candidates)
    }

    fn load_plugin(&mut self, plugin_dir: &Path, scope: SkillScope) -> Option<Candidate> {
        let root = match resolve_strict(plugin_dir) {
            Ok(root) => root,
            Err(error) => {
                self.issues.push(PluginConfigIssue::plain(
                    plugin_dir,
                    format!("Could not resolve the plugin directory: {error}"),
                ));
                return None;
            }
        };
        let detection = detect_plugin_source_format(&root);
        if let Some(schema) = &detection.unsupported_schema {
            self.issues.push(
                PluginConfigIssue::coded(
                    root.join("plugin.json"),
                    format!("This Agent Plugins schema version is not supported: {schema}"),
                    codes::SCHEMA_VERSION_UNSUPPORTED,
                    Some(DetectedPluginFormat::AgentPlugins10),
                    "manifest",
                )
                .fatal(),
            );
            return None;
        }
        if detection.source_format == DetectedPluginFormat::AgentPlugins10 {
            return self.load_native_plugin(&root, scope);
        }
        if scope == SkillScope::Builtin {
            self.issues.push(
                PluginConfigIssue::coded(
                    &root,
                    format!(
                        "A built-in plugin must be an Agent Plugins 1.0 package, not {}",
                        detection.source_format
                    ),
                    codes::FORMAT_UNSUPPORTED_BUILTIN,
                    Some(detection.source_format),
                    "manifest",
                )
                .fatal(),
            );
        }
        let adapted = scope != SkillScope::Builtin
            && matches!(
                detection.source_format,
                DetectedPluginFormat::Codex
                    | DetectedPluginFormat::ClaudeCode
                    | DetectedPluginFormat::KimiCode
                    | DetectedPluginFormat::OpenCode
            );
        if adapted {
            let data_root_base = self.plugin_data_root_base(&root);
            let result =
                super::adapters::adapt(detection.source_format, &root, &data_root_base, scope);
            self.record_adapter_result(detection.source_format, &result.diagnostics);
            let Some(package) = result.package else {
                self.record_unsupported(
                    &file_name(&root),
                    detection.source_format,
                    &result.unsupported_components,
                );
                return None;
            };
            return self.candidate_from_adapted(&root, package, &result.unsupported_components);
        }
        let marker_paths = detection
            .marker_paths
            .iter()
            .map(|path| relative_plugin_path(path, &root))
            .collect::<Vec<_>>()
            .join(", ");
        let (message, code) = match detection.source_format {
            DetectedPluginFormat::Ambiguous => (
                format!("More than one plugin format matched: {marker_paths}"),
                codes::FORMAT_AMBIGUOUS,
            ),
            DetectedPluginFormat::OpenCode => (
                "OpenCode plugins that run code are never imported or evaluated".to_owned(),
                codes::EXECUTABLE_FORMAT_UNSUPPORTED,
            ),
            DetectedPluginFormat::Unknown => (
                "No plugin manifest in a supported format was found".to_owned(),
                codes::FORMAT_UNRECOGNIZED,
            ),
            other => (
                format!("The {other} plugin format has no compatibility adapter here"),
                codes::ADAPTER_UNAVAILABLE,
            ),
        };
        self.issues.push(
            PluginConfigIssue::coded(
                detection.marker_paths.first().cloned().unwrap_or(root),
                message,
                code,
                Some(detection.source_format),
                "manifest",
            )
            .fatal(),
        );
        None
    }

    fn load_native_plugin(&mut self, root: &Path, scope: SkillScope) -> Option<Candidate> {
        let manifest_path = root.join("plugin.json");
        let loaded = resolve_strict(&manifest_path)
            .map_err(|error| error.to_string())
            .and_then(|resolved| {
                if !is_relative_to(&resolved, root) || !resolved.is_file() {
                    return Err(
                        "plugin.json has to resolve to a regular file inside the plugin".to_owned(),
                    );
                }
                let manifest = parse_manifest(&resolved)?;
                Ok((resolved, manifest))
            });
        let (resolved_manifest, manifest) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                self.issues.push(PluginConfigIssue::plain(
                    &manifest_path,
                    format!("Could not load the manifest: {error}"),
                ));
                return None;
            }
        };
        let extension = self.parse_vibe_extension(&manifest, &resolved_manifest);
        let manifest_digest = canonical_json_digest(&manifest.dump).unwrap_or_default();
        let content_digest = match digest_plugin_tree(
            root,
            &plugin_runtime_state_names(DetectedPluginFormat::AgentPlugins10),
        ) {
            Ok(digest) => digest,
            Err(error) => {
                self.issues.push(PluginConfigIssue::plain(
                    root,
                    format!("Could not digest the plugin contents: {error}"),
                ));
                return None;
            }
        };
        let namespace = extension
            .as_ref()
            .and_then(|extension| extension.tool_namespace.clone())
            .unwrap_or_else(|| typescript_identifier(&manifest.name));
        let reserved = RESERVED_NAMESPACES.contains(&namespace.as_str())
            && !(scope == SkillScope::Builtin && namespace == "vibe");
        if reserved {
            self.issues.push(PluginConfigIssue::plain(
                &manifest_path,
                format!(
                    "The plugin namespace {} is reserved",
                    pycompat::repr_str(&namespace)
                ),
            ));
            return None;
        }
        let description = match &manifest.description {
            Some(description) if !description.is_empty() => description.clone(),
            _ => format!("Capabilities provided by {}.", manifest.name),
        };
        let data_root = self.scope_data_root(scope, root).join(&manifest.name);
        let descriptor = PluginDescriptor {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            description,
            root: root.to_path_buf(),
            manifest_path: resolved_manifest.clone(),
            data_root,
            namespace,
            scope,
            source_format: DetectedPluginFormat::AgentPlugins10,
            tool_overrides: extension
                .as_ref()
                .map(|extension| extension.tool_overrides.clone())
                .unwrap_or_default(),
            private_metadata: BTreeMap::new(),
            manifest_digest,
            content_digest,
            author: author_display_name(
                manifest
                    .author_name
                    .as_ref()
                    .map(|name| Value::String(name.clone()))
                    .as_ref(),
            ),
        };
        let skills = self.load_skills_from_roots(&descriptor, &[root.join("skills")]);
        let mcp_servers = self.load_mcp_servers(&descriptor);
        let runtime_hooks = self.load_runtime_hooks(&descriptor, extension.as_ref());
        let knowledge = self.load_knowledge(&descriptor, extension.as_ref());
        let agents = self.load_agents(&descriptor, extension.as_ref());
        let libraries = self.load_libraries(&descriptor, extension.as_ref());
        let connectors = self.load_connectors(&descriptor, extension.as_ref());
        Some(Candidate {
            descriptor,
            skills,
            mcp_servers,
            runtime_hooks,
            knowledge,
            agents,
            libraries,
            connectors,
            manifest_path: resolved_manifest,
        })
    }

    fn plugin_data_root_base(&self, root: &Path) -> PathBuf {
        self.resolver.data_root_base.clone().unwrap_or_else(|| {
            root.parent()
                .and_then(Path::parent)
                .map_or_else(PathBuf::new, Path::to_path_buf)
                .join("plugin-data")
        })
    }

    fn scope_data_root(&mut self, scope: SkillScope, root: &Path) -> PathBuf {
        if scope == SkillScope::Builtin {
            if let Some(base) = &self.resolver.data_root_base {
                return base.clone();
            }
            self.issues.push(PluginConfigIssue::plain(
                root,
                "A built-in plugin needs a data root base",
            ));
        }
        self.plugin_data_root_base(root)
    }

    fn remove_same_scope_duplicates(&mut self, candidates: Vec<Candidate>) -> Vec<Candidate> {
        let mut order: Vec<String> = Vec::new();
        let mut by_name: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
        for candidate in candidates {
            let name = candidate.descriptor.name.clone();
            if !by_name.contains_key(&name) {
                order.push(name.clone());
            }
            by_name.entry(name).or_default().push(candidate);
        }
        let mut selected = Vec::new();
        for name in order {
            let matches = by_name.remove(&name).unwrap_or_default();
            if matches.len() == 1 {
                selected.extend(matches);
                continue;
            }
            for item in &matches {
                self.issues.push(PluginConfigIssue::plain(
                    &item.manifest_path,
                    format!(
                        "Two plugins named {} have the same precedence",
                        pycompat::repr_str(&name)
                    ),
                ));
            }
        }
        selected
    }

    fn remove_namespace_collisions(&mut self, candidates: Vec<Candidate>) -> Vec<Candidate> {
        let mut order: Vec<String> = Vec::new();
        let mut by_namespace: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
        for candidate in candidates {
            let namespace = candidate.descriptor.namespace.clone();
            if !by_namespace.contains_key(&namespace) {
                order.push(namespace.clone());
            }
            by_namespace.entry(namespace).or_default().push(candidate);
        }
        let mut selected = Vec::new();
        for namespace in order {
            let matches = by_namespace.remove(&namespace).unwrap_or_default();
            if matches.len() == 1 {
                selected.extend(matches);
                continue;
            }
            let mut names: Vec<&str> = matches
                .iter()
                .map(|item| item.descriptor.name.as_str())
                .collect();
            names.sort_unstable();
            let names = names.join(", ");
            for item in &matches {
                self.issues.push(PluginConfigIssue::plain(
                    &item.manifest_path,
                    format!(
                        "The plugin namespace {} is claimed by {names}",
                        pycompat::repr_str(&namespace)
                    ),
                ));
            }
        }
        selected
    }
}

fn select_by_precedence(
    project: Vec<Candidate>,
    user: Vec<Candidate>,
    builtin: Vec<Candidate>,
) -> Vec<Candidate> {
    let mut by_name: BTreeMap<String, Candidate> = BTreeMap::new();
    for candidate in builtin.into_iter().chain(user).chain(project) {
        by_name.insert(candidate.descriptor.name.clone(), candidate);
    }
    by_name.into_values().collect()
}

const fn scope_rank(scope: SkillScope) -> u8 {
    match scope {
        SkillScope::Project => 0,
        SkillScope::Global => 1,
        SkillScope::Builtin => 2,
    }
}

#[cfg(test)]
mod native_tests;
