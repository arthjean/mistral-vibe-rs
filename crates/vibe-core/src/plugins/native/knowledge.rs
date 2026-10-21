//! The knowledge folders a native plugin publishes under
//! `ai.mistral.vibe/knowledge`. Reference `PluginResolver._load_knowledge`.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::super::compatibility::read_text;
use super::super::diagnostics::{self as codes, PluginConfigIssue};
use super::super::paths::{exists_or_link, file_name, is_symlink};
use super::super::strict::{self, ValidationErrors, char_len};
use super::contained::{
    contained_component_directory, contained_component_file, sorted_children,
    validate_contained_tree,
};
use super::manifest::Extension;
use super::{
    COMPONENT_NAME, PluginDescriptor, PluginKnowledgeDefinition, Resolve, VIBE_EXTENSION_DIRECTORY,
};
use crate::pycompat;
use crate::skills::parser::parse_skill_markdown;

const MAX_PLUGIN_KNOWLEDGE_FOLDERS: usize = 100;
const MAX_PLUGIN_KNOWLEDGE_ENTRYPOINT_BYTES: u64 = 256 * 1024;

impl Resolve<'_> {
    pub(super) fn load_knowledge(
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
            pycompat::repr_str(&metadata.name),
            pycompat::repr_str(&directory)
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
