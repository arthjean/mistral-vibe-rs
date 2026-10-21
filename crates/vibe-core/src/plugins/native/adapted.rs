//! A foreign plugin, once its compatibility adapter has read it, as the
//! resolver carries it: the same descriptor and components a native package
//! produces. Reference `PluginResolver._candidate_from_adapted`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::Value;

use super::super::canonical::canonical_json_digest;
use super::super::compatibility::{
    AdaptedHook, AdaptedMcpServer, AdaptedPluginPackage, AdaptedSkill, AdaptedUnsupportedComponent,
    DetectedPluginFormat, PluginAdapterDiagnostic, plugin_runtime_state_names, read_text,
};
use super::super::content::digest_plugin_tree;
use super::super::diagnostics::{self as codes, PluginConfigIssue};
use super::super::paths::path_text;
use super::mcp::plugin_mcp_server;
use super::{
    Candidate, PluginDescriptor, PluginHookSource, PluginMcpServerDefinition,
    PluginUnsupportedComponent, RESERVED_NAMESPACES, RUNTIME_SKILL_PATH_METADATA, Resolve,
    RuntimeHookDefinition, SKILL_TRANSLATION_METADATA,
};
use crate::extensions::{SkillDefinition, skill_file_path};
use crate::pycompat::{self, Dumps};
use crate::skills::SkillSource;
use crate::text::sha256_hex;

impl Resolve<'_> {
    pub(super) fn candidate_from_adapted(
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
                        pycompat::repr_str(&package.namespace)
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
        self.record_unsupported(&package.name, package.source_format, unsupported);
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

    pub(super) fn record_unsupported(
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
                        pycompat::repr_str(&alias)
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
        let source = PluginHookSource::of(plugin.scope);
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
                    source,
                    order,
                    cwd: cwd.clone(),
                    environment: environment.clone(),
                    protocol: definition.protocol,
                    plugin_name: plugin.name.clone(),
                    declared_name: definition.config.name.clone(),
                    config_file: definition.source_path.clone(),
                }
            })
            .collect()
    }

    pub(super) fn record_adapter_result(
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
}

fn adapt_mcp_servers(
    plugin: &PluginDescriptor,
    servers: &[AdaptedMcpServer],
) -> Vec<PluginMcpServerDefinition> {
    servers
        .iter()
        .map(|adapted| {
            plugin_mcp_server(
                plugin,
                adapted.source_id.clone(),
                &adapted.server,
                adapted.config_file.clone(),
            )
        })
        .collect()
}

/// The frontmatter a synthesized skill's `SKILL.md` carries. Reference
/// `_render_generated_skill`.
fn render_generated_skill(definition: &AdaptedSkill) -> String {
    let mut lines = vec![
        "---".to_owned(),
        format!(
            "name: {}",
            pycompat::dumps(
                &Value::String(definition.source_name.clone()),
                Dumps::DEFAULT
            )
        ),
        format!(
            "description: {}",
            pycompat::dumps(
                &Value::String(definition.description.clone()),
                Dumps::DEFAULT
            )
        ),
    ];
    if !definition.allowed_tools.is_empty() {
        lines.push(format!(
            "allowed-tools: {}",
            pycompat::dumps(
                &Value::Array(
                    definition
                        .allowed_tools
                        .iter()
                        .map(|tool| Value::String(tool.clone()))
                        .collect()
                ),
                Dumps::DEFAULT
            )
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
