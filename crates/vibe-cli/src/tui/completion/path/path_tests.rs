use super::rank::can_possibly_fuzzy_match;
use super::*;

use std::time::{Duration, Instant};

/// Waits for `probe` to hold, up to the second the watcher's own bounds
/// allow: a 200 ms step plus the time the platform takes to report.
fn eventually(mut probe: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if probe() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    probe()
}

fn watching_index() -> PathIndex {
    let index = PathIndex::default();
    index.set_watch_enabled(true);
    index
}

/// Reference `max_entries_to_process` defaults to `None` since v2.25.3, so
/// an entry past the old 32 000-entry window is still ranked.
#[test]
fn every_indexed_entry_is_ranked() {
    let mut entries = (0..32_000)
        .map(|index| {
            let rel = format!("a{index:05}.txt");
            indexed_path(rel.clone(), rel, false)
        })
        .collect::<Vec<_>>();
    entries.push(indexed_path(
        "zzzz-needle.txt".to_owned(),
        "zzzz-needle.txt".to_owned(),
        false,
    ));
    let ranked = rank_indexed_paths(entries.iter(), "needle", true);
    assert_eq!(
        ranked.first().map(|candidate| candidate.label.as_str()),
        Some("@zzzz-needle.txt")
    );
}

#[test]
fn workspace_index_is_reused_across_queries() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::write(temporary.path().join("alpha.txt"), "fixture").expect("path fixture");
    let mut index = WorkspaceIndex::default();

    assert_eq!(index.candidates(temporary.path(), "a").unwrap().len(), 1);
    assert_eq!(index.candidates(temporary.path(), "al").unwrap().len(), 1);
    assert_eq!(index.stats().rebuilds, 1);
    assert_eq!(index.stats().incremental_updates, 0);
}

/// The first query blocks on the first walk rather than answering from an
/// index that has not been built, which is the reference's
/// `_wait_for_rebuild`.
#[test]
fn the_first_query_waits_for_the_first_build() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::write(temporary.path().join("alpha.txt"), "fixture").expect("path fixture");
    let mut index = WorkspaceIndex::default();

    let candidates = index
        .candidates(temporary.path(), "alpha")
        .expect("the first query is answered");

    assert_eq!(index.stats().rebuilds, 1);
    assert_eq!(
        candidates.first().map(|candidate| candidate.label.as_str()),
        Some("@alpha.txt")
    );
}

/// A reset drops the built index and the rules compiled for its root, so
/// the next query rebuilds both. The counters survive, as they do upstream.
#[test]
fn a_reset_rebuilds_and_recompiles_the_rules_for_the_root() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::write(temporary.path().join("alpha.txt"), "fixture").expect("path fixture");
    let mut index = WorkspaceIndex::default();
    index
        .candidates(temporary.path(), "")
        .expect("the first query is answered");
    assert!(index.rules.root.is_some());

    index.reset();
    assert!(index.entries.is_empty(), "the reset index holds nothing");
    assert!(index.rules.root.is_none(), "the rules are uncompiled");

    // A file the reset index has never seen appears, so the answer can only
    // come from a fresh walk under freshly compiled rules.
    fs::write(temporary.path().join("beta.log"), "fixture").expect("path fixture");
    fs::write(temporary.path().join("gamma.txt"), "fixture").expect("path fixture");
    let candidates = index
        .candidates(temporary.path(), "")
        .expect("the query after a reset is answered");

    assert_eq!(index.stats().rebuilds, 2, "the counters survive a reset");
    let labels = candidates
        .iter()
        .map(|candidate| candidate.label.as_str())
        .collect::<Vec<_>>();
    assert!(labels.contains(&"@gamma.txt"), "{labels:?}");
    assert!(
        !labels.contains(&"@beta.log"),
        "the recompiled rules still ignore `*.log`: {labels:?}"
    );
}

