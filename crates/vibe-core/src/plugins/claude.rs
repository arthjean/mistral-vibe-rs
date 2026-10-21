//! Reading a Claude Code plugin.
//!
//! Reference `vibe/core/plugins/_claude.py`. The manifest at
//! `.claude-plugin/plugin.json` names skills, static commands, MCP servers and
//! hooks, each of which also has a conventional location. Skills and MCP
//! servers translate directly, commands become synthesized skills, and the
//! command hooks of three lifecycles run under the `claude_code` protocol.
//! Everything else is recorded as unsupported.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::compatibility::{
    AdaptedHook, AdaptedMcpServer, AdaptedPluginPackage, AdaptedSkill, AdaptedUnsupportedComponent,
    DetectedPluginFormat, PluginAdapterDiagnostic, PluginAdapterResult, UnsupportedKind,
    author_display_name, read_text, resolve_declared_path, typescript_identifier,
};
use super::diagnostics::Severity;
use super::foreign::{
    AliasedObject, CommandRules, adapt_declared_commands, adapt_mcp_servers, adapt_skill_file,
    declared_paths, diagnostic, hook_matcher, hook_type, mcp_mapping, optional_aliased_string,
    skill_files_for_declared_path, unsupported_component, unsupported_hook,
};
use super::paths::{is_relative_to, resolve_strict};
use super::strict::{self, ValidationErrors, char_len};
use crate::hooks::{HookConfig, HookProtocol, HookType};
use crate::pycompat;
use crate::skills::SkillScope;

const FORMAT: DetectedPluginFormat = DetectedPluginFormat::ClaudeCode;
const MANIFEST: &str = ".claude-plugin/plugin.json";
/// The default timeout of a Claude command hook, in seconds.
const DEFAULT_HOOK_TIMEOUT: f64 = 600.0;

#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static PLUGIN_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$").expect("plugin name pattern compiles")
});
#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static SEMVER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$",
    )
    .expect("semver pattern compiles")
});

/// The declared fields of `_ClaudeManifest`: the key a document spells, and
/// the field name `populate_by_name` also accepts when it differs.
const FIELDS: [(&str, Option<&str>); 25] = [
    ("name", None),
    ("$schema", Some("schema_id")),
    ("displayName", Some("display_name")),
    ("version", None),
    ("description", None),
    ("author", None),
    ("homepage", None),
    ("repository", None),
    ("license", None),
    ("keywords", None),
    ("skills", None),
    ("commands", None),
    ("hooks", None),
    ("mcpServers", Some("mcp_servers")),
    ("agents", None),
    ("lspServers", Some("lsp_servers")),
    ("outputStyles", Some("output_styles")),
    ("workflows", None),
    ("settings", None),
    ("experimental", None),
    ("interface", None),
    ("defaultEnabled", Some("default_enabled")),
    ("dependencies", None),
    ("userConfig", Some("user_config")),
    ("channels", None),
];

/// The manifest after validation. Reference `_ClaudeManifest`.
struct Manifest {
    object: AliasedObject,
    name: String,
    version: Option<String>,
    description: Option<String>,
}

fn parse_manifest(raw: &Value) -> Result<Manifest, String> {
    let mut errors = ValidationErrors::default();
    let Value::Object(map) = raw else {
        return Err("the manifest has to be an object".to_owned());
    };
    let name = strict::required_string(map, "", "name", &mut errors);
    if let Some(name) = &name
        && !(1..=256).contains(&char_len(name))
    {
        errors.push("name", "must hold 1 to 256 characters");
    }
    optional_aliased_string(map, "$schema", "schema_id", &mut errors);
    optional_aliased_string(map, "displayName", "display_name", &mut errors);
    let version = optional_aliased_string(map, "version", "version", &mut errors);
    let description = optional_aliased_string(map, "description", "description", &mut errors);
    optional_aliased_string(map, "homepage", "homepage", &mut errors);
    optional_aliased_string(map, "license", "license", &mut errors);
    strict::string_list(map, "", "keywords", true, &mut errors);
    if !errors.is_empty() {
        return Err(errors.render("Claude plugin manifest"));
    }
    let (Some(name), Some(version), Some(description)) = (name, version, description) else {
        return Err(errors.render("Claude plugin manifest"));
    };
    if !PLUGIN_NAME.is_match(&name) {
        return Err(
            "name must be lowercase words of letters and digits joined by hyphens".to_owned(),
        );
    }
    if let Some(version) = &version
        && !SEMVER.is_match(version)
    {
        return Err("version must be a semantic version".to_owned());
    }
    Ok(Manifest {
        object: AliasedObject::new(map.clone(), &FIELDS),
        name,
        version,
        description,
    })
}

