//! The skills a native plugin ships under `skills/`. Reference
//! `PluginResolver._load_skills_from_roots`.

use std::path::{Path, PathBuf};

use super::super::compatibility::{openai_policy, read_text};
use super::super::diagnostics::{self as codes, PluginConfigIssue};
use super::super::paths::{exists_or_link, file_name, is_relative_to, resolve_strict};
use super::contained::sorted_children;
use super::{PluginDescriptor, Resolve};
use crate::extensions::SkillDefinition;
use crate::pycompat;
use crate::skills::SkillSource;
use crate::skills::parser::parse_skill_markdown;
use crate::skills::schema::SkillMetadata;

impl Resolve<'_> {
    pub(super) fn load_skills_from_roots(
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
                                pycompat::repr_str(&skill.name),
                                pycompat::repr_str(&expected_name)
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
                            pycompat::repr_str(&alias)
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
        Ok(SkillDefinition::from_metadata(
            metadata,
            &body,
            model_invocable,
            SkillSource::Local,
            plugin.scope,
            path,
        ))
    }
}
