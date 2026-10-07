//! Staging a resolved plugin set on disk and connecting its tool sources.
//!
//! Reference `vibe/core/plugins/_materialize.py`. Knowledge folders and
//! libraries are copied under each plugin's data root, so nothing at runtime
//! reads the plugin tree itself; the tool catalog is built from what the
//! declared sources answer (see [`super::catalog`]).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::catalog::{PluginConnectorCatalog, PluginMcpDiscovery, build_tool_catalog};
use super::diagnostics::{self as codes, PluginConfigIssue};
use super::native::{
    PluginKnowledgeDefinition, PluginLibraryDefinition, ResolvedPluginSet, python_repr,
};
use super::paths::{is_relative_to, resolve_strict};

/// The variable the Node loader reads its package roots from.
pub const PLUGIN_NODE_PATHS_ENV: &str = "UNIFIED_HARNESS_PLUGIN_NODE_PATHS";

/// The Node module that installs the loader hooks beside it. This port's own
/// text; only the variable name and the file names are shared with the
/// reference.
const NODE_LOADER_REGISTER: &str = "// Installs the plugin package resolver for this Node process.\n\
import { register } from \"node:module\";\n\
\n\
register(\"./loader-hooks.mjs\", import.meta.url);\n";

/// Loader hooks that retry a bare specifier Node could not resolve against
/// each plugin package root in turn. This port's own text.
const NODE_LOADER_HOOKS: &str = "// Resolves bare specifiers against the plugin package roots.\n\
import { createRequire } from \"node:module\";\n\
import { delimiter, join } from \"node:path\";\n\
import { pathToFileURL } from \"node:url\";\n\
\n\
const packageRoots = (process.env.UNIFIED_HARNESS_PLUGIN_NODE_PATHS || \"\")\n\
  .split(delimiter)\n\
  .filter((root) => root.length > 0)\n\
  .map((root) => createRequire(pathToFileURL(join(root, \"index.cjs\"))));\n\
\n\
function isBare(specifier) {\n\
  return !(specifier.startsWith(\".\") || specifier.startsWith(\"/\") || specifier.includes(\":\"));\n\
}\n\
\n\
export async function resolve(specifier, context, nextResolve) {\n\
  let failure;\n\
  try {\n\
    return await nextResolve(specifier, context);\n\
  } catch (error) {\n\
    failure = error;\n\
  }\n\
  if (isBare(specifier)) {\n\
    for (const requireFrom of packageRoots) {\n\
      let located;\n\
      try {\n\
        located = requireFrom.resolve(specifier);\n\
      } catch {\n\
        continue;\n\
      }\n\
      return nextResolve(pathToFileURL(located).href, context);\n\
    }\n\
  }\n\
  throw failure;\n\
}\n";

/// One tool a plugin source published. Reference `PluginToolDefinition`.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub exposure: String,
}

/// The tools one group publishes. Reference `PluginToolGroup`.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginToolGroup {
    pub plugin_name: String,
    pub name: String,
    pub description: String,
    pub tools: Vec<PluginToolDefinition>,
}

/// Where a published tool executes. Reference `PluginToolRoute`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginToolRoute {
    pub plugin_name: String,
    pub group_name: String,
    pub function_name: String,
    pub source_kind: String,
    pub source_id: String,
    pub source_tool_name: String,
    pub execution_name: String,
    pub schema_fingerprint: String,
}

/// What the declared sources answered with. Reference `PluginToolCatalog`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginToolCatalog {
    pub tool_groups: Vec<PluginToolGroup>,
    pub tool_routes: BTreeMap<(String, String), PluginToolRoute>,
    /// A source that answered, including with an empty catalog.
    pub connected_mcp_sources: BTreeSet<(String, String)>,
    pub connected_connector_sources: BTreeSet<(String, String)>,
}

