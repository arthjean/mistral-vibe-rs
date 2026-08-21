//! Project discovery and configuration source resolution, exercised through
//! `LayeredConfig::load` and the write path rather than through the resolver
//! alone: what a session reads and writes is the observable contract.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use toml::Table;

use super::harness::{ConfigSource, HarnessFiles};
use super::{
    ConfigMutation, ConfigPaths, ConfigTarget, ConfigWrite, LayeredConfig, PatchOperation,
};
use crate::config::JsonPointer;

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

fn write_config(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("configuration directory");
    }
    fs::write(path, contents).expect("configuration fixture");
}

fn project_config(directory: &Path) -> PathBuf {
    directory.join(".vibe/config.toml")
}

fn set(pointer: &str, value: &str) -> ConfigMutation {
    ConfigMutation {
        pointer: JsonPointer::parse(pointer).expect("pointer"),
        operation: PatchOperation::Set(toml::Value::String(value.to_owned())),
    }
}

#[test]
fn the_walk_finds_the_nearest_project_file_above_the_working_directory() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    let repository = root.join("work");
    let nested = repository.join("a/b/c");
    fs::create_dir_all(&nested).expect("nested working directory");
    write_config(&project_config(&repository), "active_model = \"root\"\n");

    let snapshot = store(root, &nested)
        .with_project_trusted(true)
        .load()
        .expect("configuration composes");

    assert_eq!(snapshot.selected_target, ConfigTarget::Project);
    assert_eq!(snapshot.selected_path, project_config(&repository));
    assert_eq!(snapshot.effective["active_model"].as_str(), Some("root"));

    // A file closer to the working directory wins over the repository root.
    write_config(
        &project_config(&repository.join("a")),
        "active_model = \"nearest\"\n",
    );
    let snapshot = store(root, &nested)
        .with_project_trusted(true)
        .load()
        .expect("configuration composes");
    assert_eq!(
        snapshot.selected_path,
        project_config(&repository.join("a"))
    );
    assert_eq!(snapshot.effective["active_model"].as_str(), Some("nearest"));
}

#[test]
fn the_walk_stops_before_the_directory_holding_the_vibe_home() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    // `{root}/home/.vibe/config.toml` is the user file. A walk that inspected
    // `{root}/home` would adopt it as a project file.
    write_config(
        &vibe_home(root).join("config.toml"),
        "active_model = \"user\"\n",
    );
    let working_directory = root.join("home/projects/service");
    fs::create_dir_all(&working_directory).expect("working directory");

    let store = store(root, &working_directory).with_project_trusted(true);
    assert_eq!(store.paths.discovered_project_config(), None);

    let snapshot = store.load().expect("configuration composes");
    assert_eq!(snapshot.selected_target, ConfigTarget::User);
    assert_eq!(snapshot.selected_path, vibe_home(root).join("config.toml"));
    assert_eq!(snapshot.effective["active_model"].as_str(), Some("user"));
    assert!(snapshot.target_values[&ConfigTarget::Project].is_empty());
}

#[test]
fn an_untrusted_discovered_file_leaves_the_project_layer_empty() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    let repository = root.join("work");
    let nested = repository.join("a/b");
    fs::create_dir_all(&nested).expect("nested working directory");
    write_config(&project_config(&repository), "active_model = \"project\"\n");
    write_config(
        &vibe_home(root).join("config.toml"),
        "active_model = \"user\"\n",
    );

    let snapshot = store(root, &nested)
        .with_project_trusted(false)
        .load()
        .expect("configuration composes");

    assert_eq!(snapshot.selected_target, ConfigTarget::User);
    assert!(snapshot.target_values[&ConfigTarget::Project].is_empty());
    assert_eq!(snapshot.effective["active_model"].as_str(), Some("user"));
}

#[test]
fn a_symlinked_working_directory_terminates_the_walk() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    let target = root.join("work/a/b");
    fs::create_dir_all(&target).expect("link target");
    write_config(&project_config(&target), "active_model = \"linked\"\n");
    let link = root.join("link");
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&target, &link).is_ok();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(&target, &link).is_ok();
    if !linked {
        // Creating a symlink needs a privilege this host does not grant; the
        // walk is lexical either way.
        return;
    }

    let snapshot = store(root, &link)
        .with_project_trusted(true)
        .load()
        .expect("configuration composes");

    assert_eq!(snapshot.selected_target, ConfigTarget::Project);
    assert_eq!(snapshot.effective["active_model"].as_str(), Some("linked"));
}

#[test]
fn only_the_project_source_refuses_a_user_write() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    let repository = root.join("work");
    fs::create_dir_all(&repository).expect("working directory");
    write_config(&project_config(&repository), "active_model = \"project\"\n");
    let user_path = vibe_home(root).join("config.toml");
    write_config(&user_path, "active_model = \"user\"\n");
    let before = fs::read_to_string(&user_path).expect("user file");

    let store = store(root, &repository)
        .with_project_trusted(true)
        .with_sources(BTreeSet::from([ConfigSource::Project]));
    let snapshot = store.load().expect("configuration composes");

    assert_eq!(snapshot.selected_target, ConfigTarget::Project);
    // The user file is neither read nor writable while its source is closed.
    assert!(snapshot.target_values[&ConfigTarget::User].is_empty());
    assert!(!store.harness_files().persist_allowed());
    let error = store
        .batch_write(&[ConfigWrite {
            target: ConfigTarget::User,
            expected_fingerprint: snapshot.fingerprints[&ConfigTarget::User].clone(),
            mutations: vec![set("/active_model", "written")],
        }])
        .expect_err("a closed source refuses the write");
    assert!(matches!(
        error,
        super::ConfigError::PersistenceDisabled(ConfigTarget::User)
    ));
    assert_eq!(
        fs::read_to_string(&user_path).expect("user file"),
        before,
        "the refused write left the file byte-identical"
    );
}

