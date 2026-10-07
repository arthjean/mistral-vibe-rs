//! The compatibility adapters that read foreign plugin formats.
//!
//! Reference `_compatibility_adapter` in `vibe/core/plugins/_native.py`.

use std::path::Path;

use super::compatibility::{DetectedPluginFormat, PluginAdapterResult};
use crate::skills::SkillScope;

/// Adapts the plugin at `root`, which `source_format` names; a format with no
/// adapter yields an empty result.
#[must_use]
pub fn adapt(
    source_format: DetectedPluginFormat,
    root: &Path,
    data_root_base: &Path,
    scope: SkillScope,
) -> PluginAdapterResult {
    match source_format {
        DetectedPluginFormat::Codex => super::codex::adapt(root, data_root_base, scope),
        DetectedPluginFormat::ClaudeCode => super::claude::adapt(root, data_root_base, scope),
        DetectedPluginFormat::KimiCode => super::kimi::adapt(root, data_root_base, scope),
        DetectedPluginFormat::OpenCode => super::foreign::adapt(root, data_root_base, scope),
        _ => PluginAdapterResult::default(),
    }
}