/// US-202: the key is the only thing that decides whether a watcher runs.
#[test]
fn the_configured_key_gates_the_watcher() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let index = PathIndex::default();

    index
        .candidates(temporary.path(), "")
        .expect("a query with the key off is answered");
    assert!(
        !vibe_core::sync::lock(&index.inner).is_watching(),
        "no watcher exists while the key is off"
    );

    index.set_watch_enabled(true);
    index
        .candidates(temporary.path(), "")
        .expect("a query with the key on is answered");
    assert!(
        vibe_core::sync::lock(&index.inner).is_watching(),
        "the key turns a watcher on"
    );

    index.set_watch_enabled(false);
    index
        .candidates(temporary.path(), "")
        .expect("a query after the key went off is answered");
    assert!(
        !vibe_core::sync::lock(&index.inner).is_watching(),
        "the watcher is stopped before the query is answered"
    );
}

/// US-202: a file written during a session reaches the next query without
/// a rebuild, which is what the whole watcher exists for.
#[test]
fn a_file_written_during_a_session_appears_in_the_next_query() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::write(temporary.path().join("alpha.txt"), "fixture").expect("path fixture");
    let index = watching_index();
    let candidates = index
        .candidates(temporary.path(), "")
        .expect("the first query is answered");
    assert_eq!(candidates.len(), 1);

    fs::write(temporary.path().join("beta.txt"), "fixture").expect("written during a session");

    assert!(
        eventually(|| index
            .candidates(temporary.path(), "beta")
            .is_ok_and(|candidates| candidates
                .iter()
                .any(|candidate| candidate.label == "@beta.txt"))),
        "the written file reaches the index"
    );
    let stats = index
        .inner
        .lock()
        .map(|index| index.stats())
        .expect("the index is readable");
    assert_eq!(stats.rebuilds, 1, "no rebuild answered the change");
    assert!(stats.incremental_updates >= 1, "the change was applied");
}

/// US-202: a root change stops the previous root's watcher before the new
/// one starts, so no batch can reach an index that does not describe it.
#[test]
fn a_root_change_moves_the_watcher() {
    let first = tempfile::tempdir().expect("first workspace");
    let second = tempfile::tempdir().expect("second workspace");
    fs::write(second.path().join("only-here.txt"), "fixture").expect("path fixture");
    let index = watching_index();

    index
        .candidates(first.path(), "")
        .expect("the first root is answered");
    let candidates = index
        .candidates(second.path(), "")
        .expect("the second root is answered");

    assert_eq!(
        candidates.first().map(|candidate| candidate.label.as_str()),
        Some("@only-here.txt")
    );
    let (root, rebuilds) = index
        .inner
        .lock()
        .map(|index| (index.root.clone(), index.stats().rebuilds))
        .expect("the index is readable");
    assert_eq!(rebuilds, 2, "each root was walked once");
    assert_eq!(
        root,
        Some(vibe_core::platform::canonicalize(second.path()).expect("resolved root"))
    );
}

/// US-202: an unwatchable root is a notice, never a failed query.
#[test]
fn an_unwatchable_root_reports_once_and_keeps_answering() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::write(temporary.path().join("alpha.txt"), "fixture").expect("path fixture");
    let index = watching_index();
    index
        .candidates(temporary.path(), "")
        .expect("the first query is answered");

    // Break the watcher the way an exhausted backend does: the controller
    // is asked to watch a root that is not there.
    {
        let mut held = vibe_core::sync::lock(&index.inner);
        held.watcher.stop();
        held.watch_notice_shown = false;
        let absent = temporary.path().join("absent");
        held.sync_watcher(&absent);
    }

    assert!(
        index
            .take_notice()
            .is_some_and(|notice| notice.starts_with(WATCH_UNAVAILABLE)),
        "the failure is reported"
    );
    assert!(
        index.take_notice().is_none(),
        "and reported once per session"
    );
    assert_eq!(
        index
            .candidates(temporary.path(), "alpha")
            .expect("completion keeps answering")
            .len(),
        1
    );
}