#[test]
fn no_usable_source_writes_to_an_ephemeral_layer_instead_of_disk() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    let repository = root.join("work");
    fs::create_dir_all(&repository).expect("working directory");

    let store = store(root, &repository)
        .with_project_trusted(true)
        .with_sources(BTreeSet::from([ConfigSource::Project]));
    let snapshot = store.load().expect("configuration composes");
    assert_eq!(snapshot.selected_target, ConfigTarget::Ephemeral);

    let written = store
        .batch_write(&[ConfigWrite {
            target: ConfigTarget::Ephemeral,
            expected_fingerprint: snapshot.fingerprints[&ConfigTarget::Ephemeral].clone(),
            mutations: vec![set("/active_model", "in-memory")],
        }])
        .expect("the ephemeral layer accepts the write");

    assert_eq!(
        written.effective["active_model"].as_str(),
        Some("in-memory")
    );
    assert!(!project_config(&repository).exists());
    assert!(!vibe_home(root).join("config.toml").exists());
    // A second store over the same paths starts from nothing: the document
    // never left memory.
    let fresh = super::LayeredConfig::new(
        ConfigPaths {
            vibe_home: vibe_home(root),
            working_directory: repository.clone(),
        },
        Table::new(),
    )
    .with_project_trusted(true)
    .with_sources(BTreeSet::from([ConfigSource::Project]))
    .load()
    .expect("configuration composes");
    assert!(fresh.effective.get("active_model").is_none());
}

#[test]
fn project_roots_absolutize_deduplicate_and_drop_the_working_directory() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    let repository = root.join("work");
    let sibling = root.join("sibling");
    fs::create_dir_all(&repository).expect("working directory");
    fs::create_dir_all(&sibling).expect("additional directory");

    let harness = HarnessFiles::new(
        ConfigPaths {
            vibe_home: vibe_home(root),
            working_directory: repository.clone(),
        },
        ConfigSource::all(),
        vec![
            repository.clone(),
            sibling.clone(),
            sibling.join("."),
            PathBuf::from("nested"),
            repository.join("nested/.."),
        ],
        true,
    );

    assert_eq!(
        harness.project_roots(),
        vec![
            repository.clone(),
            sibling.clone(),
            repository.join("nested"),
        ],
        "the working directory is dropped from the additional roots and the rest is deduplicated"
    );
}

#[test]
fn an_additional_directory_containing_the_working_directory_survives() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    let parent = root.join("workspace");
    let repository = parent.join("service");
    fs::create_dir_all(&repository).expect("working directory");

    let harness = HarnessFiles::new(
        ConfigPaths {
            vibe_home: vibe_home(root),
            working_directory: repository.clone(),
        },
        ConfigSource::all(),
        vec![parent.clone()],
        true,
    );

    assert_eq!(harness.project_roots(), vec![repository, parent]);
}

#[test]
fn an_untrusted_workspace_contributes_no_project_root_and_no_project_hooks() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    let repository = root.join("work");
    fs::create_dir_all(&repository).expect("working directory");

    let harness = HarnessFiles::new(
        ConfigPaths {
            vibe_home: vibe_home(root),
            working_directory: repository,
        },
        ConfigSource::all(),
        Vec::new(),
        false,
    );

    assert!(harness.project_roots().is_empty());
    assert_eq!(
        harness.config_file().map(|(target, _)| target),
        Some(ConfigTarget::User)
    );
}

/// US-260: reference `project_tools_dirs` folds `find_local_config_dirs` over
/// the open roots, which keeps `{root}/.vibe/tools` only when it is a directory
/// and never looks below the root, and reference `user_tools_dirs` answers
/// nothing at all once the user source is disabled.
#[test]
fn tool_directories_follow_the_open_roots_and_the_enabled_sources() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path();
    let repository = root.join("work");
    let sibling = root.join("sibling");
    fs::create_dir_all(repository.join(".vibe/tools")).expect("project tools directory");
    fs::create_dir_all(&sibling).expect("additional directory without one");
    fs::create_dir_all(vibe_home(root).join("tools")).expect("user tools directory");

    let paths = ConfigPaths {
        vibe_home: vibe_home(root),
        working_directory: repository.clone(),
    };
    let harness = HarnessFiles::new(paths.clone(), ConfigSource::all(), vec![sibling], true);
    assert_eq!(
        harness.project_tools_dirs(),
        vec![repository.join(".vibe/tools")],
        "a root holding no tools directory contributes none"
    );
    assert_eq!(
        harness.user_tools_dirs(),
        vec![vibe_home(root).join("tools")]
    );

    let untrusted = HarnessFiles::new(paths.clone(), ConfigSource::all(), Vec::new(), false);
    assert!(untrusted.project_tools_dirs().is_empty());

    let project_only = HarnessFiles::new(
        paths,
        BTreeSet::from([ConfigSource::Project]),
        Vec::new(),
        true,
    );
    assert!(
        project_only.user_tools_dirs().is_empty(),
        "a disabled user source contributes no user tools directory"
    );
}
