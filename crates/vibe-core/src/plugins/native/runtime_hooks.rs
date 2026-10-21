//! The hooks a native plugin declares in `ai.mistral.vibe/hooks.toml`.
//! Reference `PluginResolver._load_runtime_hooks`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use super::super::diagnostics::{self as codes, PluginConfigIssue};
use super::super::paths::{exists_or_link, path_text};
use super::contained::contained_component_file;
use super::manifest::Extension;
use super::{
    PluginDescriptor, PluginHookSource, Resolve, RuntimeHookDefinition, VIBE_EXTENSION_DIRECTORY,
};
use crate::hooks::HookProtocol;
use crate::pycompat;

const MAX_PLUGIN_HOOKS_BYTES: u64 = 64 * 1024;
const MAX_PLUGIN_HOOKS: usize = 128;

impl Resolve<'_> {
    pub(super) fn load_runtime_hooks(
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
        let source = PluginHookSource::of(plugin.scope);
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
                        pycompat::repr_str(&hook.name)
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
                source,
                order,
                cwd: Some(plugin.root.clone()),
                environment: environment.clone(),
                protocol: HookProtocol::Vibe,
                plugin_name: plugin.name.clone(),
                declared_name: hook.name.clone(),
                config_file: resolved.clone(),
            });
        }
        definitions
    }
}
