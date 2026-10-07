//! Discovering and reading the plugins installed for a session.
//!
//! Reference `PluginResolver` in `vibe/core/plugins/_native.py`. Three scopes
//! are scanned, project roots first, then the user root, then the builtin
//! roots, and a plugin name resolves to the highest scope that holds it. Each
//! directory is detected as a native Agent Plugins 1.0 package or handed to
//! the compatibility adapter of its foreign format. A broken plugin is dropped
//! and reported, never fatal to the resolve.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde_json::{Map, Value};

use super::canonical::{canonical_json_digest, sha256_hex};
use super::compatibility::{
    AdaptedHook, AdaptedMcpServer, AdaptedPluginPackage, AdaptedSkill, AdaptedUnsupportedComponent,
    DetectedPluginFormat, PluginAdapterDiagnostic, PluginMcpServer, PluginToolOverride,
    author_display_name, blake2s_hex, detect_plugin_source_format, plugin_runtime_state_names,
    read_text, relative_plugin_path, typescript_identifier,
};
use super::content::digest_plugin_tree;
use super::diagnostics::{self as codes, PluginConfigIssue, Severity};
use super::paths::{is_relative_to, resolve_lax, resolve_strict};
use super::strict::{self, ValidationErrors, at, char_len};
use crate::extensions::SkillDefinition;
use crate::hooks::HookConfig;
use crate::skills::parser::parse_skill_markdown;
use crate::skills::schema::SkillMetadata;
use crate::skills::{SkillScope, SkillSource};

const RESERVED_NAMESPACES: [&str; 5] = ["file_system", "self", "process", "agent", "vibe"];
pub const VIBE_EXTENSION: &str = "ai.mistral.vibe";
const VIBE_EXTENSION_DIRECTORY: &str = "ai.mistral.vibe";
pub const MCP_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json";
/// Where a translated skill's runtime file is recorded in its metadata.
pub const RUNTIME_SKILL_PATH_METADATA: &str = "unified-harness.runtime-skill-path";
/// Where a translated skill's translation kind is recorded in its metadata.
pub const SKILL_TRANSLATION_METADATA: &str = "unified-harness.translation";
const MAX_PLUGIN_HOOKS_BYTES: u64 = 64 * 1024;
const MAX_PLUGIN_HOOKS: usize = 128;
const MAX_PLUGIN_KNOWLEDGE_FOLDERS: usize = 100;
const MAX_PLUGIN_KNOWLEDGE_ENTRYPOINT_BYTES: u64 = 256 * 1024;
const MAX_PLUGIN_AGENTS: usize = 128;
const MAX_PLUGIN_AGENT_BYTES: u64 = 64 * 1024;
const MAX_PLUGIN_COMPONENT_PATH_LENGTH: usize = 1024;
const MAX_NODE_LIBRARY_ALIAS_LENGTH: usize = 214;
const MAX_PYTHON_LIBRARY_ALIAS_LENGTH: usize = 128;
const MAX_CONNECTOR_TOOL_NAME_LENGTH: usize = 256;

#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static PLUGIN_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?$").expect("plugin name pattern compiles")
});
#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static COMPONENT_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$").expect("component name pattern compiles")
});
#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static TYPESCRIPT_IDENTIFIER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_$][A-Za-z0-9_$]*$").expect("identifier pattern compiles")
});
#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static NODE_LIBRARY_ALIAS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:@[a-z0-9][a-z0-9._-]*/)?[a-z0-9][a-z0-9._-]*$")
        .expect("node alias pattern compiles")
});
#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static PYTHON_LIBRARY_ALIAS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("python alias pattern compiles")
});
#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static PLUGIN_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{PLUGIN_(?:ROOT|DATA)\}").expect("placeholder pattern compiles")
});
#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static HTTP_HEADER_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[!#$%&'*+.^_`|~0-9A-Za-z-]+$").expect("header pattern compiles")
});

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
    /// `subagent`, the only type a plugin document may declare.
    pub agent_type: String,
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
/// source.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeHookDefinition {
    pub config: HookConfig,
    /// `project_plugin` or `global_plugin`.
    pub source: String,
    pub order: usize,
    pub cwd: Option<PathBuf>,
    pub environment: BTreeMap<String, String>,
    /// `private`: a plugin hook is never targeted by name from outside.
    pub visibility: String,
    /// `vibe`, `claude_code` or `kimi_code`.
    pub protocol: String,
    pub plugin_name: String,
    pub declared_name: String,
    pub config_file: PathBuf,
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

/// The validated native manifest. Reference `_PluginManifest`.
#[derive(Debug, Clone)]
struct Manifest {
    name: String,
    version: Option<String>,
    description: Option<String>,
    author_name: Option<String>,
    extensions: Option<Map<String, Value>>,
    dump: Value,
}

/// The validated `ai.mistral.vibe` extension. Reference
/// `_VibePluginExtension`.
#[derive(Debug, Clone, Default)]
struct Extension {
    tool_namespace: Option<String>,
    tool_overrides: BTreeMap<String, PluginToolOverride>,
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
                .get(&resolve_lax(plugin_dir))
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
                    python_repr(&namespace)
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

    fn candidate_from_adapted(
        &mut self,
        root: &Path,
        package: AdaptedPluginPackage,
        unsupported: &[AdaptedUnsupportedComponent],
    ) -> Option<Candidate> {
        if RESERVED_NAMESPACES.contains(&package.namespace.as_str()) {
            self.issues.push(
                PluginConfigIssue::coded(
                    &package.manifest_path,
                    format!(
                        "The plugin namespace {} is reserved",
                        python_repr(&package.namespace)
                    ),
                    codes::NAMESPACE_RESERVED,
                    Some(package.source_format),
                    "manifest",
                )
                .fatal(),
            );
            return None;
        }
        let digested = read_text(&package.manifest_path).and_then(|text| {
            let digest =
                digest_plugin_tree(root, &plugin_runtime_state_names(package.source_format))?;
            Ok((text, digest))
        });
        let (manifest_text, content_digest) = match digested {
            Ok(digested) => digested,
            Err(error) => {
                self.issues.push(
                    PluginConfigIssue::coded(
                        &package.manifest_path,
                        format!("Could not digest the plugin contents: {error}"),
                        codes::FILESYSTEM_ERROR,
                        Some(package.source_format),
                        "package",
                    )
                    .fatal(),
                );
                return None;
            }
        };
        let manifest_digest = serde_json::from_str::<Value>(&manifest_text)
            .ok()
            .and_then(|value| canonical_json_digest(&value).ok())
            .unwrap_or_else(|| sha256_hex(manifest_text.as_bytes()));
        let descriptor = PluginDescriptor {
            name: package.name.clone(),
            version: package.version.clone(),
            description: package.description.clone(),
            root: root.to_path_buf(),
            manifest_path: package.manifest_path.clone(),
            data_root: package.data_root.clone(),
            namespace: package.namespace.clone(),
            scope: package.scope,
            source_format: package.source_format,
            tool_overrides: package.tool_overrides.clone(),
            private_metadata: package.private_metadata.clone(),
            manifest_digest,
            content_digest,
            author: package.author.clone(),
        };
        self.unsupported.extend(
            unsupported
                .iter()
                .map(|component| PluginUnsupportedComponent {
                    plugin_name: package.name.clone(),
                    source_format: package.source_format,
                    kind: component.kind.clone(),
                    path: component.path.clone(),
                    reason: component.reason.clone(),
                }),
        );
        let mut skills = self.load_skills_from_roots(&descriptor, &package.skill_roots);
        for (alias, skill) in self.load_adapted_skills(&descriptor, &package.adapted_skills) {
            if let Some(slot) = skills.iter_mut().find(|(existing, _)| *existing == alias) {
                slot.1 = skill;
            } else {
                skills.push((alias, skill));
            }
        }
        Some(Candidate {
            mcp_servers: adapt_mcp_servers(&descriptor, &package.mcp_servers),
            runtime_hooks: self.adapt_runtime_hooks(&descriptor, &package.adapted_hooks),
            descriptor,
            skills,
            knowledge: Vec::new(),
            agents: Vec::new(),
            libraries: Vec::new(),
            connectors: Vec::new(),
            manifest_path: package.manifest_path,
        })
    }

    fn record_unsupported(
        &mut self,
        plugin_name: &str,
        source_format: DetectedPluginFormat,
        unsupported: &[AdaptedUnsupportedComponent],
    ) {
        self.unsupported.extend(
            unsupported
                .iter()
                .map(|component| PluginUnsupportedComponent {
                    plugin_name: plugin_name.to_owned(),
                    source_format,
                    kind: component.kind.clone(),
                    path: component.path.clone(),
                    reason: component.reason.clone(),
                }),
        );
    }

