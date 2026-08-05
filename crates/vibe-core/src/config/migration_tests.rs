//! The four configuration migrations, and the load path that applies them once.

use std::fs;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use super::migration::{BASH_READ_ONLY_MIGRATION, default_read_only_commands, migrate_document};
use super::{ConfigPaths, ConfigTarget, LayeredConfig};
use crate::platform::Platform;

fn document(contents: &str) -> Table {
    contents.parse().expect("fixture TOML")
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("array")
        .iter()
        .map(|entry| entry.as_str().expect("string").to_owned())
        .collect()
}

fn allowlist(document: &Table) -> Vec<String> {
    strings(&document["tools"]["bash"]["allowlist"])
}

fn vibe_home(root: &Path) -> PathBuf {
    root.join("home/.vibe")
}

fn store(root: &Path, working_directory: &Path) -> LayeredConfig {
    LayeredConfig::new(
        ConfigPaths {
            vibe_home: vibe_home(root),
            working_directory: working_directory.to_path_buf(),
        },
        Table::new(),
    )
}

#[test]
fn the_bash_allowlist_gains_find_and_loses_trailing_wildcards() {
    let mut fixture = document(
        r#"
applied_migrations = ["bash_read_only_defaults_v1"]
[tools.bash]
allowlist = ["git status", "git log *", "git log", "cargo *"]
"#,
    );

    assert!(migrate_document(&mut fixture));

    assert_eq!(
        allowlist(&fixture),
        vec!["cargo", "find", "git log", "git status"],
        "`find` is added, the trailing wildcards are stripped and the duplicates collapse"
    );
    // A second pass has nothing left to do.
    let mut settled = fixture.clone();
    assert!(!migrate_document(&mut settled));
    assert_eq!(settled, fixture);
}

#[test]
fn the_read_only_commands_are_unioned_in_once_and_recorded() {
    let mut fixture = document(
        r#"
[tools.bash]
allowlist = ["git status"]
"#,
    );

    assert!(migrate_document(&mut fixture));

    let migrated = allowlist(&fixture);
    for command in default_read_only_commands(super::migration::host_platform()) {
        assert!(
            migrated.iter().any(|entry| entry == command),
            "`{command}` joined the allowlist"
        );
    }
    assert!(migrated.iter().any(|entry| entry == "git status"));
    assert_eq!(
        strings(&fixture["applied_migrations"]),
        vec![BASH_READ_ONLY_MIGRATION]
    );

    // Recorded: the operator can remove any of them and they stay removed. The
    // list keeps `find`, which the first migration owns.
    let mut trimmed = fixture.clone();
    let kept = vec![
        Value::String("find".to_owned()),
        Value::String("git status".to_owned()),
    ];
    trimmed["tools"]["bash"]
        .as_table_mut()
        .expect("bash table")
        .insert("allowlist".to_owned(), Value::Array(kept));
    assert!(!migrate_document(&mut trimmed));
    assert_eq!(allowlist(&trimmed), vec!["find", "git status"]);
}

#[test]
fn the_read_only_command_set_follows_the_platform() {
    assert!(default_read_only_commands(Platform::Posix).contains(&"grep"));
    assert!(default_read_only_commands(Platform::GitBash).contains(&"grep"));
    assert_eq!(
        default_read_only_commands(Platform::Windows),
        vec!["dir", "findstr", "more", "type", "ver", "where"]
    );
}

#[test]
fn a_document_needing_nothing_is_left_alone() {
    let mut fixture = document("active_model = \"mistral-medium-3.5\"\n");

    assert!(!migrate_document(&mut fixture));

    assert_eq!(fixture, document("active_model = \"mistral-medium-3.5\"\n"));
}

#[test]
fn the_renamed_model_is_rekeyed_repointed_and_repriced() {
    let mut fixture = document(
        r#"
active_model = "devstral-2"
applied_migrations = ["bash_read_only_defaults_v1"]

[models."devstral-2"]
name = "mistral-vibe-cli-latest"
alias = "devstral-2"
temperature = 0.2
"#,
    );

    assert!(migrate_document(&mut fixture));

    let models = fixture["models"].as_table().expect("model map");
    assert!(!models.contains_key("devstral-2"), "the old key is gone");
    let migrated = models["mistral-medium-3.5"].as_table().expect("model");
    assert_eq!(migrated["alias"].as_str(), Some("mistral-medium-3.5"));
    assert_eq!(migrated["temperature"].as_float(), Some(1.0));
    assert_eq!(migrated["input_price"].as_float(), Some(1.5));
    assert_eq!(migrated["output_price"].as_float(), Some(7.5));
    assert_eq!(migrated["thinking"].as_str(), Some("high"));
    assert_eq!(migrated["supports_images"].as_bool(), Some(true));
    assert_eq!(fixture["active_model"].as_str(), Some("mistral-medium-3.5"));
}

