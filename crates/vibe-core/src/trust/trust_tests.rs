use std::fs;

use super::*;

fn store(root: &Path) -> TrustStore {
    TrustStore::for_vibe_home(&root.join("vibe-home"))
}

#[test]
fn a_missing_trust_file_is_created_empty_and_owner_only() {
    let root = tempfile::tempdir().expect("root");
    let store = store(root.path());
    let path = store.settings_path();
    assert_eq!(
        fs::read_to_string(&path).expect("created"),
        "trusted = []\nuntrusted = []\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn decisions_are_written_as_tomli_w_writes_them_and_keep_an_existing_mode() {
    let root = tempfile::tempdir().expect("root");
    let home = root.path().join("vibe-home");
    fs::create_dir_all(&home).expect("home");
    let path = home.join(TRUST_FILE);
    fs::write(&path, "trusted = []\n").expect("seeded");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
    }
    let store = TrustStore::for_vibe_home(&home);
    let odd = root.path().join("q\"u\\ote");
    fs::create_dir_all(&odd).expect("odd");
    store.add_untrusted(&odd).expect("declined");
    store.add_trusted(root.path()).expect("trusted");
    let odd = resolve(&odd)
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let root_text = resolve(root.path()).to_string_lossy().into_owned();
    assert_eq!(
        fs::read_to_string(&path).expect("written"),
        format!("trusted = [\n    \"{root_text}\",\n]\nuntrusted = [\n    \"{odd}\",\n]\n")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o644, "an existing file keeps its mode");
    }
}

#[test]
fn a_malformed_file_is_reset_and_unknown_keys_are_ignored() {
    let root = tempfile::tempdir().expect("root");
    let home = root.path().join("vibe-home");
    fs::create_dir_all(&home).expect("home");
    fs::write(home.join(TRUST_FILE), "trusted = [\n").expect("malformed");
    let reset = TrustStore::for_vibe_home(&home);
    assert_eq!(
        fs::read_to_string(reset.settings_path()).expect("reset"),
        "trusted = []\nuntrusted = []\n"
    );

    let other = root.path().join("other-home");
    fs::create_dir_all(&other).expect("other home");
    let trusted = resolve(root.path()).to_string_lossy().into_owned();
    let seeded = format!("extra = true\ntrusted = [{trusted:?}]\n");
    fs::write(other.join(TRUST_FILE), &seeded).expect("seeded");
    let store = TrustStore::for_vibe_home(&other);
    assert_eq!(store.is_trusted(root.path()), Some(true));
    assert_eq!(
        fs::read_to_string(store.settings_path()).expect("untouched"),
        seeded,
        "reading never rewrites a readable file"
    );
}

#[test]
fn the_closest_decision_answers_and_a_grant_outranks_a_decline_at_the_same_level() {
    let root = tempfile::tempdir().expect("root");
    let store = store(root.path());
    let inner = root.path().join("inner");
    store.add_trusted(root.path()).expect("trusted");
    store.add_untrusted(&inner).expect("declined");
    assert_eq!(store.is_trusted(&root.path().join("child")), Some(true));
    assert_eq!(store.is_trusted(&inner.join("child")), Some(false));
    assert_eq!(store.find_trust_root(&inner), None);
    assert_eq!(store.trust_status(&inner), TrustStatus::Untrusted);

    store.trust_for_session(&inner);
    store.trust_for_session(&inner);
    assert_eq!(store.is_trusted(&inner), Some(true));
    assert_eq!(
        store.trust_status(&inner.join("child")),
        TrustStatus::Session
    );
    store.revoke_session_trust(&inner);
    assert_eq!(
        store.trust_status(&inner),
        TrustStatus::Session,
        "grants are counted"
    );
    store.revoke_session_trust(&inner);
    assert_eq!(store.trust_status(&inner), TrustStatus::Untrusted);
    assert!(store.is_explicitly_untrusted(&inner));
    assert!(!store.is_explicitly_untrusted(&inner.join("child")));
}