    fn load_adapted_skills(
        &mut self,
        plugin: &PluginDescriptor,
        definitions: &[AdaptedSkill],
    ) -> Vec<(String, SkillDefinition)> {
        let mut skills: Vec<(String, SkillDefinition)> = Vec::new();
        for definition in definitions {
            let alias = format!("{}:{}", plugin.namespace, definition.source_name);
            if skills.iter().any(|(existing, _)| *existing == alias) {
                self.issues.push(PluginConfigIssue::coded(
                    &definition.source_path,
                    format!(
                        "The plugin skill alias {} is declared twice",
                        python_repr(&alias)
                    ),
                    codes::SKILL_COLLISION,
                    Some(plugin.source_format),
                    "skill",
                ));
                continue;
            }
            let mut runtime_path = definition.source_path.clone();
            if definition.translation.as_deref() == Some("synthetic_skill") {
                runtime_path = plugin
                    .data_root
                    .join("generated-skills")
                    .join(&definition.source_name)
                    .join("SKILL.md");
                let written = runtime_path
                    .parent()
                    .map_or(Ok(()), fs::create_dir_all)
                    .and_then(|()| fs::write(&runtime_path, render_generated_skill(definition)));
                if let Err(error) = written {
                    self.issues.push(PluginConfigIssue::coded(
                        &definition.source_path,
                        format!("Could not write the generated skill: {error}"),
                        codes::SKILL_MATERIALIZATION_FAILED,
                        Some(plugin.source_format),
                        "skill",
                    ));
                    continue;
                }
            }
            let mut metadata = definition.metadata.clone();
            metadata.insert(
                RUNTIME_SKILL_PATH_METADATA.to_owned(),
                runtime_path.to_string_lossy().into_owned(),
            );
            if let Some(translation) = &definition.translation {
                metadata.insert(SKILL_TRANSLATION_METADATA.to_owned(), translation.clone());
            }
            let skill_path = skill_file_path(&definition.source_path);
            skills.push((
                alias.clone(),
                SkillDefinition {
                    name: alias,
                    description: definition.description.clone(),
                    license: definition.license.clone(),
                    compatibility: definition.compatibility.clone(),
                    metadata,
                    allowed_tools: definition.allowed_tools.clone(),
                    user_invocable: definition.user_invocable,
                    model_invocable: definition.model_invocable,
                    body: definition.prompt.clone(),
                    source: SkillSource::Plugin,
                    scope: plugin.scope,
                    path: Some(skill_path),
                    registry: None,
                },
            ));
        }
        skills
    }

    fn adapt_runtime_hooks(
        &self,
        plugin: &PluginDescriptor,
        definitions: &[AdaptedHook],
    ) -> Vec<RuntimeHookDefinition> {
        let source = hook_source(plugin.scope);
        let mut environment = BTreeMap::from([
            ("PLUGIN_ROOT".to_owned(), path_text(&plugin.root)),
            ("PLUGIN_DATA".to_owned(), path_text(&plugin.data_root)),
        ]);
        let mut cwd = Some(plugin.root.clone());
        if plugin.source_format == DetectedPluginFormat::ClaudeCode {
            environment.insert("CLAUDE_PLUGIN_ROOT".to_owned(), path_text(&plugin.root));
            environment.insert(
                "CLAUDE_PLUGIN_DATA".to_owned(),
                path_text(&plugin.data_root),
            );
            cwd = None;
        }
        if plugin.source_format == DetectedPluginFormat::KimiCode {
            environment.insert("KIMI_PLUGIN_ROOT".to_owned(), path_text(&plugin.root));
            environment.insert(
                "KIMI_CODE_HOME".to_owned(),
                path_text(&self.resolver.vibe_home),
            );
        }
        definitions
            .iter()
            .enumerate()
            .map(|(order, definition)| {
                let mut config = definition.config.clone();
                config.name = format!("{}:{}", plugin.name, definition.config.name);
                RuntimeHookDefinition {
                    config,
                    source: source.to_owned(),
                    order,
                    cwd: cwd.clone(),
                    environment: environment.clone(),
                    visibility: "private".to_owned(),
                    protocol: definition.protocol.clone(),
                    plugin_name: plugin.name.clone(),
                    declared_name: definition.config.name.clone(),
                    config_file: definition.source_path.clone(),
                }
            })
            .collect()
    }

