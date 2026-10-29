use super::{MODEL_ALIAS, PROVIDER_KEY_VARIABLE, Setup, World};

#[test]
fn a_world_holds_its_directories_and_a_committed_repository_under_the_worlds_directory() {
    let world = World::new(Setup::default()).unwrap();
    let root = world.root();
    let temp = super::dunce_canonical(&super::worlds_parent()).unwrap();
    assert!(root.starts_with(&temp), "{}", root.display());
    if let Some(home) = std::env::var_os("HOME") {
        assert!(!root.starts_with(home));
    }
    assert!(
        root.ancestors()
            .skip(1)
            .all(|dir| !dir.join(".git").exists())
    );
    for directory in ["home", "vibe-home", "tmp", "work"] {
        assert!(root.join(directory).is_dir(), "{directory}");
    }
    let log = world
        .program("git")
        .args(["log", "--format=%an <%ae> %s"])
        .run()
        .unwrap();
    assert!(log.success(), "{}", log.stderr);
    assert_eq!(
        log.stdout.trim(),
        "Vibe E2E <e2e@example.invalid> e2e world"
    );
}

#[test]
fn the_config_points_the_provider_and_every_host_at_loopback() {
    let world = World::new(Setup::default()).unwrap();
    let config = std::fs::read_to_string(world.vibe_home().join("config.toml")).unwrap();
    let parsed: toml_like::Table = toml_like::parse(&config);
    let base = world.platform().base();
    assert!(base.starts_with("http://127.0.0.1:"));
    assert_eq!(parsed.get("active_model"), Some(MODEL_ALIAS));
    assert_eq!(parsed.get("enable_update_checks"), Some("false"));
    for key in [
        "console_base_url",
        "vibe_base_url",
        "vibe_code_sessions_base_url",
    ] {
        assert_eq!(parsed.get(key), Some(base), "{key}");
    }
    assert!(
        config.contains(&format!("[experiments]\napi_host = \"{base}\"")),
        "{config}"
    );
    assert!(
        config.contains(&format!("api_base = \"{base}/v1\"")),
        "{config}"
    );
    assert!(config.contains(&format!("api_key_env_var = \"{PROVIDER_KEY_VARIABLE}\"")));
    let mistral =
        World::new(Setup::mistral().config("[tools.bash]\npermission = \"always\"")).unwrap();
    let config = std::fs::read_to_string(mistral.vibe_home().join("config.toml")).unwrap();
    assert!(config.contains("backend = \"mistral\""), "{config}");
    assert!(
        config.contains("[tools.bash]\npermission = \"always\"\n"),
        "{config}"
    );
}

/// A minimal reader for the top-level `key = value` lines the test checks.
mod toml_like {
    pub struct Table(Vec<(String, String)>);

    impl Table {
        pub fn get(&self, key: &str) -> Option<&str> {
            self.0
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        }
    }

    pub fn parse(text: &str) -> Table {
        Table(
            text.lines()
                .take_while(|line| !line.starts_with('['))
                .filter_map(|line| line.split_once(" = "))
                .map(|(k, v)| (k.to_owned(), v.trim_matches('"').to_owned()))
                .collect(),
        )
    }
}

#[cfg(unix)]
#[test]
fn a_child_environment_is_built_from_empty() {
    use std::collections::BTreeSet;

    use super::WORLD_VARIABLE;

    let world = World::new(Setup::default().env("SCENARIO_VAR", "1")).unwrap();
    let output = world.program("env").env("COMMAND_VAR", "2").run().unwrap();
    let names: BTreeSet<&str> = output
        .stdout
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name))
        .collect();
    let expected: BTreeSet<&str> = [
        "PATH",
        "HOME",
        "VIBE_HOME",
        "TMPDIR",
        "LANG",
        "TERM",
        "DBUS_SESSION_BUS_ADDRESS",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
        WORLD_VARIABLE,
        PROVIDER_KEY_VARIABLE,
        "SCENARIO_VAR",
        "COMMAND_VAR",
    ]
    .into();
    assert_eq!(names, expected);
    assert!(
        output
            .stdout
            .contains(&format!("TMPDIR={}", world.tmp().display()))
    );
    assert!(
        output
            .stdout
            .contains("DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent")
    );
    assert!(
        output
            .stdout
            .contains(&format!("{WORLD_VARIABLE}={}", world.name()))
    );
}

#[test]
fn a_seed_path_outside_the_root_is_refused() {
    let world = World::new(Setup::default()).unwrap();
    world.write("work/AGENTS.md", "inside").unwrap();
    assert_eq!(
        std::fs::read_to_string(world.work().join("AGENTS.md")).unwrap(),
        "inside"
    );
    for outside in ["../escape", "work/../../escape", "/etc/escape"] {
        let error = world.write(outside, "x").unwrap_err();
        assert!(error.0.contains(outside), "{error}");
        assert!(error.0.contains("outside the world root"), "{error}");
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(std::env::temp_dir(), world.work().join("link")).unwrap();
        let error = world.write("work/link/escape", "x").unwrap_err();
        assert!(error.0.contains("work/link/escape"), "{error}");

        // A refused seed creates nothing outside either, even a directory on
        // the way to the file.
        let outside = World::new(Setup::default()).unwrap();
        std::os::unix::fs::symlink(outside.root(), world.work().join("out")).unwrap();
        let error = world.write("work/out/created/escape", "x").unwrap_err();
        assert!(error.0.contains("work/out/created/escape"), "{error}");
        assert!(!outside.root().join("created").exists());
    }
}

#[cfg(unix)]
#[test]
fn dropping_a_world_removes_read_only_directories_too() {
    use std::os::unix::fs::PermissionsExt;
    let world = World::new(Setup::default()).unwrap();
    world.write("work/locked/inner/file", "x").unwrap();
    for (path, mode) in [
        ("work/locked/inner/file", 0o400),
        ("work/locked/inner", 0o500),
        ("work/locked", 0o500),
    ] {
        std::fs::set_permissions(
            world.root().join(path),
            std::fs::Permissions::from_mode(mode),
        )
        .unwrap();
    }
    let root = world.root().to_path_buf();
    drop(world);
    assert!(!root.exists(), "{} was left behind", root.display());
}