/// A resolved set with its runtime files staged. Reference
/// `MaterializedPluginSet`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MaterializedPluginSet {
    pub resolution: ResolvedPluginSet,
    pub tool_groups: Vec<PluginToolGroup>,
    pub tool_routes: BTreeMap<(String, String), PluginToolRoute>,
    pub connected_mcp_sources: BTreeSet<(String, String)>,
    pub connected_connector_sources: BTreeSet<(String, String)>,
    pub knowledge: Vec<PluginKnowledgeDefinition>,
    pub libraries: Vec<PluginLibraryDefinition>,
    pub process_environment: BTreeMap<String, String>,
    pub issues: Vec<PluginConfigIssue>,
}

/// Stages a resolution. Reference `PluginMaterializer`.
#[derive(Default)]
pub struct PluginMaterializer<'a> {
    pub mcp_discovery: Option<&'a dyn PluginMcpDiscovery>,
    pub connector_catalog: Option<&'a dyn PluginConnectorCatalog>,
}

impl PluginMaterializer<'_> {
    /// Reference `PluginMaterializer.materialize`. The three stages run in
    /// sequence here; their issues are appended knowledge first, then
    /// libraries, then tools, an order no reader depends on since every
    /// publisher sorts them.
    pub async fn materialize(&self, resolution: ResolvedPluginSet) -> MaterializedPluginSet {
        let mut issues = resolution.issues.clone();
        let knowledge = materialize_knowledge(&resolution, &mut issues);
        let (libraries, process_environment) = materialize_libraries(&resolution, &mut issues);
        let catalog = if resolution.mcp_servers.is_empty() && resolution.connectors.is_empty() {
            PluginToolCatalog::default()
        } else {
            build_tool_catalog(
                &resolution,
                self.mcp_discovery,
                self.connector_catalog,
                &mut issues,
            )
            .await
        };
        MaterializedPluginSet {
            resolution,
            tool_groups: catalog.tool_groups,
            tool_routes: catalog.tool_routes,
            connected_mcp_sources: catalog.connected_mcp_sources,
            connected_connector_sources: catalog.connected_connector_sources,
            knowledge,
            libraries,
            process_environment,
            issues,
        }
    }
}

fn materialize_knowledge(
    resolution: &ResolvedPluginSet,
    issues: &mut Vec<PluginConfigIssue>,
) -> Vec<PluginKnowledgeDefinition> {
    let mut materialized = Vec::new();
    for definition in &resolution.knowledge {
        let Some(plugin) = resolution
            .plugins
            .iter()
            .find(|plugin| plugin.name == definition.plugin_name)
        else {
            continue;
        };
        if let Err(error) = seed_knowledge(definition, &plugin.data_root) {
            issues.push(PluginConfigIssue::coded(
                &definition.runtime_root,
                format!("Could not stage the plugin knowledge: {error}"),
                codes::KNOWLEDGE_MATERIALIZATION_FAILED,
                Some(plugin.source_format),
                "knowledge",
            ));
            continue;
        }
        materialized.push(definition.clone());
    }
    materialized
}

fn seed_knowledge(definition: &PluginKnowledgeDefinition, data_root: &Path) -> io::Result<()> {
    let parent = definition
        .runtime_root
        .parent()
        .ok_or_else(|| io::Error::other("knowledge runtime root has no parent"))?;
    fs::create_dir_all(parent)?;
    if !definition.runtime_root.exists() {
        let temporary = parent.join(format!(".{}-{}", definition.source_name, random_suffix()));
        fs::create_dir(&temporary)?;
        let staged = temporary.join("content");
        let outcome = copy_tree(&definition.source_root, &staged).and_then(|()| {
            match fs::rename(&staged, &definition.runtime_root) {
                Err(error) if definition.runtime_root.exists() => {
                    // Another staging won the race; its copy is the same tree.
                    let _ = error;
                    Ok(())
                }
                other => other,
            }
        });
        let _ = fs::remove_dir_all(&temporary);
        outcome?;
    }
    let data_root = resolve_strict(data_root)?;
    let runtime_root = resolve_strict(&definition.runtime_root)?;
    let entrypoint = resolve_strict(&definition.runtime_entrypoint)?;
    if !is_relative_to(&runtime_root, &data_root)
        || !runtime_root.is_dir()
        || !is_relative_to(&entrypoint, &runtime_root)
        || !entrypoint.is_file()
    {
        return Err(io::Error::other(
            "the staged knowledge path leaves the plugin data root",
        ));
    }
    Ok(())
}