#[test]
fn a_prompt_reads_the_repository_above_the_working_directory() {
    let root = tempfile::tempdir().expect("root");
    let repository = root.path().join("repo");
    fs::create_dir_all(repository.join(".git")).expect("git");
    fs::write(repository.join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");
    fs::create_dir_all(repository.join(".vibe/plugins")).expect("plugins");
    fs::create_dir_all(repository.join("pkg/sub")).expect("sub");
    fs::write(repository.join("pkg/AGENTS.md"), "").expect("nested agents");
    let store = store(root.path());
    let prompt = build_trust_prompt(&repository.join("pkg/sub"), true, &store).expect("prompt");
    assert_eq!(prompt.repo_root, Some(resolve(&repository)));
    assert!(prompt.detected_files.is_empty());
    assert_eq!(prompt.repo_detected_files, vec![".vibe/", "pkg/AGENTS.md"]);
    assert!(prompt.offer_repo_trust);
    assert_eq!(
        available_decisions(&prompt, false),
        vec![
            WorkspaceTrustDecision::TrustRepository,
            WorkspaceTrustDecision::TrustDirectory,
            WorkspaceTrustDecision::Decline,
        ]
    );
}

#[test]
fn a_git_file_does_not_mark_a_repository_root() {
    let root = tempfile::tempdir().expect("root");
    fs::write(root.path().join(".git"), "gitdir: /elsewhere\n").expect("linked");
    fs::create_dir_all(root.path().join("sub")).expect("sub");
    assert_eq!(find_git_repo_ancestor(&root.path().join("sub")), None);
}

#[test]
fn a_repository_is_explicitly_untrusted_only_when_it_is_declined_itself() {
    let root = tempfile::tempdir().expect("root");
    let repository = root.path().join("outer/repo");
    fs::create_dir_all(repository.join(".git")).expect("git");
    fs::write(repository.join(".git/HEAD"), "").expect("head");
    fs::create_dir_all(repository.join("sub")).expect("sub");
    fs::write(repository.join("sub/AGENTS.md"), "").expect("agents");
    let store = store(root.path());
    store
        .add_untrusted(&root.path().join("outer"))
        .expect("declined");
    let prompt = build_trust_prompt(&repository.join("sub"), true, &store).expect("prompt");
    assert!(!prompt.repo_explicitly_untrusted);
    assert!(prompt.offer_repo_trust);
}

#[test]
fn untrusted_config_directories_are_listed_only_under_a_trusted_directory() {
    let root = tempfile::tempdir().expect("root");
    fs::create_dir_all(root.path().join(".vibe")).expect("vibe");
    fs::write(root.path().join(".vibe/config.toml"), "").expect("config");
    fs::create_dir_all(root.path().join(".agents/skills")).expect("skills");
    let store = store(root.path());
    store
        .add_untrusted(&root.path().join(".vibe"))
        .expect("declined");
    store
        .add_untrusted(&root.path().join(".agents"))
        .expect("declined");
    assert!(find_untrusted_config_dirs(root.path(), &store).is_empty());
    store.trust_for_session(root.path());
    let resolved = resolve(root.path());
    assert_eq!(
        find_untrusted_config_dirs(root.path(), &store),
        vec![resolved.join(".agents"), resolved.join(".vibe")]
    );
}

#[test]
fn the_project_file_is_gated_on_the_directory_holding_it() {
    let root = tempfile::tempdir().expect("root");
    let working = root.path().join("sub");
    fs::create_dir_all(&working).expect("sub");
    let store = store(root.path());
    let local = working.join(".vibe/config.toml");
    let parent = root.path().join(".vibe/config.toml");
    assert!(project_config_trusted(&store, &local, &working, true));
    assert!(!project_config_trusted(&store, &parent, &working, true));
    store
        .add_untrusted(&working.join(".vibe"))
        .expect("declined");
    assert!(!project_config_trusted(&store, &local, &working, true));
    store.add_trusted(root.path()).expect("trusted");
    assert!(project_config_trusted(&store, &parent, &working, false));
}

#[test]
fn an_untrusted_config_warning_is_due_once_per_new_folder() {
    let root = tempfile::tempdir().expect("root");
    let home = root.path().join("vibe-home");
    let first = vec!["/work/.vibe".to_owned()];
    assert!(!untrusted_config_warning_due(&home, &[]));
    assert!(untrusted_config_warning_due(&home, &first));
    assert!(
        !untrusted_config_warning_due(&home, &first),
        "already warned"
    );
    let both = vec!["/work/.agents".to_owned(), "/work/.vibe".to_owned()];
    assert!(
        untrusted_config_warning_due(&home, &both),
        "a new folder warns again"
    );
    let cache: toml::Table = fs::read_to_string(home.join("cache.toml"))
        .expect("cache")
        .parse()
        .expect("toml");
    assert_eq!(
        cache["untrusted_config_warning"]["dirs"],
        toml::Value::Array(vec![
            toml::Value::String("/work/.agents".to_owned()),
            toml::Value::String("/work/.vibe".to_owned()),
        ])
    );
}
