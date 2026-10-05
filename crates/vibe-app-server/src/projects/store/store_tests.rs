//! The link store against the file the reference's `VibeProjectsStore` reads
//! and writes. The expected layouts are what `tomli_w.dumps(tomllib.loads(..))`
//! printed under the pinned reference's interpreter for the same input.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::{TempDir, tempdir};

use super::{ProjectLink, ProjectsStore, StoreError, dump, parse_document};

fn store() -> (TempDir, ProjectsStore) {
    let home = tempdir().expect("a vibe home");
    let store = ProjectsStore::in_home(home.path());
    (home, store)
}

fn remote(root: &Path, project: &str) -> ProjectLink {
    ProjectLink::Remote {
        repo_root: root.to_path_buf(),
        repo_url: "https://github.com/acme/app".to_owned(),
        project_id: project.to_owned(),
        project_name: project.to_uppercase(),
    }
}

fn local(root: &Path, project: &str) -> ProjectLink {
    ProjectLink::Local {
        directory_path: root.to_path_buf(),
        project_id: project.to_owned(),
        project_name: project.to_uppercase(),
    }
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("the directory resolves")
}

fn rewritten(text: &str) -> String {
    dump(&parse_document(text).expect("the document parses"))
}

#[test]
fn a_rewrite_lays_the_document_out_as_tomli_w_does() {
    let source = r#"
z = "x"
a=1979-05-27T07:32:00Z
b=1979-05-27T00:32:00.999999-07:00
c=07:32:00
d=1979-05-27
f=1e20
g=0.0001
h=1e16
i=-0.0
j=0.00001
k="\u0001\u007f	\"\\é"
"key with space" = 1
[t]
x = [1, [2, 3]]
[[projects]]
kind = "remote"
repo_root = "/r"
extra = { a = 1 }
"#;
    let expected = r#"z = "x"
a = 1979-05-27 07:32:00+00:00
b = 1979-05-27 00:32:00.999999-07:00
c = 07:32:00
d = 1979-05-27
f = 1e+20
g = 0.0001
h = 1e+16
i = -0.0
j = 1e-05
k = "\u0001\u007f	\"\\é"
"key with space" = 1
projects = [
    { kind = "remote", repo_root = "/r", extra = { a = 1 } },
]

[t]
x = [
    1,
    [
        2,
        3,
    ],
]
"#;
    assert_eq!(rewritten(source), expected);
}

#[test]
fn an_entry_too_wide_to_inline_is_written_as_a_table_array() {
    let long = format!("/{}", "d".repeat(80));
    let source = format!(
        "version = 1\nprojects = [{{ kind = \"local\", directory_path = \"{long}\", project_id = \"p\", project_name = \"n\" }}]\n"
    );
    let expected = format!(
        "version = 1\n\n[[projects]]\nkind = \"local\"\ndirectory_path = \"{long}\"\nproject_id = \"p\"\nproject_name = \"n\"\n"
    );
    assert_eq!(rewritten(&source), expected);
    assert_eq!(
        rewritten("version = 1\nprojects = []\n"),
        "version = 1\nprojects = []\n"
    );
}