fn materialize_libraries(
    resolution: &ResolvedPluginSet,
    issues: &mut Vec<PluginConfigIssue>,
) -> (Vec<PluginLibraryDefinition>, BTreeMap<String, String>) {
    let mut materialized = Vec::new();
    let node_available = which("node").is_some();
    for definition in &resolution.libraries {
        let Some(plugin) = resolution
            .plugins
            .iter()
            .find(|plugin| plugin.name == definition.plugin_name)
        else {
            continue;
        };
        if definition.language == "node" && !node_available {
            issues.push(PluginConfigIssue::coded(
                &definition.config_file,
                format!(
                    "The Node plugin library {} cannot load because Node.js is not installed",
                    python_repr(&definition.alias)
                ),
                codes::LIBRARY_RUNTIME_UNAVAILABLE,
                Some(plugin.source_format),
                "library",
            ));
            continue;
        }
        if let Err(error) = seed_library(definition, &plugin.data_root) {
            issues.push(PluginConfigIssue::coded(
                &definition.source_path,
                format!(
                    "Could not stage the {} plugin library {}: {error}",
                    definition.language,
                    python_repr(&definition.alias)
                ),
                codes::LIBRARY_MATERIALIZATION_FAILED,
                Some(plugin.source_format),
                "library",
            ));
            continue;
        }
        materialized.push(definition.clone());
    }
    let environment = library_environment(&materialized, resolution).unwrap_or_default();
    (materialized, environment)
}

fn seed_library(definition: &PluginLibraryDefinition, data_root: &Path) -> io::Result<()> {
    let destination = &definition.runtime_path;
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("library runtime path has no parent"))?;
    fs::create_dir_all(parent)?;
    let present = destination.exists() || fs::symlink_metadata(destination).is_ok();
    if !present {
        let name = destination
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let temporary = parent.join(format!(".{name}-{}", random_suffix()));
        let copied = if definition.source_path.is_dir() {
            copy_tree(&definition.source_path, &temporary)
        } else {
            fs::copy(&definition.source_path, &temporary).map(|_| ())
        };
        let outcome = copied.and_then(|()| match fs::rename(&temporary, destination) {
            Err(_) if destination.exists() => Ok(()),
            other => other,
        });
        if temporary.is_dir() {
            let _ = fs::remove_dir_all(&temporary);
        } else {
            let _ = fs::remove_file(&temporary);
        }
        outcome?;
    }
    let data_root = resolve_strict(data_root)?;
    let resolved = resolve_strict(destination)?;
    if !is_relative_to(&resolved, &data_root) {
        return Err(io::Error::other(
            "the staged library path leaves the plugin data root",
        ));
    }
    if definition.source_path.is_dir() != resolved.is_dir()
        || definition.source_path.is_file() != resolved.is_file()
    {
        return Err(io::Error::other(
            "the staged library has the wrong file type",
        ));
    }
    Ok(())
}

