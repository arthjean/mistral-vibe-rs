//! The shipped plugin against what the reference resolves for its own copy.

use super::*;
use crate::plugins::native::PluginResolver;
use crate::skills::SkillScope;

#[test]
fn the_vibe_plugin_resolves_with_the_reference_manifest_and_skill_set() {
    let home = tempfile::tempdir().unwrap();
    let root = materialize_builtin_plugins(home.path()).unwrap();
    let resolved = PluginResolver {
        builtin_roots: vec![root],
        data_root_base: Some(home.path().join("plugin-data")),
        ..PluginResolver::default()
    }
    .resolve();
    assert!(resolved.issues.is_empty(), "{:?}", resolved.issues);
    let [vibe] = resolved.plugins.as_slice() else {
        panic!("one plugin expected: {:?}", resolved.plugins);
    };
    assert_eq!(
        vibe.manifest_digest,
        "70952cc17e9314a23a4e135dd53a167bdb0bde2f5662e3853cb028ab6d88240a"
    );
    assert_eq!(vibe.scope, SkillScope::Builtin);
    assert_eq!(vibe.namespace, "vibe");
    assert_eq!(vibe.author.as_deref(), Some("Mistral AI"));
    assert_eq!(
        vibe.description,
        "Skills and components shipped with the Vibe CLI."
    );
    let skills: Vec<(&str, bool, bool)> = resolved
        .skills
        .iter()
        .map(|(alias, skill)| (alias.as_str(), skill.user_invocable, skill.model_invocable))
        .collect();
    assert_eq!(
        skills,
        [
            ("vibe:create-plugin", true, true),
            ("vibe:skill-creator", true, true),
            ("vibe:vibe", false, true),
            ("vibe:worktree", true, true),
        ]
    );
}

/// The prose is this repository's own, so the tree digest must differ from
/// the reference's.
#[test]
fn the_vibe_plugin_content_is_not_the_reference_text() {
    let home = tempfile::tempdir().unwrap();
    let root = materialize_builtin_plugins(home.path()).unwrap();
    let digest = crate::plugins::content::digest_plugin_tree(
        &root.join("vibe"),
        &std::collections::BTreeSet::from([".git".to_owned()]),
    )
    .unwrap();
    assert_ne!(
        digest,
        "c32af761ef9b6f9a7b3234400f35c316c404c0cfba0465208984536f15386b92"
    );
}