    fn record_adapter_result(
        &mut self,
        source_format: DetectedPluginFormat,
        diagnostics: &[PluginAdapterDiagnostic],
    ) {
        self.issues
            .extend(diagnostics.iter().map(|diagnostic| PluginConfigIssue {
                file: diagnostic.path.clone(),
                message: diagnostic.message.clone(),
                severity: diagnostic.severity,
                code: Some(diagnostic.code.clone()),
                fatal: diagnostic.fatal,
                source_format: Some(source_format),
                component: Some(diagnostic.component.clone()),
            }));
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

    fn parse_vibe_extension(
        &mut self,
        manifest: &Manifest,
        manifest_path: &Path,
    ) -> Option<Extension> {
        let raw = manifest.extensions.as_ref()?.get(VIBE_EXTENSION)?;
        // The reference reads a missing extension as `None` with `.get`, and an
        // explicit `null` the same way.
        if raw.is_null() {
            return None;
        }
        match parse_extension(raw) {
            Ok(extension) => Some(extension),
            Err(error) => {
                self.issues.push(PluginConfigIssue::plain(
                    manifest_path,
                    format!("Could not load the {VIBE_EXTENSION} extension: {error}"),
                ));
                None
            }
        }
    }

    fn load_runtime_hooks(
        &mut self,
        plugin: &PluginDescriptor,
        extension: Option<&Extension>,
    ) -> Vec<RuntimeHookDefinition> {
        if extension.is_none() {
            return Vec::new();
        }
        let path = plugin
            .root
            .join(VIBE_EXTENSION_DIRECTORY)
            .join("hooks.toml");
        if !exists_or_link(&path) {
            return Vec::new();
        }
        let resolved = contained_component_file(&plugin.root, &path).and_then(|resolved| {
            let size = fs::metadata(&resolved)
                .map_err(|error| error.to_string())?
                .len();
            if size > MAX_PLUGIN_HOOKS_BYTES {
                return Err(
                    "hooks.toml is larger than the 65536-byte limit for a plugin".to_owned(),
                );
            }
            Ok(resolved)
        });
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                self.issues.push(PluginConfigIssue::coded(
                    &path,
                    format!("Could not load the plugin hooks: {error}"),
                    codes::HOOKS_INVALID,
                    Some(plugin.source_format),
                    "hook",
                ));
                return Vec::new();
            }
        };
        let parsed = crate::hooks::load_hooks_file_with(&resolved, true);
        self.issues.extend(parsed.issues.iter().map(|issue| {
            PluginConfigIssue::coded(
                &issue.file,
                issue.message.clone(),
                codes::HOOKS_INVALID,
                Some(plugin.source_format),
                "hook",
            )
        }));
        let source = hook_source(plugin.scope);
        let environment = BTreeMap::from([
            ("PLUGIN_ROOT".to_owned(), path_text(&plugin.root)),
            ("PLUGIN_DATA".to_owned(), path_text(&plugin.data_root)),
        ]);
        let mut definitions = Vec::new();
        let mut seen = BTreeSet::new();
        for (order, hook) in parsed.hooks.iter().enumerate() {
            if order >= MAX_PLUGIN_HOOKS {
                self.issues.push(PluginConfigIssue::coded(
                    &resolved,
                    "hooks.toml declares more than the 128 hooks a plugin may",
                    codes::HOOKS_LIMIT_EXCEEDED,
                    Some(plugin.source_format),
                    "hook",
                ));
                break;
            }
            if !seen.insert(hook.name.clone()) {
                self.issues.push(PluginConfigIssue::coded(
                    &resolved,
                    format!(
                        "The plugin hook name {} is declared twice",
                        python_repr(&hook.name)
                    ),
                    codes::HOOKS_DUPLICATE_NAME,
                    Some(plugin.source_format),
                    "hook",
                ));
                continue;
            }
            let mut config = hook.clone();
            config.name = format!("{}:{}", plugin.name, hook.name);
            definitions.push(RuntimeHookDefinition {
                config,
                source: source.to_owned(),
                order,
                cwd: Some(plugin.root.clone()),
                environment: environment.clone(),
                visibility: "private".to_owned(),
                protocol: "vibe".to_owned(),
                plugin_name: plugin.name.clone(),
                declared_name: hook.name.clone(),
                config_file: resolved.clone(),
            });
        }
        definitions
    }

    fn load_knowledge(
        &mut self,
        plugin: &PluginDescriptor,
        extension: Option<&Extension>,
    ) -> Vec<PluginKnowledgeDefinition> {
        if extension.is_none() {
            return Vec::new();
        }
        let knowledge_root = plugin.root.join(VIBE_EXTENSION_DIRECTORY).join("knowledge");
        if !exists_or_link(&knowledge_root) {
            return Vec::new();
        }
        let listed = contained_component_directory(&plugin.root, &knowledge_root)
            .and_then(|resolved| Ok((resolved.clone(), sorted_children(&resolved)?)));
        let (resolved_root, children) = match listed {
            Ok(listed) => listed,
            Err(error) => {
                self.issues.push(PluginConfigIssue::coded(
                    &knowledge_root,
                    format!("Could not load the plugin knowledge: {error}"),
                    codes::KNOWLEDGE_INVALID,
                    Some(plugin.source_format),
                    "knowledge",
                ));
                return Vec::new();
            }
        };
        let mut definitions = Vec::new();
        for child in children {
            if definitions.len() >= MAX_PLUGIN_KNOWLEDGE_FOLDERS {
                self.issues.push(PluginConfigIssue::coded(
                    &resolved_root,
                    "The plugin publishes more than the 100 knowledge folders a plugin may",
                    codes::KNOWLEDGE_LIMIT_EXCEEDED,
                    Some(plugin.source_format),
                    "knowledge",
                ));
                break;
            }
            if !child.is_dir() && !is_symlink(&child) {
                continue;
            }
            let entrypoint = child.join("KNOWLEDGE.md");
            let loaded = load_knowledge_folder(&plugin.root, &child, &entrypoint);
            let (source_root, source_entrypoint, metadata) = match loaded {
                Ok(loaded) => loaded,
                Err(error) => {
                    self.issues.push(PluginConfigIssue::coded(
                        &entrypoint,
                        format!("Could not load the plugin knowledge: {error}"),
                        codes::KNOWLEDGE_INVALID,
                        Some(plugin.source_format),
                        "knowledge",
                    ));
                    continue;
                }
            };
            let runtime_root = plugin.data_root.join("knowledge").join(&metadata.name);
            definitions.push(PluginKnowledgeDefinition {
                plugin_name: plugin.name.clone(),
                name: format!("{}:{}", plugin.namespace, metadata.name),
                source_name: metadata.name.clone(),
                description: metadata.description,
                display_name: metadata.display_name,
                icon: metadata.icon,
                source_root,
                source_entrypoint,
                runtime_entrypoint: runtime_root.join("KNOWLEDGE.md"),
                runtime_root,
            });
        }
        definitions
    }

    fn load_agents(
        &mut self,
        plugin: &PluginDescriptor,
        extension: Option<&Extension>,
    ) -> Vec<PluginAgentDefinition> {
        if extension.is_none() {
            return Vec::new();
        }
        let agents_root = plugin.root.join(VIBE_EXTENSION_DIRECTORY).join("agents");
        if !exists_or_link(&agents_root) {
            return Vec::new();
        }
        let listed = contained_component_directory(&plugin.root, &agents_root)
            .and_then(|resolved| Ok((resolved.clone(), sorted_children(&resolved)?)));
        let (resolved_root, children) = match listed {
            Ok(listed) => listed,
            Err(error) => {
                self.issues.push(PluginConfigIssue::coded(
                    &agents_root,
                    format!("Could not load the plugin agents: {error}"),
                    codes::AGENT_INVALID,
                    Some(plugin.source_format),
                    "agent",
                ));
                return Vec::new();
            }
        };
        let mut definitions = Vec::new();
        for child in children {
            if definitions.len() >= MAX_PLUGIN_AGENTS {
                self.issues.push(PluginConfigIssue::coded(
                    &resolved_root,
                    "The plugin declares more than the 128 agents a plugin may",
                    codes::AGENT_LIMIT_EXCEEDED,
                    Some(plugin.source_format),
                    "agent",
                ));
                break;
            }
            if child.extension().and_then(|extension| extension.to_str()) != Some("toml") {
                continue;
            }
            let stem = child
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let loaded = load_agent_document(&plugin.root, &child, &stem);
            let (source_file, document) = match loaded {
                Ok(loaded) => loaded,
                Err(error) => {
                    self.issues.push(PluginConfigIssue::coded(
                        &child,
                        format!("Could not load the plugin agent: {error}"),
                        codes::AGENT_INVALID,
                        Some(plugin.source_format),
                        "agent",
                    ));
                    continue;
                }
            };
            let name = format!("{}:{stem}", plugin.namespace);
            let overrides = document.overrides();
            if let Some(check) = &self.resolver.agent_overrides_check
                && let Err(error) = check(&overrides)
            {
                self.issues.push(PluginConfigIssue::coded(
                    &source_file,
                    format!("Could not apply the plugin agent: {error}"),
                    codes::AGENT_INVALID,
                    Some(plugin.source_format),
                    "agent",
                ));
                continue;
            }
            definitions.push(PluginAgentDefinition {
                plugin_name: plugin.name.clone(),
                name,
                source_name: stem.clone(),
                source_file,
                display_name: document
                    .display_name
                    .clone()
                    .unwrap_or_else(|| python_title(&stem.replace('-', " "))),
                description: document.description.clone(),
                safety: document.safety.clone(),
                instructions: document.instructions.clone(),
                overrides,
                agent_type: "subagent".to_owned(),
            });
        }
        definitions
    }

    fn load_libraries(
        &mut self,
        plugin: &PluginDescriptor,
        extension: Option<&Extension>,
    ) -> Vec<PluginLibraryDefinition> {
        if extension.is_none() {
            return Vec::new();
        }
        let config_file = plugin.root.join("libraries.json");
        if !exists_or_link(&config_file) {
            return Vec::new();
        }
        let loaded = contained_component_file(&plugin.root, &config_file).and_then(|resolved| {
            let text = read_text(&resolved).map_err(|error| error.to_string())?;
            let (node, python) = parse_libraries(&text)?;
            Ok((
                resolved,
                (
                    in_document_order(&text, &["node"], node),
                    in_document_order(&text, &["python"], python),
                ),
            ))
        });
        let (resolved_config, (node, python)) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                self.issues.push(PluginConfigIssue::coded(
                    &config_file,
                    format!("Could not load the plugin libraries: {error}"),
                    codes::LIBRARIES_INVALID,
                    Some(plugin.source_format),
                    "library",
                ));
                return Vec::new();
            }
        };
        let libraries_root = plugin
            .data_root
            .join("libraries")
            .join(&plugin.content_digest);
        let mut definitions = Vec::new();
        for (alias, source) in node {
            let candidate = declared_candidate(&plugin.root, &source);
            let checked =
                contained_component_directory(&plugin.root, &candidate).and_then(|source_path| {
                    validate_contained_tree(&plugin.root, &candidate, &source_path, "library")?;
                    Ok(source_path)
                });
            match checked {
                Ok(source_path) => {
                    let runtime_path = alias.split('/').fold(
                        libraries_root.join("node").join("node_modules"),
                        |path, part| path.join(part),
                    );
                    definitions.push(PluginLibraryDefinition {
                        plugin_name: plugin.name.clone(),
                        language: "node".to_owned(),
                        alias,
                        source_path,
                        runtime_path,
                        config_file: resolved_config.clone(),
                    });
                }
                Err(error) => self.issues.push(PluginConfigIssue::coded(
                    &candidate,
                    format!(
                        "Could not load the Node plugin library {}: {error}",
                        python_repr(&alias)
                    ),
                    codes::LIBRARY_INVALID,
                    Some(plugin.source_format),
                    "library",
                )),
            }
        }
        for (alias, source) in python {
            let candidate = declared_candidate(&plugin.root, &source);
            let checked = if is_symlink(&candidate) {
                Err("a library path cannot be a symbolic link".to_owned())
            } else if candidate.is_dir() {
                contained_component_directory(&plugin.root, &candidate).and_then(|source_path| {
                    validate_contained_tree(&plugin.root, &candidate, &source_path, "library")?;
                    Ok((source_path, alias.clone()))
                })
            } else {
                contained_component_file(&plugin.root, &candidate).and_then(|source_path| {
                    if source_path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        != Some("py")
                    {
                        return Err("a Python library file has to end in .py".to_owned());
                    }
                    Ok((source_path, format!("{alias}.py")))
                })
            };
            match checked {
                Ok((source_path, runtime_name)) => definitions.push(PluginLibraryDefinition {
                    plugin_name: plugin.name.clone(),
                    language: "python".to_owned(),
                    alias,
                    source_path,
                    runtime_path: libraries_root.join("python").join(runtime_name),
                    config_file: resolved_config.clone(),
                }),
                Err(error) => self.issues.push(PluginConfigIssue::coded(
                    &candidate,
                    format!(
                        "Could not load the Python plugin library {}: {error}",
                        python_repr(&alias)
                    ),
                    codes::LIBRARY_INVALID,
                    Some(plugin.source_format),
                    "library",
                )),
            }
        }
        definitions
    }

    fn load_connectors(
        &mut self,
        plugin: &PluginDescriptor,
        extension: Option<&Extension>,
    ) -> Vec<PluginConnectorDefinition> {
        if extension.is_none() {
            return Vec::new();
        }
        let config_file = plugin.root.join("connectors.json");
        if !exists_or_link(&config_file) {
            return Vec::new();
        }
        let loaded = contained_component_file(&plugin.root, &config_file).and_then(|resolved| {
            let text = read_text(&resolved).map_err(|error| error.to_string())?;
            Ok((resolved, parse_connectors(&text)?))
        });
        match loaded {
            Ok((resolved_config, requirements)) => requirements
                .into_iter()
                .map(|(source_id, tools)| PluginConnectorDefinition {
                    plugin_name: plugin.name.clone(),
                    source_id,
                    tools,
                    config_file: resolved_config.clone(),
                })
                .collect(),
            Err(error) => {
                self.issues.push(PluginConfigIssue::coded(
                    &config_file,
                    format!("Could not load the plugin connectors: {error}"),
                    codes::CONNECTORS_INVALID,
                    Some(plugin.source_format),
                    "connector",
                ));
                Vec::new()
            }
        }
    }

    fn remove_library_collisions(
        &mut self,
        definitions: Vec<PluginLibraryDefinition>,
    ) -> Vec<PluginLibraryDefinition> {
        let mut by_alias: BTreeMap<(String, String), Vec<PluginLibraryDefinition>> =
            BTreeMap::new();
        for definition in definitions {
            by_alias
                .entry((definition.language.clone(), definition.alias.clone()))
                .or_default()
                .push(definition);
        }
        let mut selected = Vec::new();
        for ((language, alias), matches) in by_alias {
            if matches.len() == 1 {
                selected.extend(matches);
                continue;
            }
            let mut owners: Vec<&str> = matches
                .iter()
                .map(|item| item.plugin_name.as_str())
                .collect();
            owners.sort_unstable();
            let owners = owners.join(", ");
            for item in &matches {
                self.issues.push(PluginConfigIssue::coded(
                    &item.config_file,
                    format!(
                        "The {language} library alias {} is claimed by more than one plugin: {owners}",
                        python_repr(&alias)
                    ),
                    codes::LIBRARY_ALIAS_COLLISION,
                    Some(DetectedPluginFormat::AgentPlugins10),
                    "library",
                ));
            }
        }
        selected
    }

    fn remove_config_shadowed_mcp_servers(
        &mut self,
        definitions: Vec<PluginMcpServerDefinition>,
        plugins: &BTreeMap<String, PluginDescriptor>,
    ) -> Vec<PluginMcpServerDefinition> {
        let Some(configured) = &self.resolver.configured_mcp_names else {
            return definitions;
        };
        let mut selected = Vec::new();
        for definition in definitions {
            if !configured.contains(&definition.source_id) {
                selected.push(definition);
                continue;
            }
            self.issues.push(PluginConfigIssue {
                file: definition.config_file.clone(),
                message: format!(
                    "The MCP server {} of plugin {} is disabled because a configured MCP server \
                     already has that name and the two would share credentials; rename one of them",
                    python_repr(&definition.source_id),
                    python_repr(&definition.plugin_name),
                ),
                severity: Severity::Warning,
                code: Some(codes::MCP_SERVER_SHADOWED.to_owned()),
                fatal: false,
                source_format: plugins
                    .get(&definition.plugin_name)
                    .map(|plugin| plugin.source_format),
                component: Some("mcp_server".to_owned()),
            });
        }
        selected
    }

    fn load_mcp_servers(&mut self, plugin: &PluginDescriptor) -> Vec<PluginMcpServerDefinition> {
        let config_path = plugin.root.join("mcp.json");
        if !exists_or_link(&config_path) {
            return Vec::new();
        }
        let loaded = resolve_strict(&config_path)
            .map_err(|error| error.to_string())
            .and_then(|resolved| {
                if !is_relative_to(&resolved, &plugin.root) || !resolved.is_file() {
                    return Err(
                        "mcp.json has to resolve to a regular file inside the plugin".to_owned(),
                    );
                }
                let text = read_text(&resolved).map_err(|error| error.to_string())?;
                let raw: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
                let key = if raw.get("mcpServers").is_some() {
                    "mcpServers"
                } else {
                    "mcp_servers"
                };
                Ok((
                    resolved,
                    in_document_order(&text, &[key], parse_mcp_configuration(&raw)?),
                ))
            });
        let (resolved_config, servers) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                self.issues.push(PluginConfigIssue::plain(
                    &config_path,
                    format!("Could not load the MCP configuration: {error}"),
                ));
                return Vec::new();
            }
        };
        let mut definitions = Vec::new();
        for (source_id, raw_server) in servers {
            let parsed = parse_mcp_server(&raw_server);
            let server = parsed.and_then(|parsed| {
                let transport = parsed.kind.clone();
                to_plugin_mcp_server(plugin, &source_id, parsed).map(|server| (server, transport))
            });
            match server {
                Err(error) => self.issues.push(PluginConfigIssue::plain(
                    &resolved_config,
                    format!(
                        "Could not load the MCP server {}: {error}",
                        python_repr(&source_id)
                    ),
                )),
                Ok((None, transport)) => self.issues.push(PluginConfigIssue::plain(
                    &resolved_config,
                    format!(
                        "The MCP server {} uses the unsupported transport {}",
                        python_repr(&source_id),
                        python_repr(&transport)
                    ),
                )),
                Ok((Some(server), _)) => {
                    let private_alias = private_server_alias(&plugin.name, &source_id);
                    definitions.push(PluginMcpServerDefinition {
                        plugin_name: plugin.name.clone(),
                        plugin_namespace: plugin.namespace.clone(),
                        source_id,
                        server: server.with_name(&private_alias),
                        private_alias,
                        config_file: resolved_config.clone(),
                    });
                }
            }
        }
        definitions
    }

    fn load_skills_from_roots(
        &mut self,
        plugin: &PluginDescriptor,
        skill_roots: &[PathBuf],
    ) -> Vec<(String, SkillDefinition)> {
        let mut skills: Vec<(String, SkillDefinition)> = Vec::new();
        for skills_dir in skill_roots {
            if !exists_or_link(skills_dir) {
                continue;
            }
            let listed = resolve_strict(skills_dir)
                .map_err(|error| error.to_string())
                .and_then(|resolved| {
                    if !is_relative_to(&resolved, &plugin.root) {
                        return Err("skills has to resolve inside the plugin".to_owned());
                    }
                    if !resolved.is_dir() {
                        return Err("skills has to resolve to a directory".to_owned());
                    }
                    sorted_children(&resolved)
                });
            let children = match listed {
                Ok(children) => children,
                Err(error) => {
                    self.issues.push(PluginConfigIssue::coded(
                        skills_dir,
                        format!("Could not load the skills: {error}"),
                        codes::PATH_OUTSIDE_ROOT,
                        Some(plugin.source_format),
                        "skill",
                    ));
                    continue;
                }
            };
            for skill_dir in children {
                if !skill_dir.is_dir() {
                    continue;
                }
                let skill_path = skill_dir.join("SKILL.md");
                let expected_name = file_name(&skill_dir);
                let loaded = match resolve_strict(&skill_path) {
                    Err(error) => Err(error.to_string()),
                    Ok(resolved) if !is_relative_to(&resolved, &plugin.root) => {
                        Err("SKILL.md has to resolve inside the plugin".to_owned())
                    }
                    Ok(resolved) if !resolved.is_file() => continue,
                    Ok(resolved) => self.parse_skill(&resolved, plugin).and_then(|skill| {
                        if skill.name == expected_name {
                            Ok((resolved, skill))
                        } else {
                            Err(format!(
                                "the skill name {} does not match its directory {}",
                                python_repr(&skill.name),
                                python_repr(&expected_name)
                            ))
                        }
                    }),
                };
                let (resolved_skill, skill) = match loaded {
                    Ok(loaded) => loaded,
                    Err(error) => {
                        if exists_or_link(&skill_path) {
                            self.issues.push(PluginConfigIssue::coded(
                                &skill_path,
                                format!("Could not load the skill: {error}"),
                                codes::SKILL_INVALID,
                                Some(plugin.source_format),
                                "skill",
                            ));
                        }
                        continue;
                    }
                };
                let alias = format!("{}:{}", plugin.namespace, skill.name);
                if skills.iter().any(|(existing, _)| *existing == alias) {
                    self.issues.push(PluginConfigIssue::coded(
                        &resolved_skill,
                        format!(
                            "The plugin skill alias {} is declared twice",
                            python_repr(&alias)
                        ),
                        codes::SKILL_COLLISION,
                        Some(plugin.source_format),
                        "skill",
                    ));
                    continue;
                }
                let mut skill = skill;
                skill.name.clone_from(&alias);
                skill.source = SkillSource::Plugin;
                skills.push((alias, skill));
            }
        }
        skills
    }

    fn parse_skill(
        &mut self,
        path: &Path,
        plugin: &PluginDescriptor,
    ) -> Result<SkillDefinition, String> {
        let content =
            read_text(path).map_err(|error| format!("the file cannot be read: {error}"))?;
        let (frontmatter, body) =
            parse_skill_markdown(&content).map_err(|error| error.to_string())?;
        let metadata = SkillMetadata::validate(&frontmatter).map_err(|error| error.to_string())?;
        let model_invocable = match openai_policy(path, &plugin.root) {
            Ok(allowed) => allowed,
            Err(reason) => {
                self.issues.push(PluginConfigIssue::coded(
                    crate::skills::openai_metadata_path(path).unwrap_or_default(),
                    format!("The OpenAI skill metadata is invalid, so the model cannot invoke the skill: {reason}"),
                    codes::SKILL_OPENAI_METADATA_INVALID,
                    Some(plugin.source_format),
                    "skill",
                ));
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
            model_invocable: model_invocable && !metadata.disable_model_invocation,
            body: body.trim().to_owned(),
            source: SkillSource::Local,
            scope: plugin.scope,
            path: Some(skill_file_path(path)),
            registry: None,
        })
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
                        python_repr(&name)
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
                        python_repr(&namespace)
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

const fn hook_source(scope: SkillScope) -> &'static str {
    match scope {
        SkillScope::Project => "project_plugin",
        _ => "global_plugin",
    }
}

