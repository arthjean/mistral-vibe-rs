//! Where skills are read from, and which of them survive the configuration.
//!
//! The corpus replay in `skills_parity_tests` measures these same rules against
//! the reference's own answers. These tests state them against the port's entry
//! point, `discover_extensions`, so a regression names the rule it broke rather
//! than a scenario number.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::extensions::{DiscoveryRoots, SkillDefinition, discover_extensions};
use crate::skills::{SearchInputs, SkillDiscovery, SkillScope, SkillSource, search_paths};

/// A `SKILL.md` under `directory`, named after its own frontmatter.
fn write_skill(directory: &Path, name: &str, description: &str) {
    let skill = directory.join(name);
    fs::create_dir_all(&skill).expect("the skill directory is writable");
    fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
    )
    .expect("the skill file is writable");
}

/// The scenario roots every test here shares: a project directory, a home
/// directory holding both `.vibe` and `.agents`, and an extra directory a
/// `skill_paths` entry can name.
struct Tree {
    _temporary: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
    extra: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("a scratch directory is available");
        // The roots are canonicalized so a platform that hands out a symlinked
        // temporary directory still compares equal to what discovery resolves.
        let root = temporary
            .path()
            .canonicalize()
            .expect("the scratch directory resolves");
        let (project, home, extra) = (root.join("project"), root.join("home"), root.join("extra"));
        for directory in [&project, &home, &extra] {
            fs::create_dir_all(directory).expect("the scenario root is writable");
        }
        Self {
            _temporary: temporary,
            project,
            home,
            extra,
        }
    }

    fn vibe_home(&self) -> PathBuf {
        self.home.join(".vibe")
    }

    /// The discovery a trusted session over this tree resolves.
    fn discovery(&self, configured: &[String], trusted: bool) -> SkillDiscovery {
        let projects = if trusted {
            vec![self.project.clone()]
        } else {
            Vec::new()
        };
        SkillDiscovery {
            roots: search_paths(&SearchInputs {
                configured,
                projects: &projects,
                vibe_home: &self.vibe_home(),
                user_home: Some(&self.home),
                working_directory: &self.project,
            }),
            enabled: Vec::new(),
            disabled: Vec::new(),
        }
    }

    /// The names `discover_extensions` publishes for `skills`.
    fn published(&self, skills: SkillDiscovery) -> Vec<String> {
        discover_extensions(
            &DiscoveryRoots {
                skills,
                ..DiscoveryRoots::default()
            },
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
        )
        .skills
        .into_keys()
        .collect()
    }
}

/// US-165: the four documented roots are read, and the fifth is the deprecated
/// one this port keeps so an existing installation does not stop loading.
#[test]
fn all_five_search_roots_are_walked() {
    let tree = Tree::new();
    write_skill(&tree.project.join(".vibe/skills"), "alpha", "project vibe");
    write_skill(
        &tree.project.join(".agents/skills"),
        "beta",
        "project agents",
    );
    write_skill(&tree.home.join(".vibe/skills"), "gamma", "user vibe");
    write_skill(&tree.home.join(".agents/skills"), "delta", "user agents");
    write_skill(
        &tree.home.join(".vibe/extensions/skills"),
        "ghost",
        "the deprecated root",
    );

    let published = tree.published(tree.discovery(&[], true));

    assert_eq!(
        published,
        vec!["alpha", "beta", "delta", "gamma", "ghost"],
        "every documented root is read, and the deprecated one still is"
    );
}

/// US-165: precedence runs configured, then project, then user, and a name
/// found earlier is the one published.
#[test]
fn the_first_root_holding_a_name_wins() {
    let tree = Tree::new();
    for (root, description) in [
        (tree.extra.clone(), "configured"),
        (tree.project.join(".vibe/skills"), "project"),
        (tree.project.join(".agents/skills"), "project agents"),
        (tree.home.join(".vibe/skills"), "user"),
        (tree.home.join(".agents/skills"), "user agents"),
        (tree.home.join(".vibe/extensions/skills"), "deprecated"),
    ] {
        write_skill(&root, "shared", description);
    }
    let configured = vec![tree.extra.to_string_lossy().into_owned()];

    let ranked = |configured: &[String], trusted: bool| {
        discover_extensions(
            &DiscoveryRoots {
                skills: tree.discovery(configured, trusted),
                ..DiscoveryRoots::default()
            },
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
        )
        .skills["shared"]
            .description
            .clone()
    };

    assert_eq!(ranked(&configured, true), "configured");
    assert_eq!(ranked(&[], true), "project");
    assert_eq!(
        ranked(&[], false),
        "user",
        "an untrusted project contributes no root, so the user one wins"
    );
    fs::remove_dir_all(tree.home.join(".vibe/skills")).expect("the user root is removable");
    assert_eq!(ranked(&[], false), "user agents");
    fs::remove_dir_all(tree.home.join(".agents/skills")).expect("the agents root is removable");
    assert_eq!(
        ranked(&[], false),
        "deprecated",
        "the deprecated root ranks after both documented user roots"
    );
}