#[test]
fn the_renamed_model_migrates_in_the_persisted_list_form_too() {
    let mut fixture = document(
        r#"
applied_migrations = ["bash_read_only_defaults_v1"]

[[models]]
name = "mistral-vibe-cli-latest"
alias = "devstral-2"

[[models]]
name = "other"
alias = "other"
"#,
    );

    assert!(migrate_document(&mut fixture));

    let models = fixture["models"].as_array().expect("model list");
    assert_eq!(models[0]["alias"].as_str(), Some("mistral-medium-3.5"));
    assert_eq!(models[0]["supports_images"].as_bool(), Some(true));
    assert_eq!(models[1]["alias"].as_str(), Some("other"));
}

#[test]
fn renamed_tools_move_their_settings_without_clobbering_the_new_key() {
    let mut fixture = document(
        r#"
applied_migrations = ["bash_read_only_defaults_v1"]
enabled_tools = ["read", "bash"]
disabled_tools = ["search_replace"]

[tools.read]
max_lines = 200

[tools.search_replace]
max_content_size = 4096
create_backup = true
strict = true
"#,
    );

    assert!(migrate_document(&mut fixture));

    let tools = fixture["tools"].as_table().expect("tools table");
    assert!(!tools.contains_key("read"));
    assert!(!tools.contains_key("search_replace"));
    assert_eq!(tools["read_file"]["max_lines"].as_integer(), Some(200));
    let edit = tools["edit"].as_table().expect("edit settings");
    assert_eq!(edit["strict"].as_bool(), Some(true));
    assert!(
        !edit.contains_key("max_content_size") && !edit.contains_key("create_backup"),
        "the options the new tool does not carry are dropped"
    );
    assert_eq!(
        strings(&fixture["enabled_tools"]),
        vec!["read_file", "bash"]
    );
    assert_eq!(strings(&fixture["disabled_tools"]), vec!["edit"]);
}

#[test]
fn an_existing_new_tool_key_survives_the_rename() {
    let mut fixture = document(
        r#"
applied_migrations = ["bash_read_only_defaults_v1"]

[tools.read]
max_lines = 200

[tools.read_file]
max_lines = 500
"#,
    );

    assert!(migrate_document(&mut fixture));

    let tools = fixture["tools"].as_table().expect("tools table");
    assert!(!tools.contains_key("read"));
    assert_eq!(
        tools["read_file"]["max_lines"].as_integer(),
        Some(500),
        "the key the operator already configured wins"
    );
}

#[test]
fn migrating_rewrites_the_user_file_once_and_leaves_an_untrusted_project_alone() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    let repository = root.join("work");
    fs::create_dir_all(repository.join(".vibe")).expect("project directory");
    fs::create_dir_all(vibe_home(root)).expect("vibe home");
    let user_path = vibe_home(root).join("config.toml");
    let project_path = repository.join(".vibe/config.toml");
    fs::write(&user_path, "active_model = \"devstral-2\"\n").expect("user fixture");
    fs::write(&project_path, "active_model = \"devstral-2\"\n").expect("project fixture");

    let store = store(root, &repository).with_project_trusted(false);
    assert!(store.migrate_sources().expect("migrations run").is_empty());
    let snapshot = store.load().expect("configuration composes");

    assert_eq!(
        snapshot.effective["active_model"].as_str(),
        Some("mistral-medium-3.5"),
        "the user file was brought forward before the first read"
    );
    assert!(
        fs::read_to_string(&user_path)
            .expect("user file")
            .contains("mistral-medium-3.5")
    );
    assert_eq!(
        fs::read_to_string(&project_path).expect("project file"),
        "active_model = \"devstral-2\"\n",
        "an untrusted project file is never rewritten"
    );
    assert!(snapshot.validation_warnings.is_empty());

    // Migrations run once per store: an external edit is not migrated again.
    fs::write(&user_path, "active_model = \"devstral-2\"\n").expect("external edit");
    assert!(store.migrate_sources().expect("migrations run").is_empty());
    assert_eq!(
        fs::read_to_string(&user_path).expect("user file"),
        "active_model = \"devstral-2\"\n"
    );
}