fn adapt_mcp_servers(
    plugin: &PluginDescriptor,
    servers: &[AdaptedMcpServer],
) -> Vec<PluginMcpServerDefinition> {
    servers
        .iter()
        .map(|adapted| {
            let private_alias = private_server_alias(&plugin.name, &adapted.source_id);
            PluginMcpServerDefinition {
                plugin_name: plugin.name.clone(),
                plugin_namespace: plugin.namespace.clone(),
                source_id: adapted.source_id.clone(),
                server: adapted.server.with_name(&private_alias),
                private_alias,
                config_file: adapted.config_file.clone(),
            }
        })
        .collect()
}

/// The private routing alias of a plugin-owned MCP server: a BLAKE2s digest of
/// the plugin name and the source id, then the identifier-safe source id.
/// Reference `_private_server_alias`.
#[must_use]
pub fn private_server_alias(plugin_name: &str, source_id: &str) -> String {
    let identity = format!("{plugin_name}\0{source_id}");
    let digest = blake2s_hex(identity.as_bytes(), 8);
    let source = typescript_identifier(if source_id.is_empty() {
        "server"
    } else {
        source_id
    });
    let source: String = source.chars().take(80).collect();
    format!("plugin_{digest}_{source}")
}

/// The frontmatter a synthesized skill's `SKILL.md` carries. Reference
/// `_render_generated_skill`.
#[must_use]
pub fn render_generated_skill(definition: &AdaptedSkill) -> String {
    let mut lines = vec![
        "---".to_owned(),
        format!(
            "name: {}",
            crate::hooks::python_json_dumps(&Value::String(definition.source_name.clone()))
        ),
        format!(
            "description: {}",
            crate::hooks::python_json_dumps(&Value::String(definition.description.clone()))
        ),
    ];
    if !definition.allowed_tools.is_empty() {
        lines.push(format!(
            "allowed-tools: {}",
            crate::hooks::python_json_dumps(&Value::Array(
                definition
                    .allowed_tools
                    .iter()
                    .map(|tool| Value::String(tool.clone()))
                    .collect()
            ))
        ));
    }
    if !definition.model_invocable {
        lines.push("disable-model-invocation: true".to_owned());
    }
    lines.extend([
        "---".to_owned(),
        String::new(),
        definition.prompt.clone(),
        String::new(),
    ]);
    lines.join("\n")
}