/// Reads a Claude Code plugin. Reference `ClaudePluginAdapter.adapt`.
#[must_use]
pub fn adapt(root: &Path, data_root_base: &Path, scope: SkillScope) -> PluginAdapterResult {
    let manifest_path = root.join(".claude-plugin").join("plugin.json");
    let loaded = resolve_declared_path(root, MANIFEST).and_then(|resolved| {
        let text = read_text(&resolved).map_err(|error| error.to_string())?;
        let raw: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        Ok((resolved, parse_manifest(&raw)?))
    });
    let (resolved_manifest, manifest) = match loaded {
        Ok(loaded) => loaded,
        Err(error) => {
            return PluginAdapterResult {
                package: None,
                diagnostics: vec![
                    diagnostic(
                        FORMAT,
                        "manifest_invalid",
                        &manifest_path,
                        format!("Could not load the Claude plugin manifest: {error}"),
                        "manifest",
                        Severity::Error,
                    )
                    .fatal(),
                ],
                unsupported_components: Vec::new(),
            };
        }
    };
    let mut diagnostics = Vec::new();
    let mut unsupported = Vec::new();
    let namespace = typescript_identifier(&manifest.name);
    let data_root = data_root_base.join(&namespace);
    let mut skills = adapt_skills(root, &manifest, &mut diagnostics);
    let commands = adapt_declared_commands(
        root,
        manifest.object.field("commands"),
        &CommandRules {
            format: FORMAT,
            manifest: root.join(".claude-plugin").join("plugin.json"),
            conventional_directory: Some("./commands/"),
            vendor: "Claude",
            import_failed: "Could not import the Claude commands",
            severity: Severity::Error,
        },
        &mut diagnostics,
    );
    let mcp_servers = adapt_mcp(
        root,
        &data_root,
        &resolved_manifest,
        &manifest,
        &mut diagnostics,
    );
    let hooks = adapt_hooks(
        root,
        &resolved_manifest,
        &manifest,
        &mut diagnostics,
        &mut unsupported,
    );
    report_unsupported_components(
        root,
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
            source_format: DetectedPluginFormat::ClaudeCode,
            manifest_path: resolved_manifest,
            author: author_display_name(manifest.object.field("author")),
            private_metadata: [(
                "claudeManifest".to_owned(),
                Value::Object(manifest.object.raw.clone()),
            )]
            .into_iter()
            .collect(),
            name: manifest.name,
            version: manifest.version,
            description,
            namespace,
            data_root,
            scope,
            skill_roots: Vec::new(),
            mcp_servers,
            tool_overrides: std::collections::BTreeMap::new(),
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
    let mut paths = declared_paths(manifest.object.field("skills")).unwrap_or_else(|| {
        diagnostics.push(diagnostic(
            FORMAT,
            "skills_declaration_invalid",
            root.join(".claude-plugin").join("plugin.json"),
            "Claude skills have to be a relative path or a list of relative paths.",
            "skill",
            Severity::Error,
        ));
        Vec::new()
    });
    if root.join("skills").is_dir() {
        paths.push("./skills/".to_owned());
    } else if paths.is_empty() && root.join("SKILL.md").is_file() {
        paths.push("./SKILL.md".to_owned());
    }
    let mut files = std::collections::BTreeSet::new();
    for value in paths {
        let Some(relative) = value.strip_prefix("./") else {
            diagnostics.push(diagnostic(
                FORMAT,
                "skill_path_invalid",
                root.join(&value),
                "A Claude skill path has to start with './'.",
                "skill",
                Severity::Error,
            ));
            continue;
        };
        match skill_files_for_declared_path(root, &value) {
            Ok(found) => files.extend(found),
            Err(error) => diagnostics.push(diagnostic(
                FORMAT,
                "skill_path_invalid",
                root.join(relative),
                format!("Could not import the Claude skills: {error}"),
                "skill",
                Severity::Error,
            )),
        }
    }
    files
        .into_iter()
        .filter_map(|path| adapt_skill_file(&path, root, FORMAT, diagnostics, false))
        .collect()
}

fn adapt_mcp(
    root: &Path,
    data_root: &Path,
    manifest_path: &Path,
    manifest: &Manifest,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Vec<AdaptedMcpServer> {
    let mut sources: Vec<(PathBuf, Map<String, Value>)> = Vec::new();
    if root.join(".mcp.json").is_file()
        && let Some(loaded) = load_mcp_file(root, "./.mcp.json", diagnostics)
    {
        sources.push(loaded);
    }
    let declared = manifest.object.field("mcpServers");
    if let Some(Value::Object(_)) = declared {
        if let Some(mapping) = mcp_mapping(declared) {
            sources.push((manifest_path.to_path_buf(), mapping));
        }
    } else {
        match declared_paths(declared) {
            None => diagnostics.push(diagnostic(
                FORMAT,
                "mcp_declaration_invalid",
                manifest_path,
                "Claude mcpServers has to be an object, a path, or a list of paths.",
                "mcp",
                Severity::Error,
            )),
            Some(paths) => {
                for value in paths {
                    if let Some(loaded) = load_mcp_file(root, &value, diagnostics)
                        && !sources.iter().any(|(path, _)| *path == loaded.0)
                    {
                        sources.push(loaded);
                    }
                }
            }
        }
    }
    let placeholders = [
        ("${CLAUDE_PLUGIN_ROOT}", root.to_string_lossy().into_owned()),
        (
            "${CLAUDE_PLUGIN_DATA}",
            data_root.to_string_lossy().into_owned(),
        ),
    ];
    sources
        .iter()
        .flat_map(|(path, mapping)| {
            adapt_mcp_servers(
                root,
                data_root,
                mapping,
                path,
                FORMAT,
                diagnostics,
                &placeholders,
            )
        })
        .collect()
}

fn load_mcp_file(
    root: &Path,
    value: &str,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Option<(PathBuf, Map<String, Value>)> {
    let loaded = (|| {
        if !value.starts_with("./") {
            return Err("a Claude component path has to start with './'".to_owned());
        }
        let path = resolve_declared_path(root, value)?;
        let text = read_text(&path).map_err(|error| error.to_string())?;
        let raw: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        let mapping = mcp_mapping(Some(&raw))
            .ok_or_else(|| "the MCP configuration has to be an object".to_owned())?;
        Ok((path, mapping))
    })();
    match loaded {
        Ok(loaded) => Some(loaded),
        Err(error) => {
            diagnostics.push(diagnostic(
                FORMAT,
                "mcp_config_invalid",
                root.join(value.strip_prefix("./").unwrap_or(value)),
                format!("Could not import the Claude MCP configuration: {error}"),
                "mcp",
                Severity::Error,
            ));
            None
        }
    }
}

fn report_unsupported_components(
    root: &Path,
    manifest_path: &Path,
    manifest: &Manifest,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) {
    for (kind, field, conventional) in [
        (UnsupportedKind::Agents, "agents", root.join("agents")),
        (UnsupportedKind::Lsp, "lspServers", root.join(".lsp.json")),
        (
            UnsupportedKind::OutputStyles,
            "outputStyles",
            root.join("output-styles"),
        ),
        (
            UnsupportedKind::Workflows,
            "workflows",
            root.join("workflows"),
        ),
        (
            UnsupportedKind::Settings,
            "settings",
            root.join("settings.json"),
        ),
    ] {
        let declared = manifest.object.field(field).is_some();
        if !declared && !conventional.exists() {
            continue;
        }
        let path = if declared {
            manifest_path
        } else {
            conventional.as_path()
        };
        diagnostics.push(diagnostic(
            FORMAT,
            &format!("{kind}_unsupported"),
            path,
            format!("Claude {field} are kept as package data and not converted into a capability."),
            kind.as_str(),
            Severity::Info,
        ));
        unsupported.push(unsupported_component(
            kind,
            path,
            &format!("claude_{kind}_unsupported"),
        ));
    }
    if let Some(Value::Object(experimental)) = manifest.object.field("experimental") {
        for kind in [UnsupportedKind::Themes, UnsupportedKind::Monitors] {
            if !experimental.contains_key(kind.as_str()) {
                continue;
            }
            diagnostics.push(diagnostic(
                FORMAT,
                &format!("{kind}_unsupported"),
                manifest_path,
                format!("Claude {kind} have no portable runtime equivalent."),
                kind.as_str(),
                Severity::Info,
            ));
            unsupported.push(unsupported_component(
                kind,
                manifest_path,
                &format!("claude_{kind}_unsupported"),
            ));
        }
    }
    let mut present_ui: Vec<&str> = [
        "interface",
        "defaultEnabled",
        "dependencies",
        "userConfig",
        "channels",
    ]
    .into_iter()
    .filter(|field| manifest.object.field(field).is_some())
    .collect();
    present_ui.sort_unstable();
    if !present_ui.is_empty() {
        diagnostics.push(diagnostic(FORMAT, "install_ui_unsupported", manifest_path, format!(
                "These Claude installation, configuration and interface fields are not supported: {}",
                present_ui.join(", ")
            ), "interface", Severity::Info));
        unsupported.push(unsupported_component(
            UnsupportedKind::InstallUi,
            manifest_path,
            "claude_install_ui_unsupported",
        ));
    }
    let extras = manifest.object.extras();
    if !extras.is_empty() {
        diagnostics.push(diagnostic(
            FORMAT,
            "fields_unsupported",
            manifest_path,
            format!(
                "These unrecognized Claude manifest fields were kept privately: {}",
                extras.join(", ")
            ),
            "manifest",
            Severity::Info,
        ));
    }
}

/// One hook configuration: where it was read, and its object.
struct HookSource {
    path: PathBuf,
    object: Map<String, Value>,
}

fn adapt_hooks(
    root: &Path,
    manifest_path: &Path,
    manifest: &Manifest,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) -> Vec<AdaptedHook> {
    let sources = hook_sources(root, manifest_path, manifest, diagnostics);
    let mut hooks = Vec::new();
    for source in sources {
        let groups = source
            .object
            .get("hooks")
            .cloned()
            .unwrap_or_else(|| Value::Object(source.object.clone()));
        let Value::Object(groups) = groups else {
            diagnostics.push(hooks_invalid(
                &source.path,
                "A Claude hook configuration has to hold a hooks object.",
            ));
            continue;
        };
        // Events run in the order the document lists them, which the
        // order-preserving `serde_json::Map` keeps.
        for (event_name, raw_rules) in &groups {
            adapt_hook_group(
                &source.path,
                event_name,
                raw_rules,
                &mut hooks,
                diagnostics,
                unsupported,
            );
        }
    }
    hooks
}

fn hook_sources(
    root: &Path,
    manifest_path: &Path,
    manifest: &Manifest,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Vec<HookSource> {
    let declared = manifest.object.field("hooks");
    let mut sources = Vec::new();
    let conventional = root.join("hooks").join("hooks.json");
    if conventional.is_file()
        && let Some(loaded) = load_hooks_file(root, &conventional, diagnostics)
    {
        sources.push(loaded);
    }
    let mut paths: Vec<String> = Vec::new();
    match declared {
        None => {}
        Some(Value::String(path)) => paths.push(path.clone()),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
            paths.extend(
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned),
            );
        }
        Some(Value::Object(inline)) => sources.push(HookSource {
            path: manifest_path.to_path_buf(),
            object: inline.clone(),
        }),
        Some(_) => diagnostics.push(hooks_invalid(
            manifest_path,
            "Claude hooks have to be an inline object, a relative JSON path, or a list of them.",
        )),
    }
    for declared_path in paths {
        let resolved = (|| {
            if !declared_path.starts_with("./") {
                return Err("a Claude hook path has to start with './'".to_owned());
            }
            let resolved = resolve_declared_path(root, &declared_path)?;
            if !resolved.is_file() {
                return Err("a Claude hook path has to resolve to a JSON file".to_owned());
            }
            Ok(resolved)
        })();
        match resolved {
            Ok(resolved) => {
                if let Some(loaded) = load_hooks_file(root, &resolved, diagnostics)
                    && !sources.iter().any(|source| source.path == loaded.path)
                {
                    sources.push(loaded);
                }
            }
            Err(error) => diagnostics.push(hooks_invalid(
                manifest_path,
                format!("Could not import the Claude hooks: {error}"),
            )),
        }
    }
    sources
}

fn load_hooks_file(
    root: &Path,
    path: &Path,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
) -> Option<HookSource> {
    let loaded = resolve_strict(path)
        .map_err(|error| error.to_string())
        .and_then(|resolved| {
            if !is_relative_to(&resolved, root) || !resolved.is_file() {
                return Err("a Claude hook file has to stay inside the plugin".to_owned());
            }
            let text = read_text(&resolved).map_err(|error| error.to_string())?;
            match serde_json::from_str::<Value>(&text).map_err(|error| error.to_string())? {
                Value::Object(object) => Ok(HookSource {
                    path: resolved,
                    object,
                }),
                _ => Err("a Claude hook configuration has to be an object".to_owned()),
            }
        });
    match loaded {
        Ok(loaded) => Some(loaded),
        Err(error) => {
            diagnostics.push(hooks_invalid(
                path,
                format!("Could not import the Claude hooks: {error}"),
            ));
            None
        }
    }
}

fn adapt_hook_group(
    path: &Path,
    event_name: &str,
    raw_rules: &Value,
    hooks: &mut Vec<AdaptedHook>,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) {
    let Some(hook_type) = hook_type(event_name) else {
        unsupported_hook(
            FORMAT,
            path,
            format!(
                "The Claude hook event {} has no matching runtime lifecycle.",
                pycompat::repr_str(event_name)
            ),
            "claude_hook_lifecycle_unsupported",
            diagnostics,
            unsupported,
        );
        return;
    };
    let Value::Array(rules) = raw_rules else {
        diagnostics.push(hooks_invalid(
            path,
            format!(
                "The Claude hook event {} has to hold a list.",
                pycompat::repr_str(event_name)
            ),
        ));
        return;
    };
    for (rule_index, raw_rule) in rules.iter().enumerate() {
        let Value::Object(rule) = raw_rule else {
            diagnostics.push(hooks_invalid(
                path,
                format!("The Claude hook rule {event_name}[{rule_index}] has to be an object."),
            ));
            continue;
        };
        let Some(matcher) = hook_matcher(rule.get("matcher")) else {
            unsupported_hook(
                FORMAT,
                path,
                format!(
                    "The Claude matcher {} cannot be mapped onto runtime tool names.",
                    rule.get("matcher")
                        .map_or_else(|| "None".to_owned(), Value::to_string)
                ),
                "claude_hook_matcher_unsupported",
                diagnostics,
                unsupported,
            );
            continue;
        };
        let Some(Value::Array(actions)) = rule.get("hooks") else {
            diagnostics.push(hooks_invalid(
                path,
                format!(
                    "The Claude hook rule {event_name}[{rule_index}] has to hold a hooks list."
                ),
            ));
            continue;
        };
        for (action_index, action) in actions.iter().enumerate() {
            let context = HookContext {
                path,
                event_name,
                hook_type,
                matcher: &matcher,
                rule_index,
                action_index,
            };
            if let Some(hook) = adapt_hook_action(&context, action, diagnostics, unsupported) {
                hooks.push(hook);
            }
        }
    }
}

struct HookContext<'a> {
    path: &'a Path,
    event_name: &'a str,
    hook_type: HookType,
    matcher: &'a str,
    rule_index: usize,
    action_index: usize,
}

fn adapt_hook_action(
    context: &HookContext<'_>,
    action: &Value,
    diagnostics: &mut Vec<PluginAdapterDiagnostic>,
    unsupported: &mut Vec<AdaptedUnsupportedComponent>,
) -> Option<AdaptedHook> {
    let path = context.path;
    let Value::Object(action) = action else {
        diagnostics.push(hooks_invalid(
            path,
            format!(
                "The Claude hook action {}[{}].hooks[{}] has to be an object.",
                context.event_name, context.rule_index, context.action_index
            ),
        ));
        return None;
    };
    let has_unsupported_field = ["if", "args", "shell", "asyncRewake"]
        .iter()
        .any(|field| action.contains_key(*field));
    if action.get("type").and_then(Value::as_str) != Some("command")
        || action.get("async") == Some(&Value::Bool(true))
        || has_unsupported_field
    {
        unsupported_hook(
            FORMAT,
            path,
            "Only synchronous shell command hooks without an `if` filter map onto runtime hooks.",
            "claude_hook_action_unsupported",
            diagnostics,
            unsupported,
        );
        return None;
    }
    let command = match action.get("command") {
        Some(Value::String(command)) if !pycompat::strip(command).is_empty() => command.clone(),
        _ => {
            diagnostics.push(hooks_invalid(
                path,
                "A Claude command hook needs a non-empty command.",
            ));
            return None;
        }
    };
    let timeout = match action.get("timeout") {
        None | Some(Value::Null) => DEFAULT_HOOK_TIMEOUT,
        Some(Value::Number(number)) if number.as_f64().is_some_and(|value| value > 0.0) => {
            number.as_f64().unwrap_or(DEFAULT_HOOK_TIMEOUT)
        }
        Some(_) => {
            diagnostics.push(hooks_invalid(
                path,
                "A Claude command hook timeout has to be a positive number.",
            ));
            return None;
        }
    };
    Some(AdaptedHook {
        config: HookConfig {
            name: format!(
                "claude-{}-{}-{}",
                context.event_name.to_lowercase(),
                context.rule_index,
                context.action_index
            ),
            hook_type: context.hook_type,
            command,
            matcher: (context.hook_type != HookType::PostAgent).then(|| context.matcher.to_owned()),
            timeout,
            strict: false,
            description: None,
        },
        source_path: path.to_path_buf(),
        protocol: HookProtocol::ClaudeCode,
    })
}

fn hooks_invalid(path: &Path, message: impl Into<String>) -> PluginAdapterDiagnostic {
    diagnostic(
        FORMAT,
        "hooks_invalid",
        path,
        message,
        "hook",
        Severity::Error,
    )
}

#[cfg(test)]
mod claude_tests;