fn library_environment(
    libraries: &[PluginLibraryDefinition],
    resolution: &ResolvedPluginSet,
) -> io::Result<BTreeMap<String, String>> {
    let node_roots: BTreeSet<PathBuf> = libraries
        .iter()
        .filter(|definition| definition.language == "node")
        .map(|definition| node_modules_root(&definition.runtime_path))
        .collect::<io::Result<_>>()?;
    let python_roots: BTreeSet<PathBuf> = libraries
        .iter()
        .filter(|definition| definition.language == "python")
        .filter_map(|definition| definition.runtime_path.parent().map(Path::to_path_buf))
        .collect();
    let mut environment = BTreeMap::new();
    if !python_roots.is_empty() {
        environment.insert(
            "PYTHONPATH".to_owned(),
            prepend_search_paths(&python_roots, std::env::var_os("PYTHONPATH")),
        );
    }
    if node_roots.is_empty() {
        return Ok(environment);
    }
    environment.insert(
        "NODE_PATH".to_owned(),
        prepend_search_paths(&node_roots, std::env::var_os("NODE_PATH")),
    );
    environment.insert(
        PLUGIN_NODE_PATHS_ENV.to_owned(),
        join_paths(node_roots.iter().map(PathBuf::as_path)),
    );
    let Some(first) = resolution.plugins.first() else {
        return Ok(environment);
    };
    let loader_root = first
        .data_root
        .parent()
        .map_or_else(PathBuf::new, Path::to_path_buf)
        .join(".runtime")
        .join("node-loader-v1");
    fs::create_dir_all(&loader_root)?;
    let register = loader_root.join("register.mjs");
    write_runtime_file(&register, NODE_LOADER_REGISTER)?;
    write_runtime_file(&loader_root.join("loader-hooks.mjs"), NODE_LOADER_HOOKS)?;
    let register_url = url::Url::from_file_path(resolve_strict(&register)?)
        .map_err(|()| io::Error::other("the loader path is not absolute"))?;
    let loader_option = format!("--import={register_url}");
    let existing = std::env::var("NODE_OPTIONS").unwrap_or_default();
    let options: Vec<&str> = [existing.trim(), loader_option.as_str()]
        .into_iter()
        .filter(|option| !option.is_empty())
        .collect();
    environment.insert("NODE_OPTIONS".to_owned(), options.join(" "));
    Ok(environment)
}

fn node_modules_root(runtime_path: &Path) -> io::Result<PathBuf> {
    runtime_path
        .ancestors()
        .skip(1)
        .find(|parent| {
            parent
                .file_name()
                .is_some_and(|name| name == "node_modules")
        })
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            io::Error::other(format!(
                "the Node library path has no node_modules root: {}",
                runtime_path.display()
            ))
        })
}

fn prepend_search_paths(paths: &BTreeSet<PathBuf>, existing: Option<OsString>) -> String {
    let mut values: Vec<String> = paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    if let Some(existing) = existing.filter(|value| !value.is_empty()) {
        values.push(existing.to_string_lossy().into_owned());
    }
    values.join(path_separator())
}

fn join_paths<'a>(paths: impl Iterator<Item = &'a Path>) -> String {
    paths
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(path_separator())
}

const fn path_separator() -> &'static str {
    if cfg!(windows) { ";" } else { ":" }
}

fn write_runtime_file(path: &Path, content: &str) -> io::Result<()> {
    if fs::read_to_string(path).is_ok_and(|current| current == content) {
        return Ok(());
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}-{}", random_suffix()));
    let outcome = fs::write(&temporary, content).and_then(|()| fs::rename(&temporary, path));
    let _ = fs::remove_file(&temporary);
    outcome
}

/// Copies a tree of plain files and directories.
fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&path, &destination)?;
        } else {
            fs::copy(&path, &destination)?;
        }
    }
    Ok(())
}

fn random_suffix() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default()
            .to_le_bytes();
        bytes.copy_from_slice(&stamp);
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `shutil.which` for a bare name.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|directory| {
        let candidate = directory.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        if cfg!(windows) {
            [".exe", ".cmd", ".bat"]
                .iter()
                .map(|suffix| directory.join(format!("{name}{suffix}")))
                .find(|candidate| candidate.is_file())
        } else {
            None
        }
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}