/// US-165: an untrusted workspace contributes no root at all, so a project
/// shipping a skill cannot publish it without trust.
#[test]
fn an_untrusted_project_is_not_walked() {
    let tree = Tree::new();
    write_skill(&tree.project.join(".vibe/skills"), "alpha", "project");
    write_skill(&tree.project.join(".agents/skills"), "beta", "project");
    write_skill(&tree.home.join(".vibe/skills"), "gamma", "user");

    assert_eq!(tree.published(tree.discovery(&[], false)), vec!["gamma"]);
}

/// US-165: two spellings of one directory are walked once, so a skill reached
/// through a symlinked root is not read twice.
#[cfg(unix)]
#[test]
fn a_root_reached_twice_is_walked_once() {
    let tree = Tree::new();
    write_skill(&tree.extra, "iota", "configured");
    let link = tree.project.join("link-to-extra");
    std::os::unix::fs::symlink(&tree.extra, &link).expect("the scenario symlink is creatable");

    let configured = vec![
        tree.extra.to_string_lossy().into_owned(),
        link.to_string_lossy().into_owned(),
    ];
    let roots = tree.discovery(&configured, true).roots;

    assert_eq!(
        roots.iter().filter(|root| **root == tree.extra).count(),
        1,
        "the two spellings resolve to one root: {roots:?}"
    );
    assert_eq!(
        tree.published(tree.discovery(&configured, true)),
        vec!["iota"]
    );
}

/// US-166: a configured directory leads the walk, and the entries that name
/// nothing walkable are skipped without taking the rest of the roots down.
#[test]
fn configured_paths_lead_and_unusable_entries_are_skipped() {
    let tree = Tree::new();
    write_skill(&tree.extra, "epsilon", "configured");
    write_skill(&tree.project.join(".vibe/skills"), "alpha", "project");
    let plain = tree.extra.join("plain.txt");
    fs::write(&plain, "not a directory").expect("the fixture file is writable");

    let configured = vec![
        tree.extra.join("missing").to_string_lossy().into_owned(),
        plain.to_string_lossy().into_owned(),
        tree.extra.to_string_lossy().into_owned(),
    ];
    let discovery = tree.discovery(&configured, true);

    assert_eq!(
        discovery.roots.first(),
        Some(&tree.extra),
        "the one usable configured entry is the first root: {:?}",
        discovery.roots
    );
    assert_eq!(tree.published(discovery), vec!["alpha", "epsilon"]);
}

/// US-166: a relative entry is anchored on the working directory and a leading
/// `~` becomes the home directory, matching the reference's before-validator.
#[test]
fn relative_and_tilde_entries_are_expanded() {
    let tree = Tree::new();
    write_skill(&tree.project.join("rel-skills"), "eta", "relative");
    write_skill(&tree.home.join("tilde-skills"), "theta", "tilde");

    let configured = vec!["./rel-skills".to_owned(), "~/tilde-skills".to_owned()];

    assert_eq!(
        tree.published(tree.discovery(&configured, true)),
        vec!["eta", "theta"]
    );
}

/// US-166: the key is read per catalog build, so changing it changes what the
/// next build publishes. This is the behavior-change proof the story owes.
#[test]
fn changing_the_configured_paths_changes_the_published_set() {
    let tree = Tree::new();
    write_skill(&tree.extra, "epsilon", "configured");

    assert!(tree.published(tree.discovery(&[], true)).is_empty());
    assert_eq!(
        tree.published(tree.discovery(&[tree.extra.to_string_lossy().into_owned()], true)),
        vec!["epsilon"]
    );
}