#[test]
fn composing_a_configuration_never_rewrites_a_file() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    let repository = root.join("work");
    fs::create_dir_all(repository.join(".vibe")).expect("project directory");
    let project_path = repository.join(".vibe/config.toml");
    let original = "active_model = \"devstral-2\"\n";
    fs::write(&project_path, original).expect("project fixture");

    // The reference migrates when the orchestrator is built, not when
    // `ConfigBuilder.build` merges, and the committed corpus records the
    // unmigrated merge.
    let snapshot = store(root, &repository)
        .with_project_trusted(true)
        .load()
        .expect("configuration composes");

    assert_eq!(
        snapshot.effective["active_model"].as_str(),
        Some("devstral-2")
    );
    assert_eq!(
        fs::read_to_string(&project_path).expect("project file"),
        original
    );
}

#[test]
fn a_trusted_project_file_is_migrated_and_selected() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    let repository = root.join("work");
    fs::create_dir_all(repository.join(".vibe")).expect("project directory");
    let project_path = repository.join(".vibe/config.toml");
    fs::write(&project_path, "disabled_tools = [\"read\", \"bash\"]\n").expect("project fixture");

    let store = store(root, &repository).with_project_trusted(true);
    store.migrate_sources().expect("migrations run");
    let snapshot = store.load().expect("configuration composes");

    assert_eq!(snapshot.selected_target, ConfigTarget::Project);
    assert_eq!(snapshot.disabled_tools(), vec!["read_file", "bash"]);
    assert!(
        fs::read_to_string(&project_path)
            .expect("project file")
            .contains("read_file")
    );
}

#[test]
fn a_failed_migration_write_is_reported_and_leaves_the_file_intact() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    let repository = root.join("work");
    fs::create_dir_all(repository.join(".vibe")).expect("project directory");
    fs::create_dir_all(vibe_home(root)).expect("vibe home");
    // The blocked file is the project one. `migrate_sources` restores the vibe
    // home's own mode before it writes anything, so a mode set on that
    // directory would not survive long enough to block the write.
    let project_path = repository.join(".vibe/config.toml");
    let original = "active_model = \"devstral-2\"\n";
    fs::write(&project_path, original).expect("project fixture");

    let blocked = block_writes(&project_path);
    if !blocked {
        return;
    }
    let store = store(root, &repository).with_project_trusted(true);
    let warnings = store.migrate_sources().expect("migrations still succeed");
    let snapshot = store.load().expect("the load succeeds");
    restore_writes(&project_path);
    assert_eq!(warnings, snapshot.validation_warnings);

    assert_eq!(
        fs::read_to_string(&project_path).expect("project file"),
        original,
        "the original file survives a failed migration"
    );
    assert_eq!(snapshot.validation_warnings.len(), 1);
    let warning = &snapshot.validation_warnings[0];
    assert!(warning.starts_with("Configuration migration skipped for"));
    assert!(
        !warning.contains("devstral-2"),
        "no configuration value reaches the warning"
    );
    assert_eq!(
        snapshot.effective["active_model"].as_str(),
        Some("devstral-2"),
        "the in-memory result stays unmigrated"
    );
}

/// Makes `path` unwritable for the migration, reporting whether the host let us.
#[cfg(windows)]
fn block_writes(path: &Path) -> bool {
    // Renaming onto a read-only file fails on Windows, which is the failure the
    // migration has to survive.
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let mut permissions = metadata.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions).is_ok()
}

#[cfg(unix)]
fn block_writes(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    if nix_is_root() {
        // A privileged process writes through the directory mode anyway.
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(metadata) = fs::metadata(parent) else {
        return false;
    };
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o500);
    fs::set_permissions(parent, permissions).is_ok()
}

#[cfg(windows)]
// The Unix hazard the lint warns about cannot apply: this arm is Windows-only,
// and it restores exactly the read-only bit `block_writes` set.
#[allow(clippy::permissions_set_readonly_false)]
fn restore_writes(path: &Path) {
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_readonly(false);
        let _ = fs::set_permissions(path, permissions);
    }
}

#[cfg(unix)]
fn restore_writes(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    if let Some(parent) = path.parent()
        && let Ok(metadata) = fs::metadata(parent)
    {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        let _ = fs::set_permissions(parent, permissions);
    }
}

#[cfg(unix)]
fn nix_is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "0")
}
