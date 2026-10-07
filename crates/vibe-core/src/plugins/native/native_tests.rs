//! The resolver against digests the reference produced for the same trees.

use std::fs;

use super::*;

const DEMO_MANIFEST: &str = r#"{"$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json", "name": "demo", "version": "0.1.0", "description": "Probe plugin"}
"#;
const DEMO_SKILL: &str =
    "---\nname: hello\ndescription: Says hello for the probe\n---\nSay hello.\n";

#[test]
fn a_native_plugin_resolves_with_the_reference_digests() {
    let home = tempfile::tempdir().unwrap();
    let plugin = home.path().join("plugins").join("demo");
    fs::create_dir_all(plugin.join("skills").join("hello")).unwrap();
    fs::write(plugin.join("plugin.json"), DEMO_MANIFEST).unwrap();
    fs::write(
        plugin.join("skills").join("hello").join("SKILL.md"),
        DEMO_SKILL,
    )
    .unwrap();

    let resolved = PluginResolver {
        user_roots: vec![home.path().join("plugins")],
        data_root_base: Some(home.path().join("plugin-data")),
        ..PluginResolver::default()
    }
    .resolve();

    assert!(resolved.issues.is_empty(), "{:?}", resolved.issues);
    let [demo] = resolved.plugins.as_slice() else {
        panic!("one plugin expected: {:?}", resolved.plugins);
    };
    assert_eq!(
        demo.manifest_digest,
        "b09cd24ef0ae8fc4fda13f852e2eaa488f7268660c5b92de3a0e25d68f1e692d"
    );
    assert_eq!(
        demo.content_digest,
        "a82a1ea9ab287e5fea354995c060f748719e98a6a6a16e6787c67dedf419bc1f"
    );
    assert_eq!(demo.scope, SkillScope::Global);
    assert_eq!(demo.namespace, "demo");
    let aliases: Vec<&str> = resolved
        .skills
        .iter()
        .map(|(alias, _)| alias.as_str())
        .collect();
    assert_eq!(aliases, ["demo:hello"]);
}

#[test]
fn the_private_server_alias_matches_the_reference_shape() {
    let alias = private_server_alias("demo", "my-server");
    assert!(alias.starts_with("plugin_"));
    assert!(alias.ends_with("_my_server"));
    assert_eq!(alias.len(), "plugin_".len() + 16 + "_my_server".len());
}

#[test]
fn python_title_cases_words_like_str_title() {
    assert_eq!(python_title("code reviewer"), "Code Reviewer");
    assert_eq!(python_title("a1b"), "A1B");
}
