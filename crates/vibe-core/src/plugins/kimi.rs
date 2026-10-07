//! Reading a Kimi Code plugin.
//!
//! Reference `vibe/core/plugins/_kimi.py`. The manifest is `kimi.plugin.json`
//! at the root, or `.kimi-plugin/plugin.json` when the root one is absent. It
//! names skills, static commands, MCP servers and a flat list of command
//! hooks, which run under the `kimi_code` protocol.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::claude::hook_matcher;
use super::compatibility::{
    AdaptedHook, AdaptedMcpServer, AdaptedPluginPackage, AdaptedSkill, AdaptedUnsupportedComponent,
    DetectedPluginFormat, PluginAdapterDiagnostic, PluginAdapterResult, author_display_name,
    python_strip, read_text, resolve_declared_path, typescript_identifier,
};
use super::diagnostics::Severity;
use super::foreign::{
    adapt_mcp_servers, adapt_skill_file, adapt_static_command, aliased, command_source_name,
    declared_paths, markdown_files_for_declared_path, mcp_mapping, skill_files_for_declared_path,
    truthy, unsupported_component,
};
use super::strict::{self, ValidationErrors, char_len};
use crate::hooks::{HookConfig, HookType};
use crate::skills::SkillScope;

const FORMAT: &str = "kimi_code";
const MAX_HOOK_TIMEOUT_SECONDS: i64 = 600;
/// The default timeout of a Kimi command hook, in seconds.
const DEFAULT_HOOK_TIMEOUT: f64 = 30.0;

#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static PLUGIN_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z0-9][a-z0-9_-]{0,63}$").expect("plugin name pattern compiles")
});

/// The declared fields of `_KimiManifest`: the key a document spells, and the
/// field name `populate_by_name` also accepts when it differs.
const FIELDS: [(&str, Option<&str>); 14] = [
    ("name", None),
    ("version", None),
    ("description", None),
    ("keywords", None),
    ("author", None),
    ("homepage", None),
    ("license", None),
    ("skills", None),
    ("commands", None),
    ("hooks", None),
    ("mcpServers", Some("mcp_servers")),
    ("sessionStart", Some("session_start")),
    ("skillInstructions", Some("skill_instructions")),
    ("interface", None),
];

/// The manifest after validation. Reference `_KimiManifest`.
struct Manifest {
    raw: Map<String, Value>,
    name: String,
    version: Option<String>,
    description: Option<String>,
    skill_instructions: Option<String>,
}

impl Manifest {
    /// A declared field as the model reads it: absent and `null` are both
    /// `None`.
    fn field(&self, alias: &str) -> Option<&Value> {
        let name = FIELDS
            .iter()
            .find(|(key, _)| *key == alias)
            .and_then(|(_, name)| *name)
            .unwrap_or(alias);
        aliased(&self.raw, alias, name).filter(|value| !value.is_null())
    }

    /// Reference `model_extra`.
    fn extras(&self) -> Vec<String> {
        let mut extras: Vec<String> = self
            .raw
            .keys()
            .filter(|key| {
                !FIELDS.iter().any(|(alias, name)| {
                    key.as_str() == *alias
                        || (Some(key.as_str()) == *name && !self.raw.contains_key(*alias))
                })
            })
            .cloned()
            .collect();
        extras.sort();
        extras
    }
}

