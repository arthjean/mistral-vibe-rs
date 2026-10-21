//! The subagent profiles a native plugin declares under
//! `ai.mistral.vibe/agents`. Reference `PluginResolver._load_agents` and
//! `_PluginAgentDocument`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::super::compatibility::read_text;
use super::super::diagnostics::{self as codes, PluginConfigIssue};
use super::super::paths::exists_or_link;
use super::super::strict::{self, ValidationErrors, at, char_len};
use super::contained::{contained_component_directory, contained_component_file, sorted_children};
use super::manifest::Extension;
use super::{
    COMPONENT_NAME, PLUGIN_AGENT_TYPE, PluginAgentDefinition, PluginDescriptor, Resolve,
    VIBE_EXTENSION_DIRECTORY,
};
use crate::pycompat;

const MAX_PLUGIN_AGENTS: usize = 128;
const MAX_PLUGIN_AGENT_BYTES: u64 = 64 * 1024;

impl Resolve<'_> {
    pub(super) fn load_agents(
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
                    .unwrap_or_else(|| pycompat::title(&stem.replace('-', " "))),
                description: document.description.clone(),
                safety: document.safety.clone(),
                instructions: document.instructions.clone(),
                overrides,
            });
        }
        definitions
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
        Some(Value::String(kind)) if kind == PLUGIN_AGENT_TYPE => {}
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