/// The path of a skill read from disk, with its directory resolved and its
/// file name kept. Reference `SkillInfo.from_metadata`.
fn skill_file_path(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolve_lax(parent).join(name),
        _ => path.to_path_buf(),
    }
}

fn openai_policy(path: &Path, root: &Path) -> Result<bool, String> {
    if let Some(metadata) = crate::skills::openai_metadata_path(path)
        && metadata.is_file()
    {
        let resolved = resolve_strict(&metadata)
            .map_err(|error| format!("agents/openai.yaml cannot be read: {error}"))?;
        if !is_relative_to(&resolved, &resolve_lax(root)) {
            return Err("agents/openai.yaml has to resolve inside the plugin".to_owned());
        }
    }
    crate::skills::openai_invocation_policy(path)
}

fn parse_manifest(path: &Path) -> Result<Manifest, String> {
    let text = read_text(path).map_err(|error| error.to_string())?;
    let raw: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    let mut errors = ValidationErrors::default();
    let allowed = [
        "$schema",
        "schema_id",
        "name",
        "version",
        "description",
        "author",
        "homepage",
        "repository",
        "license",
        "keywords",
        "extensions",
    ];
    let Some(map) = strict::object(&raw, "", &allowed, &mut errors) else {
        return Err(errors.render("plugin manifest"));
    };
    let schema = map.get("$schema").or_else(|| map.get("schema_id"));
    match schema {
        Some(Value::String(schema)) if schema == super::compatibility::NATIVE_SCHEMA => {}
        None => errors.push("$schema", "is required"),
        Some(_) => errors.push("$schema", "must name the Agent Plugins 1.0 schema"),
    }
    let name = strict::required_string(map, "", "name", &mut errors);
    if let Some(name) = &name {
        let length = char_len(name);
        if length == 0 || length > 64 {
            errors.push("name", "must hold 1 to 64 characters");
        } else if !PLUGIN_NAME.is_match(name) {
            errors.push(
                "name",
                "must be lowercase letters, digits, dots and hyphens",
            );
        } else if name.contains("--") || name.contains("..") {
            errors.push("name", "cannot repeat a separator");
        }
    }
    let version = strict::optional_string(map, "", "version", &mut errors);
    let description = strict::optional_string(map, "", "description", &mut errors);
    let author = match map.get("author") {
        None | Some(Value::Null) => Some(None),
        Some(value) => {
            let mut author_errors = ValidationErrors::default();
            let fields = strict::object(
                value,
                "author",
                &["name", "email", "url"],
                &mut author_errors,
            );
            let parsed = fields.map(|fields| {
                (
                    strict::optional_string(fields, "author", "name", &mut author_errors),
                    strict::optional_string(fields, "author", "email", &mut author_errors),
                    strict::optional_string(fields, "author", "url", &mut author_errors),
                )
            });
            let valid = author_errors.is_empty();
            errors.0.extend(author_errors.0);
            match parsed {
                Some((Some(name), Some(email), Some(url))) if valid => {
                    Some(Some((name, email, url)))
                }
                _ => None,
            }
        }
    };
    let homepage = strict::optional_string(map, "", "homepage", &mut errors);
    let repository = strict::optional_string(map, "", "repository", &mut errors);
    let license = strict::optional_string(map, "", "license", &mut errors);
    let keywords = strict::string_list(map, "", "keywords", true, &mut errors);
    let extensions = match map.get("extensions") {
        None | Some(Value::Null) => Some(None),
        Some(Value::Object(entries)) => Some(Some(entries.clone())),
        Some(_) => {
            errors.push("extensions", "must be an object or null");
            None
        }
    };
    if !errors.is_empty() {
        return Err(errors.render("plugin manifest"));
    }
    let (
        Some(name),
        Some(version),
        Some(description),
        Some(author),
        Some(homepage),
        Some(repository),
        Some(license),
        Some(keywords),
        Some(extensions),
    ) = (
        name,
        version,
        description,
        author,
        homepage,
        repository,
        license,
        keywords,
        extensions,
    )
    else {
        return Err(errors.render("plugin manifest"));
    };
    let optional = |value: &Option<String>| value.clone().map_or(Value::Null, Value::String);
    let dump = serde_json::json!({
        "schema_id": super::compatibility::NATIVE_SCHEMA,
        "name": name,
        "version": optional(&version),
        "description": optional(&description),
        "author": author.as_ref().map_or(Value::Null, |(name, email, url)| serde_json::json!({
            "name": optional(name),
            "email": optional(email),
            "url": optional(url),
        })),
        "homepage": optional(&homepage),
        "repository": optional(&repository),
        "license": optional(&license),
        "keywords": keywords.clone().map_or(Value::Null, |keywords| Value::Array(keywords.into_iter().map(Value::String).collect())),
        "extensions": extensions.clone().map_or(Value::Null, Value::Object),
    });
    Ok(Manifest {
        name,
        version,
        description,
        author_name: author.and_then(|(name, _, _)| name),
        extensions,
        dump,
    })
}

fn parse_extension(raw: &Value) -> Result<Extension, String> {
    let mut errors = ValidationErrors::default();
    let allowed = [
        "schemaVersion",
        "schema_version",
        "toolNamespace",
        "tool_namespace",
        "toolOverrides",
        "tool_overrides",
    ];
    let Some(map) = strict::object(raw, "", &allowed, &mut errors) else {
        return Err(errors.render("ai.mistral.vibe extension"));
    };
    let mut aliased = map.clone();
    for (alias, name) in [
        ("schemaVersion", "schema_version"),
        ("toolNamespace", "tool_namespace"),
        ("toolOverrides", "tool_overrides"),
    ] {
        if let Some(value) = map.get(alias).or_else(|| map.get(name)) {
            aliased.insert(alias.to_owned(), value.clone());
        }
    }
    strict::integer_literal(&aliased, "", "schemaVersion", 1, true, &mut errors);
    let tool_namespace = strict::optional_string(&aliased, "", "toolNamespace", &mut errors);
    if let Some(Some(namespace)) = &tool_namespace
        && !TYPESCRIPT_IDENTIFIER.is_match(namespace)
    {
        errors.push("toolNamespace", "must be an identifier");
    }
    let mut tool_overrides = BTreeMap::new();
    match aliased.get("toolOverrides") {
        None => {}
        Some(Value::Object(overrides)) => {
            for (key, value) in overrides {
                let location = at("toolOverrides", key);
                let Some(fields) =
                    strict::object(value, &location, &["name", "exposure"], &mut errors)
                else {
                    continue;
                };
                let name = strict::optional_string(fields, &location, "name", &mut errors);
                if let Some(Some(name)) = &name
                    && !TYPESCRIPT_IDENTIFIER.is_match(name)
                {
                    errors.push(at(&location, "name"), "must be an identifier");
                }
                let exposure = match fields.get("exposure") {
                    None | Some(Value::Null) => Some(None),
                    _ => strict::string_choice(
                        fields,
                        &location,
                        "exposure",
                        &["programmatic", "direct", "direct_and_programmatic"],
                        &mut errors,
                    ),
                };
                if let (Some(name), Some(exposure)) = (name, exposure) {
                    tool_overrides.insert(key.clone(), PluginToolOverride { name, exposure });
                }
            }
        }
        Some(_) => errors.push("toolOverrides", "must be an object"),
    }
    if !errors.is_empty() {
        return Err(errors.render("ai.mistral.vibe extension"));
    }
    Ok(Extension {
        tool_namespace: tool_namespace.flatten(),
        tool_overrides,
    })
}

