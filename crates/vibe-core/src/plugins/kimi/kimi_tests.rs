//! The Kimi Code adapter's manifest selection and validation.

use std::fs;

use super::adapt;
use crate::skills::SkillScope;

#[test]
fn the_root_manifest_wins_and_the_nested_one_is_reported_shadowed() {
    let root = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(root.path()).unwrap();
    fs::create_dir_all(root.join(".kimi-plugin")).unwrap();
    fs::write(root.join("kimi.plugin.json"), r#"{"name": "front"}"#).unwrap();
    fs::write(
        root.join(".kimi-plugin").join("plugin.json"),
        r#"{"name": "back"}"#,
    )
    .unwrap();

    let result = adapt(&root, &root.join("data"), SkillScope::Project);

    let package = result.package.unwrap();
    assert_eq!(package.name, "front");
    assert_eq!(package.description, "Capabilities provided by front.");
    let codes: Vec<&str> = result
        .diagnostics
        .iter()
        .map(|item| item.code.as_str())
        .collect();
    assert_eq!(codes, ["plugin.compatibility.kimi_code.manifest_shadowed"]);
}

#[test]
fn a_name_outside_the_kimi_pattern_drops_the_plugin_fatally() {
    let root = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(root.path()).unwrap();
    fs::write(root.join("kimi.plugin.json"), r#"{"name": "Upper"}"#).unwrap();

    let result = adapt(&root, &root.join("data"), SkillScope::Project);

    assert!(result.package.is_none());
    let [diagnostic] = result.diagnostics.as_slice() else {
        panic!("one diagnostic expected: {:?}", result.diagnostics);
    };
    assert_eq!(
        diagnostic.code,
        "plugin.compatibility.kimi_code.manifest_invalid"
    );
    assert!(diagnostic.fatal);
}
