//! A session's plugins from installed roots to the published catalogs.

use std::fs;

use serde_json::json;

use super::*;

const DEMO_MANIFEST: &str = "{\"$schema\": \"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\", \"name\": \"demo\", \"version\": \"0.1.0\", \"description\": \"Probe plugin\"}\n";
const DEMO_SKILL: &str =
    "---\nname: hello\ndescription: Says hello for the probe\n---\nSay hello.\n";

/// A temporary vibe home whose hardened package checkouts are made writable
/// again before it is removed.
pub(crate) struct HardenedHome(tempfile::TempDir);

impl HardenedHome {
    pub(crate) fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    pub(crate) fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for HardenedHome {
    fn drop(&mut self) {
        relax(self.0.path());
    }
}

#[cfg(unix)]
fn relax(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(root, fs::Permissions::from_mode(0o700));
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => relax(&entry.path()),
            Ok(kind) if kind.is_file() => {
                let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o600));
            }
            _ => {}
        }
    }
}

#[cfg(not(unix))]
fn relax(_root: &Path) {}

fn sources(home: &Path) -> PluginSources {
    let plugin = home.join("plugins").join("demo");
    fs::create_dir_all(plugin.join("skills").join("hello")).unwrap();
    fs::write(plugin.join("plugin.json"), DEMO_MANIFEST).unwrap();
    fs::write(
        plugin.join("skills").join("hello").join("SKILL.md"),
        DEMO_SKILL,
    )
    .unwrap();
    PluginSources {
        project_roots: Vec::new(),
        user_roots: vec![home.join("plugins")],
        vibe_home: home.to_path_buf(),
        storage_root: home.join("logs").join("session"),
        workdir: home.join("workspace"),
        configured_mcp_names: BTreeSet::new(),
    }
}

#[tokio::test]
async fn a_session_pins_its_plugins_and_publishes_the_reference_catalog() {
    let home = HardenedHome::new();
    let home = home.path().canonicalize().unwrap();
    let sources = sources(&home);
    let plugins = start(&sources, "session-1", PluginPorts::default()).await;
    let catalog = plugin_catalog(&plugins);
    let demo = &catalog["plugins"][0];
    let digest = "a82a1ea9ab287e5fea354995c060f748719e98a6a6a16e6787c67dedf419bc1f";
    assert_eq!(demo["name"], json!("demo"));
    assert_eq!(demo["sourceFormat"], json!("agent_plugins_1_0"));
    assert_eq!(demo["scope"], json!("global"));
    assert_eq!(demo["contentSha256"], json!(digest));
    assert_eq!(demo["author"], json!(null));
    assert_eq!(
        demo["pinnedRoot"],
        json!(
            home.join("logs/session/plugins/packages/a8")
                .join(digest)
                .to_string_lossy()
        )
    );
    assert_eq!(
        demo["installedRoot"],
        json!(home.join("plugins/demo").to_string_lossy())
    );
    assert_eq!(
        demo["components"],
        json!([{"kind": "skill", "name": "demo:hello", "status": null}])
    );
    assert_eq!(demo["drifted"], json!(0));
    assert_eq!(catalog["plugins"][1]["name"], json!("vibe"));
    assert_eq!(catalog["plugins"][1]["scope"], json!("builtin"));
    assert_eq!(catalog["dropped"], json!([]));

    let info = plugin_info(&plugins);
    assert_eq!(
        info["workdir"],
        json!(home.join("workspace").to_string_lossy())
    );
    assert_eq!(
        info["components"][0]["sourcePath"],
        json!(
            home.join("logs/session/plugins/packages/a8")
                .join(digest)
                .join("skills/hello/SKILL.md")
                .to_string_lossy()
        )
    );
    assert_eq!(
        info["raw"]["plugins"]["demo"]["contentSha256"],
        json!(digest)
    );
    assert_eq!(info["raw"]["routes"], json!({}));
    assert!(
        home.join("logs/session/plugins/data/session-1/demo")
            .is_dir()
    );
}

#[tokio::test]
async fn a_reload_picks_up_a_new_plugin() {
    let home = HardenedHome::new();
    let sources = sources(home.path());
    let first = start(&sources, "s", PluginPorts::default()).await;
    let second = home.path().join("plugins").join("other");
    fs::create_dir_all(&second).unwrap();
    fs::write(
        second.join("plugin.json"),
        DEMO_MANIFEST.replace("\"demo\"", "\"other\""),
    )
    .unwrap();
    let reloaded = reload(&sources, "s", &first, PluginPorts::default()).await;
    let names: Vec<&str> = reloaded
        .snapshot
        .plugins
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["demo", "other", "vibe"]);
    assert!(plugin_reload_notices(&reloaded).is_empty());
}