/// The validated knowledge frontmatter. Reference `_KnowledgeMetadata`.
struct KnowledgeMetadata {
    name: String,
    description: String,
    display_name: Option<String>,
    icon: Option<String>,
}

fn load_knowledge_folder(
    plugin_root: &Path,
    child: &Path,
    entrypoint: &Path,
) -> Result<(PathBuf, PathBuf, KnowledgeMetadata), String> {
    let source_root = contained_component_directory(plugin_root, child)?;
    validate_contained_tree(plugin_root, &source_root, &source_root, "knowledge")?;
    let source_entrypoint = contained_component_file(plugin_root, entrypoint)?;
    let size = fs::metadata(&source_entrypoint)
        .map_err(|error| error.to_string())?
        .len();
    if size > MAX_PLUGIN_KNOWLEDGE_ENTRYPOINT_BYTES {
        return Err("KNOWLEDGE.md is larger than the 262144-byte entrypoint limit".to_owned());
    }
    let content = read_text(&source_entrypoint).map_err(|error| error.to_string())?;
    let (frontmatter, _) = parse_skill_markdown(&content).map_err(|error| error.to_string())?;
    let metadata = parse_knowledge_metadata(&Value::Object(frontmatter))?;
    let directory = file_name(child);
    if metadata.name != directory {
        return Err(format!(
            "the knowledge name {} does not match its directory {}",
            python_repr(&metadata.name),
            python_repr(&directory)
        ));
    }
    Ok((source_root, source_entrypoint, metadata))
}

fn parse_knowledge_metadata(raw: &Value) -> Result<KnowledgeMetadata, String> {
    let mut errors = ValidationErrors::default();
    let Some(map) = strict::object(
        raw,
        "",
        &["name", "description", "display_name", "icon"],
        &mut errors,
    ) else {
        return Err(errors.render("knowledge metadata"));
    };
    let name = strict::required_string(map, "", "name", &mut errors);
    if let Some(name) = &name
        && (char_len(name) == 0 || char_len(name) > 64 || !COMPONENT_NAME.is_match(name))
    {
        errors.push(
            "name",
            "must be a lowercase kebab-case name of 1 to 64 characters",
        );
    }
    let description = strict::required_string(map, "", "description", &mut errors);
    if let Some(description) = &description
        && !(5..=300).contains(&char_len(description))
    {
        errors.push("description", "must hold 5 to 300 characters");
    }
    let display_name = strict::optional_string(map, "", "display_name", &mut errors);
    if let Some(Some(display_name)) = &display_name
        && !(1..=100).contains(&char_len(display_name))
    {
        errors.push("display_name", "must hold 1 to 100 characters");
    }
    let icon = strict::optional_string(map, "", "icon", &mut errors);
    if let Some(Some(icon)) = &icon
        && !(1..=16).contains(&char_len(icon))
    {
        errors.push("icon", "must hold 1 to 16 characters");
    }
    match (name, description, display_name, icon) {
        (Some(name), Some(description), Some(display_name), Some(icon)) if errors.is_empty() => {
            Ok(KnowledgeMetadata {
                name,
                description,
                display_name,
                icon,
            })
        }
        _ => Err(errors.render("knowledge metadata")),
    }
}

/// The validated agent document. Reference `_PluginAgentDocument`.
struct AgentDocument {
    display_name: Option<String>,
    description: String,
    safety: String,
    active_model: Option<String>,
    instructions: Option<String>,
    enabled_tools: Vec<String>,
    disabled_tools: Vec<String>,
    tools: Vec<(String, Map<String, Value>)>,
}

impl AgentDocument {
    /// The profile overrides the document declares. Reference
    /// `PluginResolver._load_agents`.
    fn overrides(&self) -> Map<String, Value> {
        let mut overrides = Map::new();
        if let Some(model) = &self.active_model {
            overrides.insert("active_model".to_owned(), Value::String(model.clone()));
        }
        let strings =
            |items: &[String]| Value::Array(items.iter().cloned().map(Value::String).collect());
        if !self.enabled_tools.is_empty() {
            overrides.insert("enabled_tools".to_owned(), strings(&self.enabled_tools));
        }
        if !self.disabled_tools.is_empty() {
            overrides.insert("disabled_tools".to_owned(), strings(&self.disabled_tools));
        }
        if !self.tools.is_empty() {
            overrides.insert(
                "tools".to_owned(),
                Value::Object(
                    self.tools
                        .iter()
                        .map(|(name, config)| (name.clone(), Value::Object(config.clone())))
                        .collect(),
                ),
            );
        }
        overrides
    }
}

fn load_agent_document(
    plugin_root: &Path,
    child: &Path,
    stem: &str,
) -> Result<(PathBuf, AgentDocument), String> {
    let source_file = contained_component_file(plugin_root, child)?;
    let size = fs::metadata(&source_file)
        .map_err(|error| error.to_string())?
        .len();
    if size > MAX_PLUGIN_AGENT_BYTES {
        return Err("the agent TOML is larger than the 65536-byte component limit".to_owned());
    }
    if !COMPONENT_NAME.is_match(stem) {
        return Err("an agent file name has to be a lowercase kebab-case name".to_owned());
    }
    let text = read_text(&source_file).map_err(|error| error.to_string())?;
    let table: toml::Table = text
        .parse()
        .map_err(|error: toml::de::Error| error.message().to_owned())?;
    let document = parse_agent_document(&strict::toml_to_json(&toml::Value::Table(table)))?;
    Ok((source_file, document))
}

fn parse_agent_document(raw: &Value) -> Result<AgentDocument, String> {
    let mut errors = ValidationErrors::default();
    let allowed = [
        "schema_version",
        "agent_type",
        "display_name",
        "description",
        "safety",
        "active_model",
        "instructions",
        "enabled_tools",
        "disabled_tools",
        "tools",
    ];
    let Some(map) = strict::object(raw, "", &allowed, &mut errors) else {
        return Err(errors.render("plugin agent"));
    };
    strict::integer_literal(map, "", "schema_version", 1, true, &mut errors);
    match map.get("agent_type") {
        Some(Value::String(kind)) if kind == "subagent" => {}
        None => errors.push("agent_type", "is required"),
        Some(_) => errors.push("agent_type", "must be subagent"),
    }
    let display_name = strict::optional_string(map, "", "display_name", &mut errors);
    if let Some(Some(display_name)) = &display_name
        && !(1..=100).contains(&char_len(display_name))
    {
        errors.push("display_name", "must hold 1 to 100 characters");
    }
    let description = strict::required_string(map, "", "description", &mut errors);
    if let Some(description) = &description
        && !(1..=300).contains(&char_len(description))
    {
        errors.push("description", "must hold 1 to 300 characters");
    }
    let safety = strict::string_choice(
        map,
        "",
        "safety",
        &["safe", "neutral", "destructive", "yolo"],
        &mut errors,
    );
    let active_model = strict::optional_string(map, "", "active_model", &mut errors);
    if let Some(Some(model)) = &active_model
        && model.is_empty()
    {
        errors.push("active_model", "cannot be empty");
    }
    let instructions = strict::optional_string(map, "", "instructions", &mut errors);
    if let Some(Some(instructions)) = &instructions
        && char_len(instructions) > 65_536
    {
        errors.push("instructions", "is longer than 65536 characters");
    }
    let tool_list = |key: &str, errors: &mut ValidationErrors| {
        let list = strict::string_list(map, "", key, false, errors).map(Option::unwrap_or_default);
        if let Some(items) = &list {
            if items.len() > 128 {
                errors.push(key, "holds more than 128 names");
            }
            let unique: BTreeSet<&String> = items.iter().collect();
            if unique.len() != items.len() {
                errors.push(key, "repeats a name");
            }
            if items.iter().any(String::is_empty) {
                errors.push(key, "holds an empty name");
            }
        }
        list
    };
    let enabled_tools = tool_list("enabled_tools", &mut errors);
    let disabled_tools = tool_list("disabled_tools", &mut errors);
    let mut tools = Vec::new();
    match map.get("tools") {
        None => {}
        Some(Value::Object(entries)) => {
            if entries.len() > 128 {
                errors.push("tools", "holds more than 128 tools");
            }
            for (name, value) in entries {
                let location = at("tools", name);
                let Some(fields) = strict::object(
                    value,
                    &location,
                    &["permission", "allowlist", "denylist"],
                    &mut errors,
                ) else {
                    continue;
                };
                let mut config = Map::new();
                match fields.get("permission") {
                    None | Some(Value::Null) => {}
                    _ => {
                        if let Some(Some(permission)) = strict::string_choice(
                            fields,
                            &location,
                            "permission",
                            &["always", "ask", "never"],
                            &mut errors,
                        ) {
                            config.insert("permission".to_owned(), Value::String(permission));
                        }
                    }
                }
                for key in ["allowlist", "denylist"] {
                    let list = strict::string_list(fields, &location, key, false, &mut errors)
                        .map(Option::unwrap_or_default);
                    if let Some(items) = list {
                        let unique: BTreeSet<&String> = items.iter().collect();
                        if unique.len() != items.len() {
                            errors.push(at(&location, key), "repeats a pattern");
                        }
                        config.insert(
                            key.to_owned(),
                            Value::Array(items.into_iter().map(Value::String).collect()),
                        );
                    }
                }
                tools.push((name.clone(), config));
            }
        }
        Some(_) => errors.push("tools", "must be a table"),
    }
    match (
        description,
        safety,
        display_name,
        active_model,
        instructions,
        enabled_tools,
        disabled_tools,
    ) {
        (
            Some(description),
            Some(safety),
            Some(display_name),
            Some(active_model),
            Some(instructions),
            Some(enabled_tools),
            Some(disabled_tools),
        ) if errors.is_empty() => Ok(AgentDocument {
            display_name,
            description,
            safety: safety.unwrap_or_else(|| "neutral".to_owned()),
            active_model,
            instructions,
            enabled_tools,
            disabled_tools,
            tools,
        }),
        _ => Err(errors.render("plugin agent")),
    }
}