/// US-202: a running watcher never outlives the index it feeds, and the
/// drop path is bounded by the same join the reference's `shutdown` is.
#[test]
fn dropping_the_index_stops_the_watcher_within_the_join_bound() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let mut index = WorkspaceIndex::default();
    index.watch_gate.store(true, Ordering::Relaxed);
    index
        .candidates(temporary.path(), "")
        .expect("the first query is answered");
    assert!(index.is_watching(), "the query started a watcher");

    let started = Instant::now();
    drop(index);

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the watcher is joined inside the reference's one second bound"
    );
}

/// US-203: the store's own contract, driven through the categories the
/// watcher delivers.
#[test]
fn changes_are_applied_entry_by_entry() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let root = temporary.path();
    fs::create_dir_all(root.join("src/render")).expect("fixture tree");
    fs::write(root.join("src/render/mod.rs"), "fixture").expect("fixture file");
    fs::write(root.join("src/main.rs"), "fixture").expect("fixture file");
    let mut index = WorkspaceIndex::default();
    index
        .candidates(root, "")
        .expect("the fixture root is walkable");
    let root = vibe_core::platform::canonicalize(root).expect("resolved root");

    // A created file is added; a created directory brings its descendants.
    fs::write(root.join("src/added.rs"), "fixture").expect("added file");
    fs::create_dir_all(root.join("pkg/deep")).expect("added directory");
    fs::write(root.join("pkg/deep/leaf.rs"), "fixture").expect("added descendant");
    fs::write(root.join("pkg/ignored.log"), "fixture").expect("ignored descendant");
    index.apply_changes(&[
        (ChangeKind::Added, root.join("src/added.rs")),
        (ChangeKind::Added, root.join("pkg")),
    ]);
    let keys = index.entries.keys().cloned().collect::<Vec<_>>();
    assert!(keys.contains(&"src/added.rs".to_owned()), "{keys:?}");
    assert!(keys.contains(&"pkg/deep/leaf.rs".to_owned()), "{keys:?}");
    assert!(
        !keys.contains(&"pkg/ignored.log".to_owned()),
        "an ignored descendant is never created: {keys:?}"
    );

    // A deleted directory takes everything beneath it by prefix.
    fs::remove_dir_all(root.join("src/render")).expect("removed directory");
    index.apply_changes(&[(ChangeKind::Deleted, root.join("src/render"))]);
    let keys = index.entries.keys().cloned().collect::<Vec<_>>();
    assert!(!keys.contains(&"src/render".to_owned()), "{keys:?}");
    assert!(!keys.contains(&"src/render/mod.rs".to_owned()), "{keys:?}");
    assert!(keys.contains(&"src/main.rs".to_owned()), "{keys:?}");

    // A path outside the root, a path that was never written and a
    // deletion the index never held are all skipped without an update.
    let counted = index.stats().incremental_updates;
    index.apply_changes(&[
        (ChangeKind::Added, root.join("../stranger.txt")),
        (ChangeKind::Added, root.join("src/never-written.rs")),
        (ChangeKind::Deleted, root.join("src/never-written.rs")),
        (ChangeKind::Modified, root.clone()),
    ]);
    assert_eq!(
        index.stats().incremental_updates,
        counted,
        "a batch that changes nothing counts no update"
    );
}

/// US-203: past the threshold, one rebuild replaces the batch.
#[test]
fn a_batch_past_the_threshold_rebuilds_instead() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let root = temporary.path();
    fs::create_dir(root.join("bulk")).expect("fixture directory");
    let mut index = WorkspaceIndex::default();
    index
        .candidates(root, "")
        .expect("the fixture root is walkable");
    let root = vibe_core::platform::canonicalize(root).expect("resolved root");

    let mut changes = Vec::new();
    for entry in 0..=MASS_CHANGE_THRESHOLD {
        let path = root.join(format!("bulk/file-{entry:03}.txt"));
        fs::write(&path, "fixture").expect("bulk file");
        changes.push((ChangeKind::Added, path));
    }

    index.apply_changes(&changes[..MASS_CHANGE_THRESHOLD]);
    assert_eq!(
        index.stats().rebuilds,
        1,
        "a batch at the threshold applies"
    );
    assert_eq!(index.stats().incremental_updates, 1);

    index.apply_changes(&changes);
    assert_eq!(index.stats().rebuilds, 2, "a batch past it rebuilds");
    assert_eq!(
        index.stats().incremental_updates,
        1,
        "and counts no incremental update"
    );
    assert_eq!(
        index.entries.len(),
        MASS_CHANGE_THRESHOLD + 2,
        "the rebuilt index holds the directory and every file"
    );
}