fn parse_manifest(raw: &Value) -> Result<Manifest, String> {
    let Value::Object(map) = raw else {
        return Err("the manifest has to be an object".to_owned());
    };
    let mut errors = ValidationErrors::default();
    let name = strict::required_string(map, "", "name", &mut errors);
    if let Some(name) = &name
        && !(1..=64).contains(&char_len(name))
    {
        errors.push("name", "must hold 1 to 64 characters");
    }
    let optional = |alias: &str, field: &str, errors: &mut ValidationErrors| match aliased(
        map, alias, field,
    ) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(text)) => Some(Some(text.clone())),
        Some(_) => {
            errors.push(alias, "must be a string or null");
            None
        }
    };
    let version = optional("version", "version", &mut errors);
    let description = optional("description", "description", &mut errors);
    optional("homepage", "homepage", &mut errors);
    optional("license", "license", &mut errors);
    let skill_instructions = optional("skillInstructions", "skill_instructions", &mut errors);
    strict::string_list(map, "", "keywords", true, &mut errors);
    match aliased(map, "sessionStart", "session_start") {
        None | Some(Value::Null) => {}
        Some(Value::Object(session_start)) => {
            strict::required_string(session_start, "sessionStart", "skill", &mut errors);
        }
        Some(_) => errors.push("sessionStart", "must be an object or null"),
    }
    match (name, version, description, skill_instructions) {
        (Some(name), Some(version), Some(description), Some(skill_instructions))
            if errors.is_empty() =>
        {
            if !PLUGIN_NAME.is_match(&name) {
                return Err(
                    "name must be lowercase letters, digits, hyphens and underscores".to_owned(),
                );
            }
            Ok(Manifest {
                raw: map.clone(),
                name,
                version,
                description,
                skill_instructions,
            })
        }
        _ => Err(errors.render("Kimi plugin manifest")),
    }
}

fn diagnostic(
    suffix: &str,
    path: &Path,
    message: impl Into<String>,
    component: &str,
    severity: Severity,
) -> PluginAdapterDiagnostic {
    super::foreign::diagnostic(FORMAT, suffix, path, message, component, severity, false)
}

/// Reads a Kimi Code plugin. Reference `KimiPluginAdapter.adapt`.
#[must_use]
pub fn adapt(root: &Path, data_root_base: &Path, scope: SkillScope) -> PluginAdapterResult {
    let root_manifest = root.join("kimi.plugin.json");
    let nested_manifest = root.join(".kimi-plugin").join("plugin.json");
    let (manifest_path, relative) = if root_manifest.is_file() {
        (root_manifest.clone(), "./kimi.plugin.json")
    } else {
        (nested_manifest.clone(), "./.kimi-plugin/plugin.json")
    };
    let mut diagnostics = Vec::new();
    if root_manifest.is_file() && nested_manifest.is_file() {
        diagnostics.push(diagnostic(
            "manifest_shadowed",
            &nested_manifest,
            "kimi.plugin.json wins over .kimi-plugin/plugin.json.",
            "manifest",
            Severity::Info,
        ));
    }
    let loaded = resolve_declared_path(root, relative).and_then(|resolved| {
        let text = read_text(&resolved).map_err(|error| error.to_string())?;
        let raw: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        Ok((resolved, parse_manifest(&raw)?))
    });
    let (resolved_manifest, manifest) = match loaded {
        Ok(loaded) => loaded,
        Err(error) => {
            diagnostics.push(super::foreign::diagnostic(
                FORMAT,
                "manifest_invalid",
                &manifest_path,
                format!("Could not load the Kimi plugin manifest: {error}"),
                "manifest",
                Severity::Error,
                true,
            ));
            return PluginAdapterResult {
                package: None,
                diagnostics,
                unsupported_components: Vec::new(),
            };
        }
    };
    let namespace = typescript_identifier(&manifest.name);
    let data_root = data_root_base.join(&namespace);
    let mut skills = adapt_skills(root, &manifest, &mut diagnostics);
    if let Some(instructions) = manifest
        .skill_instructions
        .as_deref()
        .filter(|text| !text.is_empty())
    {
        let instructions = python_strip(instructions);
        for skill in &mut skills {
            skill.prompt = format!("{instructions}\n\n{}", skill.prompt);
        }
    }
    let commands = adapt_commands(root, &manifest, &mut diagnostics);
    let mcp_servers = adapt_mcp(
        root,
        &data_root,
        &resolved_manifest,
        &manifest,
        &mut diagnostics,
    );
    let mut unsupported = Vec::new();
    let hooks = adapt_hooks(
        &resolved_manifest,
        manifest.field("hooks"),
        &mut diagnostics,
        &mut unsupported,
    );
    report_unsupported(
        &resolved_manifest,
        &manifest,
        &mut diagnostics,
        &mut unsupported,
    );
    skills.extend(commands);
    let description = match &manifest.description {
        Some(description) if !description.is_empty() => description.clone(),
        _ => format!("Capabilities provided by {}.", manifest.name),
    };
    PluginAdapterResult {
        package: Some(AdaptedPluginPackage {
            source_format: DetectedPluginFormat::KimiCode,
            manifest_path: resolved_manifest,
            author: author_display_name(manifest.field("author")),
            private_metadata: BTreeMap::from([(
                "kimiManifest".to_owned(),
                Value::Object(manifest.raw.clone()),
            )]),
            name: manifest.name,
            version: manifest.version,
            description,
            namespace,
            data_root,
            scope,
            skill_roots: Vec::new(),
            mcp_servers,
            tool_overrides: BTreeMap::new(),
            adapted_skills: skills,
            adapted_hooks: hooks,
        }),
        diagnostics,
        unsupported_components: unsupported,
    }
}

