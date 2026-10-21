//! The libraries a native plugin vendors (`libraries.json`) and the managed
//! connectors it requires (`connectors.json`). Reference
//! `PluginResolver._load_libraries` and `PluginResolver._load_connectors`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::super::compatibility::{DetectedPluginFormat, read_text};
use super::super::diagnostics::{self as codes, PluginConfigIssue};
use super::super::paths::{exists_or_link, is_symlink};
use super::super::strict::{self, ValidationErrors, at, char_len};
use super::contained::{
    contained_component_directory, contained_component_file, declared_candidate,
    validate_contained_tree, validate_plugin_relative_path,
};
use super::manifest::Extension;
use super::{PluginConnectorDefinition, PluginDescriptor, PluginLibraryDefinition, Resolve};
use crate::pycompat;

const MAX_NODE_LIBRARY_ALIAS_LENGTH: usize = 214;
const MAX_PYTHON_LIBRARY_ALIAS_LENGTH: usize = 128;
const MAX_CONNECTOR_TOOL_NAME_LENGTH: usize = 256;

#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static NODE_LIBRARY_ALIAS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:@[a-z0-9][a-z0-9._-]*/)?[a-z0-9][a-z0-9._-]*$")
        .expect("node alias pattern compiles")
});
#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static PYTHON_LIBRARY_ALIAS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("python alias pattern compiles")
});

impl Resolve<'_> {
    pub(super) fn load_libraries(
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
            Ok((resolved, parse_libraries(&text)?))
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
                        pycompat::repr_str(&alias)
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
                        pycompat::repr_str(&alias)
                    ),
                    codes::LIBRARY_INVALID,
                    Some(plugin.source_format),
                    "library",
                )),
            }
        }
        definitions
    }

    pub(super) fn load_connectors(
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

    pub(super) fn remove_library_collisions(
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
                        pycompat::repr_str(&alias)
                    ),
                    codes::LIBRARY_ALIAS_COLLISION,
                    Some(DetectedPluginFormat::AgentPlugins10),
                    "library",
                ));
            }
        }
        selected
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
                        pycompat::repr_str(alias)
                    ),
                );
                return None;
            }
            if let Err(error) = validate_plugin_relative_path(source, label) {
                errors.push(language, error);
                return None;
            }
        }
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