/// US-167: `enabled_skills` decides alone, and `disabled_skills` is not
/// consulted at all while it holds an entry.
#[test]
fn the_allowlist_wins_over_the_denylist() {
    let tree = Tree::new();
    for name in ["alpha", "beta", "search-code"] {
        write_skill(&tree.project.join(".vibe/skills"), name, "a skill");
    }
    let filtered = |enabled: &[&str], disabled: &[&str]| {
        let mut discovery = tree.discovery(&[], true);
        discovery.enabled = enabled.iter().map(|entry| (*entry).to_owned()).collect();
        discovery.disabled = disabled.iter().map(|entry| (*entry).to_owned()).collect();
        tree.published(discovery)
    };

    assert_eq!(filtered(&["alpha"], &["alpha"]), vec!["alpha"]);
    assert_eq!(filtered(&[], &["alpha"]), vec!["beta", "search-code"]);
    assert_eq!(filtered(&["SEARCH-*"], &[]), vec!["search-code"]);
    assert_eq!(filtered(&["re:(ALPHA|beta)"], &[]), vec!["alpha", "beta"]);
    assert_eq!(
        filtered(&["re:["], &[]),
        Vec::<String>::new(),
        "an allowlist holding only an uncompilable expression publishes nothing"
    );
    assert_eq!(
        filtered(&[], &["re:["]),
        vec!["alpha", "beta", "search-code"],
        "an uncompilable denylist entry withholds nothing and the catalog still builds"
    );
    assert_eq!(
        filtered(&[], &["", "   "]),
        vec!["alpha", "beta", "search-code"],
        "blank entries are skipped rather than matching everything"
    );
}

/// A skill the catalog is seeded with rather than walked to, which is the shape
/// US-169 will hand `discover_extensions` for each builtin.
fn seeded(name: &str) -> SkillDefinition {
    SkillDefinition {
        name: name.to_owned(),
        description: "a seeded skill".to_owned(),
        license: None,
        compatibility: None,
        metadata: BTreeMap::new(),
        allowed_tools: Vec::new(),
        user_invocable: true,
        body: String::new(),
        source: SkillSource::Builtin,
        scope: SkillScope::Global,
        path: None,
    }
}

/// US-167: the filter runs after the catalog is seeded, so a pattern matching a
/// builtin name withholds it exactly as it withholds a skill read from disk.
/// The reference applies its filters after seeding for the same reason.
#[test]
fn a_disabled_pattern_withholds_a_seeded_skill() {
    let tree = Tree::new();
    write_skill(&tree.project.join(".vibe/skills"), "alpha", "on disk");
    let mut discovery = tree.discovery(&[], true);
    discovery.disabled = vec!["vi*".to_owned()];

    let published = discover_extensions(
        &DiscoveryRoots {
            skills: discovery,
            ..DiscoveryRoots::default()
        },
        BTreeMap::new(),
        BTreeMap::from([("vibe".to_owned(), seeded("vibe"))]),
        BTreeMap::new(),
    )
    .skills
    .into_keys()
    .collect::<Vec<_>>();

    assert_eq!(
        published,
        vec!["alpha"],
        "a seeded name is filtered like any other"
    );
}

/// US-168: a file that will not parse is one issue naming it, and the walk
/// carries on. A directory that ships no `SKILL.md` is not an error.
#[test]
fn unloadable_skills_accumulate_as_issues() {
    let tree = Tree::new();
    let root = tree.project.join(".vibe/skills");
    write_skill(&root, "alpha", "valid");
    for name in ["bad-a", "bad-b", "bad-c"] {
        fs::create_dir_all(root.join(name)).expect("the skill directory is writable");
        fs::write(root.join(name).join("SKILL.md"), "no frontmatter here")
            .expect("the skill file is writable");
    }
    fs::create_dir_all(root.join("no-manifest")).expect("the empty directory is writable");

    let catalog = discover_extensions(
        &DiscoveryRoots {
            skills: tree.discovery(&[], true),
            ..DiscoveryRoots::default()
        },
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
    );

    assert_eq!(
        catalog.skills.keys().collect::<Vec<_>>(),
        vec!["alpha"],
        "the valid skill survives its neighbors"
    );
    let issues = catalog
        .issues
        .iter()
        .filter(|issue| issue.mechanism == "skills")
        .collect::<Vec<_>>();
    assert_eq!(issues.len(), 3, "{issues:?}");
    assert!(
        issues
            .iter()
            .all(|issue| issue.path.ends_with("SKILL.md") && !issue.message.is_empty()),
        "every issue names the file and a reason: {issues:?}"
    );
}