type LibraryEntries = (Vec<(String, String)>, Vec<(String, String)>);

fn parse_libraries(text: &str) -> Result<LibraryEntries, String> {
    let raw: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let mut errors = ValidationErrors::default();
    let allowed = ["schemaVersion", "schema_version", "node", "python"];
    let Some(map) = strict::object(&raw, "", &allowed, &mut errors) else {
        return Err(errors.render("plugin libraries"));
    };
    let mut aliased = map.clone();
    if let Some(value) = map
        .get("schemaVersion")
        .or_else(|| map.get("schema_version"))
    {
        aliased.insert("schemaVersion".to_owned(), value.clone());
    }
    strict::integer_literal(&aliased, "", "schemaVersion", 1, true, &mut errors);
    let read = |language: &str, label: &str, errors: &mut ValidationErrors| {
        let entries =
            strict::string_map(map, "", language, errors).map(Option::unwrap_or_default)?;
        if entries.len() > 128 {
            errors.push(language, "holds more than 128 libraries");
            return None;
        }
        for (alias, source) in &entries {
            let valid_alias = if language == "node" {
                char_len(alias) <= MAX_NODE_LIBRARY_ALIAS_LENGTH
                    && NODE_LIBRARY_ALIAS.is_match(alias)
            } else {
                char_len(alias) <= MAX_PYTHON_LIBRARY_ALIAS_LENGTH
                    && PYTHON_LIBRARY_ALIAS.is_match(alias)
            };
            if !valid_alias {
                errors.push(
                    language,
                    format!(
                        "{} is not a valid {label} package alias",
                        python_repr(alias)
                    ),
                );
                return None;
            }
            if let Err(error) = validate_plugin_relative_path(source, label) {
                errors.push(language, error);
                return None;
            }
        }
        let mut entries = entries;
        entries.sort();
        Some(entries)
    };
    let node = read("node", "Node library", &mut errors);
    let python = read("python", "Python library", &mut errors);
    match (node, python) {
        (Some(node), Some(python)) if errors.is_empty() => Ok((node, python)),
        _ => Err(errors.render("plugin libraries")),
    }
}

fn parse_connectors(text: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let raw: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let mut errors = ValidationErrors::default();
    let allowed = ["schemaVersion", "schema_version", "connectors"];
    let Some(map) = strict::object(&raw, "", &allowed, &mut errors) else {
        return Err(errors.render("plugin connectors"));
    };
    let mut aliased = map.clone();
    if let Some(value) = map
        .get("schemaVersion")
        .or_else(|| map.get("schema_version"))
    {
        aliased.insert("schemaVersion".to_owned(), value.clone());
    }
    strict::integer_literal(&aliased, "", "schemaVersion", 1, true, &mut errors);
    let mut requirements = Vec::new();
    match map.get("connectors") {
        None => errors.push("connectors", "is required"),
        Some(Value::Array(items)) => {
            if items.len() > 128 {
                errors.push("connectors", "holds more than 128 connectors");
            }
            for (index, item) in items.iter().enumerate() {
                let location = at("connectors", &index.to_string());
                let Some(fields) = strict::object(item, &location, &["id", "tools"], &mut errors)
                else {
                    continue;
                };
                let id = strict::required_string(fields, &location, "id", &mut errors);
                if let Some(id) = &id
                    && !(1..=256).contains(&char_len(id))
                {
                    errors.push(at(&location, "id"), "must hold 1 to 256 characters");
                }
                let tools = match fields.get("tools") {
                    None => {
                        errors.push(at(&location, "tools"), "is required");
                        None
                    }
                    Some(_) => strict::string_list(fields, &location, "tools", false, &mut errors)
                        .flatten(),
                };
                if let Some(tools) = &tools {
                    if tools.is_empty() || tools.len() > 256 {
                        errors.push(at(&location, "tools"), "must hold 1 to 256 tools");
                    }
                    let unique: BTreeSet<&String> = tools.iter().collect();
                    if unique.len() != tools.len() {
                        errors.push(at(&location, "tools"), "repeats a tool");
                    }
                    if tools.iter().any(|tool| {
                        tool.is_empty() || char_len(tool) > MAX_CONNECTOR_TOOL_NAME_LENGTH
                    }) {
                        errors.push(
                            at(&location, "tools"),
                            "holds a name outside 1 to 256 characters",
                        );
                    }
                }
                if let (Some(id), Some(tools)) = (id, tools) {
                    requirements.push((id, tools));
                }
            }
            let ids: BTreeSet<&String> = requirements.iter().map(|(id, _)| id).collect();
            if ids.len() != requirements.len() {
                errors.push("connectors", "repeats a connector id");
            }
        }
        Some(_) => errors.push("connectors", "must be a list"),
    }
    if errors.is_empty() {
        Ok(requirements)
    } else {
        Err(errors.render("plugin connectors"))
    }
}

/// One validated `mcpServers` entry. Reference `_MCPStdioServer`,
/// `_MCPStreamableHTTPServer` and `_MCPSSEServer`.
struct ParsedMcpServer {
    kind: String,
    command: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<String>,
    url: String,
    headers: Vec<(String, String)>,
}

/// `entries` in the order the object at `pointer` lists its keys in `text`,
/// as a Python `dict` read from the same document iterates them; this build's
/// `serde_json::Map` sorts them. Entries the document does not name keep
/// their relative order at the end.
fn in_document_order<T>(
    text: &str,
    pointer: &[&str],
    mut entries: Vec<(String, T)>,
) -> Vec<(String, T)> {
    let Some(order) = super::foreign::object_key_order(text, pointer) else {
        return entries;
    };
    entries.sort_by_key(|(key, _)| {
        order
            .iter()
            .position(|named| named == key)
            .unwrap_or(usize::MAX)
    });
    entries
}

fn parse_mcp_configuration(raw: &Value) -> Result<Vec<(String, Value)>, String> {
    let mut errors = ValidationErrors::default();
    let allowed = ["$schema", "schema_id", "mcpServers", "mcp_servers"];
    let Some(map) = strict::object(raw, "", &allowed, &mut errors) else {
        return Err(errors.render("MCP configuration"));
    };
    match map.get("$schema").or_else(|| map.get("schema_id")) {
        Some(Value::String(schema)) if schema == MCP_SCHEMA => {}
        None => errors.push("$schema", "is required"),
        Some(_) => errors.push("$schema", "must name the Agent Plugins 1.0 MCP schema"),
    }
    let servers = match map.get("mcpServers").or_else(|| map.get("mcp_servers")) {
        None => {
            errors.push("mcpServers", "is required");
            None
        }
        Some(Value::Object(servers)) => Some(
            servers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        ),
        Some(_) => {
            errors.push("mcpServers", "must be an object");
            None
        }
    };
    match servers {
        Some(servers) if errors.is_empty() => Ok(servers),
        _ => Err(errors.render("MCP configuration")),
    }
}

fn parse_mcp_server(raw: &Value) -> Result<ParsedMcpServer, String> {
    let mut errors = ValidationErrors::default();
    let kind = match raw {
        Value::Object(map) => match map.get("type") {
            Some(Value::String(kind))
                if ["stdio", "streamable-http", "sse"].contains(&kind.as_str()) =>
            {
                kind.clone()
            }
            Some(_) => {
                return Err("the server type must be stdio, streamable-http or sse".to_owned());
            }
            None => return Err("the server declares no type".to_owned()),
        },
        _ => return Err("a server must be an object".to_owned()),
    };
    let allowed: &[&str] = if kind == "stdio" {
        &["type", "command", "args", "env", "cwd"]
    } else {
        &["type", "url", "headers"]
    };
    let Some(map) = strict::object(raw, "", allowed, &mut errors) else {
        return Err(errors.render("MCP server"));
    };
    let mut parsed = ParsedMcpServer {
        kind: kind.clone(),
        command: String::new(),
        args: Vec::new(),
        env: Vec::new(),
        cwd: None,
        url: String::new(),
        headers: Vec::new(),
    };
    if kind == "stdio" {
        if let Some(command) = strict::required_string(map, "", "command", &mut errors) {
            if command.is_empty() {
                errors.push("command", "cannot be empty");
            }
            parsed.command = command;
        }
        if let Some(args) = strict::string_list(map, "", "args", false, &mut errors) {
            parsed.args = args.unwrap_or_default();
        }
        if let Some(env) = strict::string_map(map, "", "env", &mut errors) {
            let env = env.unwrap_or_default();
            let mut collision: Vec<&str> = env
                .iter()
                .map(|(name, _)| name.as_str())
                .filter(|name| {
                    let reserved = ["PLUGIN_ROOT", "PLUGIN_DATA"];
                    reserved.contains(name)
                        || (cfg!(windows) && reserved.contains(&name.to_uppercase().as_str()))
                })
                .collect();
            collision.sort_unstable();
            if !collision.is_empty() {
                errors.push(
                    "env",
                    format!(
                        "cannot define the reserved variables {}",
                        collision.join(", ")
                    ),
                );
            }
            parsed.env = env;
        }
        if let Some(cwd) = strict::optional_string(map, "", "cwd", &mut errors) {
            parsed.cwd = cwd;
        }
    } else {
        if let Some(url) = strict::required_string(map, "", "url", &mut errors) {
            if url.is_empty() {
                errors.push("url", "cannot be empty");
            }
            parsed.url = url;
        }
        if let Some(headers) = strict::string_map(map, "", "headers", &mut errors) {
            let headers = headers.unwrap_or_default();
            if let Err(error) = validate_http_headers(&headers) {
                errors.push("headers", error);
            }
            parsed.headers = headers;
        }
    }
    if errors.is_empty() {
        Ok(parsed)
    } else {
        Err(errors.render("MCP server"))
    }
}

