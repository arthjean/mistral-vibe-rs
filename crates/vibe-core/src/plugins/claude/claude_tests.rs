//! The Claude Code adapter against the vector the reference app server
//! published for the `claudey` probe plugin.

use std::fs;

use serde_json::json;

use super::hook_matcher;
use crate::plugins::compatibility::DetectedPluginFormat;
use crate::plugins::native::PluginResolver;
use crate::skills::SkillScope;

const CLAUDEY_MANIFEST: &str =
    "{\"name\":\"claudey\",\"version\":\"1.0.0\",\"description\":\"Claude-format probe\"}\n";

#[test]
fn the_claudey_probe_resolves_with_the_reference_digests() {
    let home = tempfile::tempdir().unwrap();
    let manifest = home
        .path()
        .join("plugins")
        .join("claudey")
        .join(".claude-plugin");
    fs::create_dir_all(&manifest).unwrap();
    fs::write(manifest.join("plugin.json"), CLAUDEY_MANIFEST).unwrap();

    let resolved = PluginResolver {
        user_roots: vec![home.path().join("plugins")],
        data_root_base: Some(home.path().join("plugin-data")),
        ..PluginResolver::default()
    }
    .resolve();

    assert!(resolved.issues.is_empty(), "{:?}", resolved.issues);
    let [claudey] = resolved.plugins.as_slice() else {
        panic!("one plugin expected: {:?}", resolved.plugins);
    };
    assert_eq!(claudey.source_format, DetectedPluginFormat::ClaudeCode);
    assert_eq!(claudey.scope, SkillScope::Global);
    assert_eq!(claudey.description, "Claude-format probe");
    assert_eq!(
        claudey.manifest_digest,
        "b2139fea2788ef72a33b041b61656ec88d95c892a9466a8d980f9e7f5cd28af2"
    );
    assert_eq!(
        claudey.content_digest,
        "bcfb7e5c028c2ecaed9f8d8755613dce32899deb37a8bb92acddc125a1a96bb3"
    );
}

#[test]
fn a_matcher_is_kept_when_python_would_compile_it() {
    assert_eq!(hook_matcher(None).as_deref(), Some("*"));
    assert_eq!(hook_matcher(Some(&json!(""))).as_deref(), Some("*"));
    assert_eq!(
        hook_matcher(Some(&json!("Edit|Write"))).as_deref(),
        Some("Edit|Write")
    );
    assert_eq!(
        hook_matcher(Some(&json!("(?<=x)y"))).as_deref(),
        Some("(?<=x)y")
    );
    assert_eq!(
        hook_matcher(Some(&json!(r"(a)\1"))).as_deref(),
        Some(r"(a)\1")
    );
    assert_eq!(hook_matcher(Some(&json!("Bash("))), None);
    assert_eq!(hook_matcher(Some(&json!(7))), None);
}