#[test]
fn links_round_trip_and_an_upsert_replaces_either_kind_at_the_end() {
    let (home, store) = store();
    let first = tempdir().expect("a first directory");
    let second = tempdir().expect("a second directory");
    store
        .upsert_project_link(&remote(first.path(), "a"))
        .expect("the first link saves");
    store
        .upsert_project_link(&local(second.path(), "b"))
        .expect("the second link saves");
    store
        .upsert_project_link(&local(first.path(), "c"))
        .expect("the first directory is relinked");

    let links = store.list_project_links();
    assert_eq!(
        links,
        vec![
            local(&canonical(second.path()), "b"),
            local(&canonical(first.path()), "c"),
        ]
    );
    assert_eq!(store.get_remote_project(first.path()), None);
    assert_eq!(
        store.get_project_link(first.path()),
        Some(local(&canonical(first.path()), "c"))
    );
    let text = fs::read_to_string(home.path().join("projects.toml")).expect("the store is written");
    assert!(text.starts_with("version = 1\n"), "{text}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(home.path().join("projects.toml"))
            .expect("the store exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn a_remote_delete_leaves_a_directory_link_and_a_delete_always_writes() {
    let (home, store) = store();
    let directory = tempdir().expect("a directory");
    store
        .delete_project_link(directory.path())
        .expect("a delete on an empty store writes");
    assert_eq!(
        fs::read_to_string(home.path().join("projects.toml")).expect("the store is created"),
        "version = 1\nprojects = []\n"
    );

    store
        .upsert_project_link(&local(directory.path(), "a"))
        .expect("the link saves");
    store
        .delete_remote_project(directory.path())
        .expect("the remote delete writes");
    assert_eq!(store.list_project_links().len(), 1);
    store
        .delete_project_link(directory.path())
        .expect("the delete writes");
    assert!(store.list_project_links().is_empty());
}

#[test]
fn entries_the_store_cannot_read_survive_a_rewrite_and_others_are_dropped() {
    let (home, store) = store();
    let path = home.path().join("projects.toml");
    fs::write(
        &path,
        "note = \"kept\"\nversion = 3\nprojects = [1, { kind = \"cloud\", id = \"x\" }]\n",
    )
    .expect("the store fixture writes");
    let directory = tempdir().expect("a directory");
    let wide = directory.path().join("d".repeat(100));
    fs::create_dir(&wide).expect("a wide directory");
    store
        .upsert_project_link(&local(&wide, "a"))
        .expect("the link saves");

    // The new entry is wide enough to push the array out of its inline form,
    // which is the layout `tomli_w` falls back to.
    let text = fs::read_to_string(&path).expect("the store is readable");
    assert!(
        text.starts_with(
            "note = \"kept\"\nversion = 3\n\n[[projects]]\nkind = \"cloud\"\nid = \"x\"\n\n[[projects]]\nkind = \"local\"\n"
        ),
        "{text}"
    );
    assert_eq!(store.list_project_links().len(), 1);
}

#[test]
fn a_corrupt_store_reads_as_empty_and_the_next_write_replaces_it() {
    let (home, store) = store();
    let path = home.path().join("projects.toml");
    fs::write(&path, "projects = [").expect("the store fixture writes");
    assert!(store.list_project_links().is_empty());
    let directory = tempdir().expect("a directory");
    store
        .upsert_project_link(&remote(directory.path(), "a"))
        .expect("the link saves");
    assert_eq!(
        store.list_project_links(),
        vec![remote(&canonical(directory.path()), "a")]
    );
}

#[test]
fn legacy_links_are_imported_once_while_the_store_is_absent() {
    let (home, store) = store();
    let legacy = home.path().join("vibe-code-project-links.json");
    let directory = tempdir().expect("a directory");
    let root = canonical(directory.path());
    fs::write(
        &legacy,
        serde_json::json!({
            root.to_string_lossy(): {
                "repoUrl": "https://github.com/acme/app",
                "projectId": "a",
                "projectName": "A",
            },
            "/elsewhere": {"repoUrl": "", "projectId": "b", "projectName": "B"},
            "/broken": {"projectId": "c"},
        })
        .to_string(),
    )
    .expect("the legacy fixture writes");

    assert_eq!(store.list_project_links(), vec![remote(&root, "a")]);
    assert!(!legacy.exists(), "the legacy file is removed once imported");
    assert!(home.path().join("projects.toml").exists());

    // A store that already exists is never merged with a legacy file.
    store
        .delete_project_link(&root)
        .expect("the link is removed");
    fs::write(
        &legacy,
        serde_json::json!({root.to_string_lossy(): {
            "repoUrl": "https://github.com/acme/app", "projectId": "a", "projectName": "A",
        }})
        .to_string(),
    )
    .expect("the legacy fixture writes again");
    assert!(store.list_project_links().is_empty());
    assert!(legacy.exists());
}

#[test]
fn a_failed_write_reads_as_python_reports_an_os_error() {
    let error = StoreError {
        path: PathBuf::from("/home/u/.vibe/projects.toml"),
        source: std::io::Error::from_raw_os_error(13),
    };
    assert_eq!(
        error.to_string(),
        "[Errno 13] Permission denied: '/home/u/.vibe/projects.toml'"
    );
}