fn validate_http_headers(headers: &[(String, String)]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for (name, value) in headers {
        if !HTTP_HEADER_NAME.is_match(name) {
            return Err(format!(
                "{} is not a valid HTTP header name",
                python_repr(name)
            ));
        }
        if !seen.insert(name.to_lowercase()) {
            return Err(format!("the HTTP header {} is repeated", python_repr(name)));
        }
        if value.chars().any(|character| {
            character != '\t'
                && (u32::from(character) < 0x20
                    || u32::from(character) == 0x7f
                    || u32::from(character) > 0xff)
        }) {
            return Err(format!(
                "the value of the HTTP header {} is invalid",
                python_repr(name)
            ));
        }
    }
    Ok(())
}

fn to_plugin_mcp_server(
    plugin: &PluginDescriptor,
    source_id: &str,
    parsed: ParsedMcpServer,
) -> Result<Option<PluginMcpServer>, String> {
    let private_alias = private_server_alias(&plugin.name, source_id);
    match parsed.kind.as_str() {
        "stdio" => {
            let command = resolve_plugin_command(&plugin.root, &parsed.command)?;
            let args = parsed
                .args
                .iter()
                .map(|value| expand_plugin_variables(value, &plugin.root, &plugin.data_root))
                .collect();
            let mut env: BTreeMap<String, String> = parsed
                .env
                .iter()
                .map(|(name, value)| {
                    (
                        name.clone(),
                        expand_plugin_variables(value, &plugin.root, &plugin.data_root),
                    )
                })
                .collect();
            env.insert("PLUGIN_ROOT".to_owned(), path_text(&plugin.root));
            env.insert("PLUGIN_DATA".to_owned(), path_text(&plugin.data_root));
            let cwd = resolve_plugin_cwd(plugin, parsed.cwd.as_deref())?;
            Ok(Some(PluginMcpServer::Stdio {
                name: private_alias,
                command: vec![command],
                args,
                env,
                cwd: Some(path_text(&cwd)),
            }))
        }
        "streamable-http" => {
            let url = crate::config::mcp::normalize_mcp_server_url(&parsed.url)
                .map_err(|error| error.to_string())?;
            Ok(Some(PluginMcpServer::Http {
                name: private_alias,
                transport: "streamable-http".to_owned(),
                url,
                headers: parsed.headers.into_iter().collect(),
            }))
        }
        _ => Ok(None),
    }
}

fn resolve_plugin_command(plugin_root: &Path, command: &str) -> Result<String, String> {
    if let Some(relative) = command.strip_prefix("./") {
        let resolved = resolve_lax(&plugin_root.join(relative));
        if !is_relative_to(&resolved, plugin_root) {
            return Err("a stdio command has to resolve inside the plugin".to_owned());
        }
        return Ok(path_text(&resolved));
    }
    if command.contains('/') || command.contains('\\') || command == "." || command == ".." {
        return Err("a stdio command must be a bare executable or start with './'".to_owned());
    }
    Ok(command.to_owned())
}

fn expand_plugin_variables(value: &str, plugin_root: &Path, data_root: &Path) -> String {
    PLUGIN_PLACEHOLDER
        .replace_all(value, |captures: &regex::Captures<'_>| {
            if &captures[0] == "${PLUGIN_ROOT}" {
                path_text(plugin_root)
            } else {
                path_text(data_root)
            }
        })
        .into_owned()
}

fn resolve_plugin_cwd(plugin: &PluginDescriptor, value: Option<&str>) -> Result<PathBuf, String> {
    let Some(value) = value else {
        return Ok(plugin.root.clone());
    };
    let expanded = expand_plugin_variables(value, &plugin.root, &plugin.data_root);
    let (expected_root, candidate) = if let Some(relative) = value.strip_prefix("./") {
        (plugin.root.clone(), plugin.root.join(relative))
    } else if value == "${PLUGIN_ROOT}" || value.starts_with("${PLUGIN_ROOT}/") {
        (plugin.root.clone(), PathBuf::from(&expanded))
    } else if value == "${PLUGIN_DATA}" || value.starts_with("${PLUGIN_DATA}/") {
        (plugin.data_root.clone(), PathBuf::from(&expanded))
    } else {
        return Err(
            "a stdio cwd has to start with './', '${PLUGIN_ROOT}' or '${PLUGIN_DATA}'".to_owned(),
        );
    };
    let resolved = resolve_lax(&candidate);
    if !is_relative_to(&resolved, &resolve_lax(&expected_root)) {
        return Err("the stdio cwd resolves outside the root it names".to_owned());
    }
    Ok(resolved)
}

fn validate_plugin_relative_path(value: &str, component: &str) -> Result<(), String> {
    if char_len(value) > MAX_PLUGIN_COMPONENT_PATH_LENGTH || !value.starts_with("./") {
        return Err(format!(
            "a {component} path has to start with './' and hold at most 1024 characters"
        ));
    }
    let relative = &value[2..];
    let parts: Vec<&str> = relative.split('/').collect();
    let pure = Path::new(relative);
    // `PurePosixPath` folds repeated and trailing slashes away, so an empty
    // part only counts when the whole remainder is empty.
    let meaningful: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|part| !part.is_empty())
        .collect();
    if meaningful.is_empty()
        || pure.is_absolute()
        || value.contains('\\')
        || meaningful.iter().any(|part| *part == "." || *part == "..")
    {
        return Err(format!(
            "a {component} path has to be a contained POSIX path"
        ));
    }
    Ok(())
}

fn declared_candidate(root: &Path, source: &str) -> PathBuf {
    source
        .strip_prefix("./")
        .unwrap_or(source)
        .split('/')
        .filter(|part| !part.is_empty())
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

fn validate_contained_tree(
    plugin_root: &Path,
    requested_root: &Path,
    resolved_root: &Path,
    label: &str,
) -> Result<(), String> {
    if label == "library" && is_symlink(requested_root) {
        return Err("a library path cannot be a symbolic link".to_owned());
    }
    let mut found = Vec::new();
    super::compatibility::walk_files(resolved_root, &mut found);
    for path in found {
        if is_symlink(&path) {
            return Err(format!("a {label} folder cannot hold a symbolic link"));
        }
        let resolved = resolve_strict(&path).map_err(|error| error.to_string())?;
        if !is_relative_to(&resolved, plugin_root) {
            return Err(format!("{label} content resolves outside the plugin"));
        }
        if !resolved.is_dir() && !resolved.is_file() {
            return Err(format!(
                "a {label} folder can hold only files and directories"
            ));
        }
    }
    Ok(())
}

fn contained_component_file(plugin_root: &Path, path: &Path) -> Result<PathBuf, String> {
    let resolved = resolve_strict(path).map_err(|error| error.to_string())?;
    if !is_relative_to(&resolved, plugin_root) || !resolved.is_file() {
        return Err("the component has to resolve to a regular file inside the plugin".to_owned());
    }
    Ok(resolved)
}

fn contained_component_directory(plugin_root: &Path, path: &Path) -> Result<PathBuf, String> {
    let resolved = resolve_strict(path).map_err(|error| error.to_string())?;
    if !is_relative_to(&resolved, plugin_root) || !resolved.is_dir() {
        return Err("the component has to resolve to a directory inside the plugin".to_owned());
    }
    Ok(resolved)
}

fn sorted_children(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let mut children: Vec<PathBuf> = fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    children.sort_by_key(|path| file_name(path));
    Ok(children)
}

fn exists_or_link(path: &Path) -> bool {
    path.exists() || is_symlink(path)
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A host path as text.
#[must_use]
pub fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Python's `repr` of a string, which every message quotes names with.
#[must_use]
pub fn python_repr(value: &str) -> String {
    crate::mcp::render::python_string(value)
}

/// Python's `str.title()`: a letter after a non-letter is uppercased and every
/// other letter lowercased.
#[must_use]
pub fn python_title(value: &str) -> String {
    let mut titled = String::with_capacity(value.len());
    let mut previous_cased = false;
    for character in value.chars() {
        if character.is_alphabetic() {
            if previous_cased {
                titled.extend(character.to_lowercase());
            } else {
                titled.extend(character.to_uppercase());
            }
            previous_cased = true;
        } else {
            titled.push(character);
            previous_cased = false;
        }
    }
    titled
}

#[cfg(test)]
mod native_tests;
