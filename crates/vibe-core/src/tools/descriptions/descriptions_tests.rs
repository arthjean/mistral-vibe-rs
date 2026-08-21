//! The rules a description override is resolved and applied under.
//!
//! The corpus replay in `descriptions_parity_tests` measures the ordering
//! against the reference's own answers. These tests state each rule against the
//! port's entry points, [`search_paths`] and [`ToolRegistry::available`], so a
//! regression names the rule it broke rather than a scenario number.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::matching::NameFilter;
use crate::schema::ObjectSchema;
use crate::tools::descriptions::{
    DescriptionSource, DirectoryDescriptions, SearchInputs, prompts_dir, read_overrides,
    search_paths,
};
use crate::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolExecutionOutput, ToolInvocation, ToolOutputSink,
    ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
};

/// A `<tools>/prompts/<stem>.md` holding `text`.
fn write_description(tools: &Path, stem: &str, text: &str) {
    let prompts = tools.join("prompts");
    fs::create_dir_all(&prompts).expect("the prompts directory is writable");
    fs::write(prompts.join(format!("{stem}.md")), text).expect("the description is writable");
}

/// The canonical spelling of `path`, which is what [`search_paths`] answers for
/// a directory that exists.
fn resolved(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("the directory exists")
}

/// The scenario roots these tests share: a project tools directory, a user
/// tools directory, and an extra directory a `tool_paths` entry can name.
struct Tree {
    _temporary: tempfile::TempDir,
    root: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("a temporary tree");
        let root = fs::canonicalize(temporary.path()).expect("the tree resolves");
        for relative in ["project/.vibe/tools", "home/tools", "extra", "module"] {
            fs::create_dir_all(root.join(relative)).expect("the tree is writable");
        }
        fs::write(root.join("module/custom.py"), "# a tool module\n")
            .expect("the module is writable");
        Self {
            _temporary: temporary,
            root,
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// The search paths for `configured`, with the project and user directories
    /// this tree carries. A relative entry is anchored on the tree root, which
    /// stands in for the session's working directory.
    fn search(&self, configured: &[&str]) -> Vec<PathBuf> {
        self.search_from(&self.root, configured)
    }

    /// The same, from an explicit session working directory.
    fn search_from(&self, working_directory: &Path, configured: &[&str]) -> Vec<PathBuf> {
        let configured: Vec<String> = configured.iter().map(|entry| (*entry).to_owned()).collect();
        search_paths(&SearchInputs {
            configured: &configured,
            projects: &[self.path("project/.vibe/tools")],
            user: &[self.path("home/tools")],
            user_home: Some(&self.root),
            working_directory,
        })
    }
}

/// US-260: the reference order is `tool_paths`, then the project directories,
/// then the user ones. The builtin directory the reference lists first has no
/// counterpart here, because this port compiles its builtin descriptions in;
/// `descriptions_parity_tests` measures that the rest of the order matches.
#[test]
fn search_paths_follow_the_reference_order() {
    let tree = Tree::new();
    assert_eq!(
        tree.search(&["extra", "module/custom.py"]),
        vec![
            resolved(&tree.path("extra")),
            resolved(&tree.path("module/custom.py")),
            resolved(&tree.path("project/.vibe/tools")),
            resolved(&tree.path("home/tools")),
        ]
    );
}

/// US-260: reference `_compute_search_paths` deduplicates on the resolved path
/// and keeps the first occurrence, so a second spelling of a directory already
/// listed does not move it down the list.
#[test]
fn a_directory_named_twice_is_read_once_at_its_first_position() {
    let tree = Tree::new();
    let paths = tree.search(&["./project/.vibe/tools", "extra"]);
    assert_eq!(
        paths,
        vec![
            resolved(&tree.path("project/.vibe/tools")),
            resolved(&tree.path("extra")),
            resolved(&tree.path("home/tools")),
        ]
    );
}

/// US-260: the user directory is contributed by the harness files, which return
/// none when the user source is disabled. With none contributed, none appears.
#[test]
fn a_disabled_user_source_contributes_no_user_directory() {
    let tree = Tree::new();
    let paths = search_paths(&SearchInputs {
        configured: &[],
        projects: &[tree.path("project/.vibe/tools")],
        user: &[],
        user_home: Some(&tree.root),
        working_directory: &tree.path("project"),
    });
    assert_eq!(paths, vec![resolved(&tree.path("project/.vibe/tools"))]);
}

/// US-260: an entry naming nothing on disk is kept as written rather than
/// dropping the list, and contributes no prompts directory when it is read.
#[test]
fn an_entry_that_cannot_be_canonicalized_is_matched_as_written() {
    let tree = Tree::new();
    let paths = tree.search(&["absent", "extra"]);
    assert_eq!(
        paths,
        vec![
            tree.path("absent"),
            resolved(&tree.path("extra")),
            resolved(&tree.path("project/.vibe/tools")),
            resolved(&tree.path("home/tools")),
        ]
    );
    assert_eq!(prompts_dir(&tree.path("absent")), None);
    write_description(&tree.path("extra"), "read_file", "from the extra directory");
    assert_eq!(
        read_overrides(&paths).get("read_file").map(String::as_str),
        Some("from the extra directory")
    );
}

/// US-260: reference `_iter_tool_descriptions` reads the prompts directory next
/// to a `.py` entry, so the entry is kept and its sibling is what is read.
#[test]
fn a_module_entry_is_read_through_its_sibling_prompts_directory() {
    let tree = Tree::new();
    assert_eq!(
        prompts_dir(&tree.path("module/custom.py")),
        Some(tree.path("module/prompts"))
    );
    write_description(&tree.path("module"), "read_file", "from the module sibling");
    assert_eq!(
        read_overrides(&[tree.path("module/custom.py")])
            .get("read_file")
            .map(String::as_str),
        Some("from the module sibling")
    );
}

/// US-260: a relative entry is anchored on the session's working directory. The
/// process directory is the workspace root when `cargo test` runs, which holds
/// no such directory, so anchoring on it would resolve to something else.
#[test]
fn a_relative_entry_resolves_against_the_session_working_directory() {
    let tree = Tree::new();
    fs::create_dir_all(tree.path("project/tools")).expect("the session tools directory");
    let paths = tree.search_from(&tree.path("project"), &["./tools"]);
    assert_eq!(paths.first(), Some(&resolved(&tree.path("project/tools"))));
    // The process directory is the workspace root `cargo test` runs from, and
    // the session directory is under a temporary tree, so anchoring on the
    // wrong one cannot accidentally answer the same path.
    let process = std::env::current_dir().expect("a process directory");
    assert!(!tree.path("project").starts_with(&process));
}

/// US-260: a leading `~` expands to the operator's home, the spelling an
/// operator shares between machines.
#[test]
fn a_home_relative_entry_expands_to_the_operator_home() {
    let tree = Tree::new();
    let paths = tree.search(&["~/extra"]);
    assert_eq!(paths.first(), Some(&resolved(&tree.path("extra"))));
}

/// One `fixture` tool per name, described by its own compiled description.
fn registry(names: &[(&str, ToolSource)]) -> ToolRegistry {
    let registry = ToolRegistry::default();
    for (name, source) in names {
        registry
            .register(
                ToolSpec {
                    name: (*name).to_owned(),
                    description: format!("compiled description of {name}"),
                    input_schema: ObjectSchema::new().build(),
                    output_schema: None,
                    config: Value::Null,
                    state: Value::Null,
                    availability: ToolAvailability::Available,
                    presentation: ToolPresentationKind::Generic,
                    source: *source,
                    selection_priority: 0,
                },
                Arc::new(
                    |_invocation: &ToolInvocation,
                     _output: ToolOutputSink|
                     -> OwnedToolHandlerFuture {
                        Box::pin(async { Ok(ToolExecutionOutput::text("fixture")) })
                    },
                ),
            )
            .expect("the fixture tool registers");
    }
    registry
}

/// The descriptions the registry publishes, which is what
/// `SessionToolExecutor::definitions` sends to the model.
fn published(registry: &ToolRegistry) -> BTreeMap<String, String> {
    registry
        .available(None, &NameFilter::default())
        .expect("the surface publishes")
        .into_iter()
        .map(|spec| (spec.name, spec.description))
        .collect()
}

/// US-261: a non-blank file replaces the description keyed by its stem, a stem
/// naming no tool is ignored, and a later search path wins over an earlier one.
#[test]
fn a_written_description_replaces_the_published_one_and_the_later_path_wins() {
    let tree = Tree::new();
    write_description(
        &tree.path("extra"),
        "read_file",
        "read a file, as configured",
    );
    write_description(&tree.path("extra"), "grep", "search, as configured");
    write_description(&tree.path("extra"), "weather", "no tool answers to this");
    write_description(
        &tree.path("home/tools"),
        "read_file",
        "read a file, per user",
    );

    let registry = registry(&[
        ("read_file", ToolSource::BuiltIn),
        ("grep", ToolSource::BuiltIn),
    ]);
    registry.set_descriptions(Arc::new(DirectoryDescriptions::new(
        tree.search(&["extra"]),
    )));

    assert_eq!(
        published(&registry),
        BTreeMap::from([
            ("read_file".to_owned(), "read a file, per user".to_owned()),
            ("grep".to_owned(), "search, as configured".to_owned()),
        ])
    );
}

/// US-261 and FR-06: a file holding nothing but whitespace leaves the tool its
/// own description rather than publishing an empty one.
#[test]
fn a_blank_description_file_leaves_the_tool_its_own_description() {
    let tree = Tree::new();
    write_description(&tree.path("extra"), "read_file", "   \n\t\n");
    let registry = registry(&[("read_file", ToolSource::BuiltIn)]);
    registry.set_descriptions(Arc::new(DirectoryDescriptions::new(
        tree.search(&["extra"]),
    )));
    assert_eq!(
        published(&registry).get("read_file").map(String::as_str),
        Some("compiled description of read_file")
    );
}

/// US-261: reference `_iter_tool_descriptions` swallows `OSError`, so a path
/// that cannot be read as a file is skipped and the surface still publishes. A
/// directory carrying the name is the one unreadable spelling every host and
/// every privilege level agrees on.
#[test]
fn an_unreadable_description_file_leaves_the_tool_its_own_description() {
    let tree = Tree::new();
    write_description(&tree.path("extra"), "grep", "search, as configured");
    fs::create_dir_all(tree.path("extra/prompts/read_file.md")).expect("the obstruction is made");
    let registry = registry(&[
        ("read_file", ToolSource::BuiltIn),
        ("grep", ToolSource::BuiltIn),
    ]);
    registry.set_descriptions(Arc::new(DirectoryDescriptions::new(
        tree.search(&["extra"]),
    )));
    assert_eq!(
        published(&registry),
        BTreeMap::from([
            (
                "read_file".to_owned(),
                "compiled description of read_file".to_owned()
            ),
            ("grep".to_owned(), "search, as configured".to_owned()),
        ])
    );
}

/// US-261: the override is keyed by the published name, which is the name an
/// MCP or connector tool carries, so a remote tool is redescribed too.
#[test]
fn a_remote_tool_is_redescribed_by_its_published_name() {
    let tree = Tree::new();
    write_description(
        &tree.path("extra"),
        "fixture_probe",
        "the remote tool, redescribed",
    );
    let registry = registry(&[("fixture_probe", ToolSource::Mcp)]);
    registry.set_descriptions(Arc::new(DirectoryDescriptions::new(
        tree.search(&["extra"]),
    )));
    assert_eq!(
        published(&registry)
            .get("fixture_probe")
            .map(String::as_str),
        Some("the remote tool, redescribed")
    );
}

/// US-261: the source is re-read per publication, so a file written while a
/// session is open reaches the next turn rather than a cached copy of the
/// previous one.
#[test]
fn a_description_written_mid_session_reaches_the_next_publication() {
    let tree = Tree::new();
    let registry = registry(&[("read_file", ToolSource::BuiltIn)]);
    registry.set_descriptions(Arc::new(DirectoryDescriptions::new(
        tree.search(&["extra"]),
    )));
    assert_eq!(
        published(&registry).get("read_file").map(String::as_str),
        Some("compiled description of read_file")
    );

    write_description(&tree.path("extra"), "read_file", "written mid-session");
    assert_eq!(
        published(&registry).get("read_file").map(String::as_str),
        Some("written mid-session")
    );
}

/// The non-functional bound: 50 description files resolve in under 5 ms.
///
/// The measurement is the median of nine reads, so one scheduling stall on a
/// shared runner does not fail a bound the code meets.
#[test]
fn fifty_description_files_resolve_under_the_five_millisecond_bound() {
    let tree = Tree::new();
    for index in 0..50 {
        write_description(
            &tree.path("extra"),
            &format!("tool_{index:02}"),
            "a description an operator wrote for this tool",
        );
    }
    let source = DirectoryDescriptions::new(tree.search(&["extra"]));
    assert_eq!(source.overrides().len(), 50);

    let mut samples: Vec<std::time::Duration> = (0..9)
        .map(|_| {
            let start = std::time::Instant::now();
            let overrides = source.overrides();
            let elapsed = start.elapsed();
            assert_eq!(overrides.len(), 50);
            elapsed
        })
        .collect();
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    assert!(
        median < std::time::Duration::from_millis(5),
        "resolving 50 description files took {median:?}, above the 5 ms bound"
    );
}