fn adapt_skills(
    root: &Path,
    manifest: &Manifest,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Vec<AdaptedSkill> {
    let Some(mut paths) = declared_paths(manifest.field("skills")) else {
        diagnostics.push(diagnostic(
            "skills_declaration_invalid",
            &root.join("kimi.plugin.json"),
            "Kimi skills have to be a relative path or a list of relative paths.",
            "skill",
            Severity::Error,
        ));
        return Vec::new();
    };
    if manifest.field("skills").is_none() && root.join("SKILL.md").is_file() {
        paths = vec!["./SKILL.md".to_owned()];
    }
    let mut files = BTreeSet::new();
    for value in paths {
        let Some(relative) = value.strip_prefix("./") else {
            diagnostics.push(diagnostic(
                "skill_path_invalid",
                &root.join(&value),
                "A Kimi skill path has to start with './'.",
                "skill",
                Severity::Error,
            ));
            continue;
        };
        let found = skill_files_for_declared_path(root, &value).and_then(|found| {
            if found.is_empty() {
                Err("the skill path holds no SKILL.md".to_owned())
            } else {
                Ok(found)
            }
        });
        match found {
            Ok(found) => files.extend(found),
            Err(error) => diagnostics.push(diagnostic(
                "skill_path_invalid",
                &root.join(relative),
                format!("Could not import the Kimi skill: {error}"),
                "skill",
                Severity::Warning,
            )),
        }
    }
    files
        .into_iter()
        .filter_map(|path| adapt_skill_file(&path, root, FORMAT, diagnostics, true))
        .collect()
}

fn adapt_commands(
    root: &Path,
    manifest: &Manifest,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Vec<AdaptedSkill> {
    let Some(paths) = declared_paths(manifest.field("commands")) else {
        diagnostics.push(diagnostic(
            "commands_declaration_invalid",
            &root.join("kimi.plugin.json"),
            "Kimi commands have to be a relative path or a list of relative paths.",
            "command",
            Severity::Error,
        ));
        return Vec::new();
    };
    let mut commands = Vec::new();
    for value in paths {
        let Some(relative) = value.strip_prefix("./") else {
            diagnostics.push(diagnostic(
                "command_path_invalid",
                &root.join(&value),
                "A Kimi command path has to start with './'.",
                "command",
                Severity::Warning,
            ));
            continue;
        };
        let found = markdown_files_for_declared_path(root, &value)
            .and_then(|files| Ok((files, resolve_declared_path(root, &value)?)));
        let (files, base) = match found {
            Ok(found) => found,
            Err(error) => {
                diagnostics.push(diagnostic(
                    "command_path_invalid",
                    &root.join(relative),
                    format!("Could not import the Kimi command: {error}"),
                    "command",
                    Severity::Warning,
                ));
                continue;
            }
        };
        let base = if base.is_dir() {
            base
        } else {
            base.parent().map_or_else(PathBuf::new, Path::to_path_buf)
        };
        for path in files {
            let source_name = command_source_name(&path, &base);
            if let Some(command) = adapt_static_command(&path, &source_name, FORMAT, diagnostics) {
                commands.push(command);
            }
        }
    }
    commands
}

fn adapt_mcp(
    root: &Path,
    data_root: &Path,
    manifest_path: &Path,
    manifest: &Manifest,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Vec<AdaptedMcpServer> {
    let declared = manifest.field("mcpServers");
    let Some(mapping) = mcp_mapping(declared) else {
        if declared.is_some() {
            diagnostics.push(diagnostic(
                "mcp_declaration_invalid",
                manifest_path,
                "Kimi mcpServers has to be an object.",
                "mcp",
                Severity::Error,
            ));
        }
        return Vec::new();
    };
    adapt_mcp_servers(
        root,
        data_root,
        &mapping,
        manifest_path,
        FORMAT,
        diagnostics,
        &[("${KIMI_PLUGIN_ROOT}", root.to_string_lossy().into_owned())],
    )
}

fn report_unsupported(
    manifest_path: &Path,
    manifest: &Manifest,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) {
    if manifest.field("sessionStart").is_some() {
        diagnostics.push(diagnostic(
            "session_start_unsupported",
            manifest_path,
            "Kimi sessionStart.skill is kept privately; loading a skill at session start is not supported.",
            "session_start",
            Severity::Info,
        ));
        unsupported.push(unsupported_component(
            "session_start",
            manifest_path,
            "kimi_session_start_unsupported",
        ));
    }
    if manifest.field("interface").is_some() {
        diagnostics.push(diagnostic(
            "install_ui_unsupported",
            manifest_path,
            "Kimi installation and interface metadata is kept privately and has no equivalent in this client.",
            "interface",
            Severity::Info,
        ));
    }
    let extras = manifest.extras();
    if !extras.is_empty() {
        diagnostics.push(diagnostic(
            "fields_unsupported",
            manifest_path,
            format!(
                "These unsupported Kimi manifest fields were kept privately: {}",
                extras.join(", ")
            ),
            "manifest",
            Severity::Info,
        ));
        unsupported.push(unsupported_component(
            "manifest_fields",
            manifest_path,
            "kimi_manifest_fields_unsupported",
        ));
    }
}

fn adapt_hooks(
    manifest_path: &Path,
    raw_hooks: Option<&Value>,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) -> Vec<AdaptedHook> {
    let Some(raw_hooks) = raw_hooks else {
        return Vec::new();
    };
    let Value::Array(rules) = raw_hooks else {
        diagnostics.push(hooks_invalid(
            manifest_path,
            "Kimi hooks have to be a list of declarative hook rules.",
            Severity::Error,
        ));
        return Vec::new();
    };
    let mut adapted = Vec::new();
    for (rule_index, raw_rule) in rules.iter().enumerate() {
        let Value::Object(rule) = raw_rule else {
            diagnostics.push(hooks_invalid(
                manifest_path,
                format!("The Kimi hook rule {rule_index} has to be an object."),
                Severity::Error,
            ));
            continue;
        };
        let mut unknown: Vec<&str> = rule
            .keys()
            .map(String::as_str)
            .filter(|key| !["event", "matcher", "command", "timeout"].contains(key))
            .collect();
        unknown.sort_unstable();
        if !unknown.is_empty() {
            diagnostics.push(hooks_invalid(
                manifest_path,
                format!(
                    "The Kimi hook rule {rule_index} has unsupported fields: {}",
                    unknown.join(", ")
                ),
                Severity::Warning,
            ));
            unsupported.push(unsupported_component(
                "hooks",
                manifest_path,
                "kimi_hook_rule_unsupported",
            ));
            continue;
        }
        let event = rule
            .get("event")
            .filter(|value| truthy(value))
            .or_else(|| rule.get("hookEventName"));
        let Some(hook_type) = event.and_then(Value::as_str).and_then(hook_type) else {
            let shown = event.map_or_else(|| "None".to_owned(), Value::to_string);
            unsupported_hook(
                manifest_path,
                format!("The Kimi hook event {shown} has no matching runtime lifecycle."),
                "kimi_hook_lifecycle_unsupported",
                diagnostics,
                unsupported,
            );
            continue;
        };
        let event = event.and_then(Value::as_str).unwrap_or_default();
        let Some(matcher) = hook_matcher(rule.get("matcher")) else {
            unsupported_hook(
                manifest_path,
                format!(
                    "The Kimi matcher {} cannot be mapped onto runtime tool names.",
                    rule.get("matcher")
                        .map_or_else(|| "None".to_owned(), Value::to_string)
                ),
                "kimi_hook_matcher_unsupported",
                diagnostics,
                unsupported,
            );
            continue;
        };
        if let Some(hook) = adapt_hook_rule(
            manifest_path,
            event,
            hook_type,
            &matcher,
            rule_index,
            rule,
            diagnostics,
        ) {
            adapted.push(hook);
        }
    }
    adapted
}

fn adapt_hook_rule(
    manifest_path: &Path,
    event: &str,
    hook_type: HookType,
    matcher: &str,
    rule_index: usize,
    rule: &Map<String, Value>,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Option<AdaptedHook> {
    let command = match rule.get("command") {
        Some(Value::String(command)) if !python_strip(command).is_empty() => command.clone(),
        _ => {
            diagnostics.push(hooks_invalid(
                manifest_path,
                "A Kimi command hook needs a non-empty command.",
                Severity::Error,
            ));
            return None;
        }
    };
    let timeout = match rule.get("timeout") {
        None | Some(Value::Null) => DEFAULT_HOOK_TIMEOUT,
        Some(Value::Number(number))
            if number
                .as_i64()
                .is_some_and(|value| (1..=MAX_HOOK_TIMEOUT_SECONDS).contains(&value)) =>
        {
            #[expect(clippy::cast_precision_loss, reason = "bounded to 600")]
            let seconds = number.as_i64().unwrap_or_default() as f64;
            seconds
        }
        Some(_) => {
            diagnostics.push(hooks_invalid(
                manifest_path,
                "A Kimi command hook timeout has to be a whole number of seconds from 1 to 600.",
                Severity::Error,
            ));
            return None;
        }
    };
    Some(AdaptedHook {
        config: HookConfig {
            name: format!("kimi-{}-{rule_index}", event.to_lowercase()),
            hook_type,
            command,
            matcher: (hook_type != HookType::PostAgent).then(|| matcher.to_owned()),
            timeout,
            strict: false,
            description: None,
        },
        source_path: manifest_path.to_path_buf(),
        protocol: FORMAT.to_owned(),
    })
}

/// Reference `_kimi_hook_type`.
fn hook_type(event: &str) -> Option<HookType> {
    match event {
        "PreToolUse" => Some(HookType::PreTool),
        "PostToolUse" => Some(HookType::PostTool),
        "Stop" => Some(HookType::PostAgent),
        _ => None,
    }
}

fn hooks_invalid(
    path: &Path,
    message: impl Into<String>,
    severity: Severity,
) -> PluginAdapterDiagnostic {
    diagnostic("hooks_invalid", path, message, "hook", severity)
}

fn unsupported_hook(
    path: &Path,
    message: String,
    reason: &str,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) {
    diagnostics.push(diagnostic(
        "hooks_partially_unsupported",
        path,
        message,
        "hook",
        Severity::Warning,
    ));
    unsupported.push(unsupported_component("hooks", path, reason));
}

#[cfg(test)]
mod kimi_tests;