/// US-205: every indexed entry carries the mask of its lowercased relative
/// path, and the prefilter rejects exactly the entries missing a bit the
/// query needs, before the matcher is asked anything.
#[test]
fn an_indexed_entry_carries_its_mask_and_the_prefilter_reads_it() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::create_dir(temporary.path().join("Render")).expect("fixture directory");
    fs::write(temporary.path().join("Render/Table.rs"), "fixture").expect("path fixture");
    let mut index = WorkspaceIndex::default();
    index
        .candidates(temporary.path(), "")
        .expect("the fixture root is walkable");

    let entry = index
        .entries
        .get("Render/Table.rs")
        .expect("the walked entry is held");
    assert_eq!(entry.rel_lower, "render/table.rs");
    assert_eq!(entry.ascii_mask, build_ascii_mask("render/table.rs"));

    let context = path_search_context("table");
    assert!(can_possibly_fuzzy_match(entry, &context));
    let context = path_search_context("tablez");
    assert!(
        !can_possibly_fuzzy_match(entry, &context),
        "a bit the entry lacks rejects it before the matcher"
    );
}

/// US-205: a query the mask cannot represent filters nothing.
#[test]
fn a_non_ascii_query_disables_the_mask_filter() {
    let entries = [
        indexed_path("café.txt".to_owned(), "café.txt".to_owned(), false),
        indexed_path("cafe.txt".to_owned(), "cafe.txt".to_owned(), false),
    ];

    assert!(query_ascii_mask("café").is_none());
    assert!(query_ascii_mask("cafe").is_some());
    let accented = rank_indexed_paths(entries.iter(), "café", true);
    assert_eq!(
        accented.first().map(|candidate| candidate.label.as_str()),
        Some("@café.txt"),
        "the accented entry is reachable"
    );
    assert_eq!(
        rank_indexed_paths(entries.iter(), "café", true),
        rank_indexed_paths(entries.iter(), "café", false)
    );
}

#[test]
fn default_and_gitignore_rules_match_the_reference_precedence() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::write(
        temporary.path().join(".gitignore"),
        "ignored/\n*.log\n!keep.log\nreport-[0-9].txt\n[!a]lpha.tmp\n",
    )
    .expect("ignore fixture");
    let mut rules = IgnoreRules::default();
    rules.ensure_for_root(temporary.path());
    assert!(rules.should_ignore("target", "target", true));
    assert!(rules.should_ignore("ignored", "ignored", true));
    assert!(rules.should_ignore("build.log", "build.log", false));
    assert!(!rules.should_ignore("keep.log", "keep.log", false));
    assert!(rules.should_ignore("report-7.txt", "report-7.txt", false));
    assert!(!rules.should_ignore("report-x.txt", "report-x.txt", false));
    assert!(rules.should_ignore("blpha.tmp", "blpha.tmp", false));
    assert!(!rules.should_ignore("alpha.tmp", "alpha.tmp", false));
}

#[cfg(unix)]
#[test]
fn symlink_cycles_are_listed_without_being_followed() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().expect("temporary workspace");
    symlink(temporary.path(), temporary.path().join("loop")).expect("cycle fixture");
    let candidates = PathIndex::default()
        .candidates(temporary.path(), "loop")
        .expect("cycle-safe scan");
    assert_eq!(
        candidates.first().map(|candidate| candidate.label.as_str()),
        Some("@loop")
    );
}
