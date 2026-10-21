use super::*;

#[test]
fn cache_store_survives_corruption_and_preserves_other_sections() {
    let directory = tempfile::tempdir().expect("temporary vibe home");
    let store = UpdateCacheStore::new(directory.path());
    assert_eq!(store.load(), None);
    fs::write(store.path(), "not = [toml").expect("corrupt cache");
    assert_eq!(store.load(), None);
    fs::write(store.path(), "[other]\nkept = true\n").expect("existing cache");
    let cache = UpdateCache {
        latest_version: "2.24.0".to_owned(),
        stored_at_timestamp: 42,
        seen_whats_new_version: Some("2.23.1".to_owned()),
        dismissed_version: None,
    };
    store.store(&cache);
    assert_eq!(store.load(), Some(cache));
    let text = fs::read_to_string(store.path()).expect("cache text");
    assert!(text.contains("kept = true"));
}

/// Reference `_read_section`'s migration, whose null filter is what keeps an
/// unset optional key out of the file it writes.
#[test]
fn a_legacy_json_cache_migrates_into_the_section_without_its_null_keys() {
    let directory = tempfile::tempdir().expect("temporary vibe home");
    let store = UpdateCacheStore::new(directory.path());
    fs::write(
        store.legacy_path(),
        r#"{"latest_version": "2.24.0", "stored_at_timestamp": 42,
            "seen_whats_new_version": null, "dismissed_version": "2.24.0"}"#,
    )
    .expect("legacy cache");
    assert_eq!(
        store.load(),
        Some(UpdateCache {
            latest_version: "2.24.0".to_owned(),
            stored_at_timestamp: 42,
            seen_whats_new_version: None,
            dismissed_version: Some("2.24.0".to_owned()),
        })
    );
    let migrated = fs::read_to_string(store.path()).expect("migrated cache");
    assert!(migrated.contains("dismissed_version"));
    assert!(
        !migrated.contains("seen_whats_new_version"),
        "a null legacy key is never written into the section"
    );
    // The section now answers on its own, which is what makes the migration
    // one-directional.
    fs::remove_file(store.legacy_path()).expect("drop the legacy file");
    assert_eq!(
        store.load().map(|cache| cache.latest_version),
        Some("2.24.0".to_owned())
    );
}

#[test]
fn a_populated_section_answers_before_the_legacy_file_is_read() {
    let directory = tempfile::tempdir().expect("temporary vibe home");
    let store = UpdateCacheStore::new(directory.path());
    fs::write(
        store.path(),
        "[update_cache]\nlatest_version = \"1.0.0\"\nstored_at_timestamp = 1\n",
    )
    .expect("existing section");
    fs::write(
        store.legacy_path(),
        r#"{"latest_version": "2.24.0", "stored_at_timestamp": 42}"#,
    )
    .expect("legacy cache");
    assert_eq!(store.load(), Some(UpdateCache::new("1.0.0", 1)));
    assert!(
        !fs::read_to_string(store.path())
            .expect("cache text")
            .contains("2.24.0"),
        "the legacy file is never read once the section is populated"
    );
}

/// The reference reaches `_parse` with whatever the legacy file held and
/// raises on anything but an object. This port answers absent instead, which
/// is the one place its cache store is deliberately more defensive.
#[test]
fn a_legacy_file_that_is_not_an_object_reads_as_absent_and_writes_nothing() {
    let directory = tempfile::tempdir().expect("temporary vibe home");
    let store = UpdateCacheStore::new(directory.path());
    for legacy in ["[]", "\"2.24.0\"", "7", "{not json", "null"] {
        fs::write(store.legacy_path(), legacy).expect("legacy cache");
        assert_eq!(store.load(), None, "legacy payload {legacy}");
        assert!(
            !store.path().exists(),
            "legacy payload {legacy} must write nothing"
        );
    }
}

/// Reference `_read_section` returns the legacy payload it just tried to
/// write, and `write_section` swallows its own failure, so a home nothing
/// can be written into still answers with the state it holds.
#[test]
fn a_migration_whose_write_fails_still_returns_the_legacy_values() {
    let directory = tempfile::tempdir().expect("temporary vibe home");
    let store = UpdateCacheStore::new(directory.path());
    // A directory where the cache file belongs fails every write on every
    // platform, without depending on file permissions.
    fs::create_dir(store.path()).expect("block the cache file");
    fs::write(
        store.legacy_path(),
        r#"{"latest_version": "2.24.0", "stored_at_timestamp": 42}"#,
    )
    .expect("legacy cache");
    assert_eq!(store.load(), Some(UpdateCache::new("2.24.0", 42)));
    assert!(store.path().is_dir(), "the failed write staged nothing");
}

/// Reference `write_section`, which updates the section in place instead of
/// rebuilding it, so nothing the payload omits is dropped.
#[test]
fn a_write_merges_into_the_section_and_keeps_every_key_it_does_not_carry() {
    let directory = tempfile::tempdir().expect("temporary vibe home");
    let store = UpdateCacheStore::new(directory.path());
    fs::write(
        store.path(),
        "[other]\nkept = true\n\n[update_cache]\ncustom = \"unmodeled\"\n\
         latest_version = \"1.0.0\"\nstored_at_timestamp = 1\n\
         dismissed_version = \"1.0.0\"\n",
    )
    .expect("existing cache");
    store.store(&UpdateCache::new("2.24.0", 42));
    let text = fs::read_to_string(store.path()).expect("cache text");
    assert!(text.contains("kept = true"), "a sibling table survives");
    assert!(
        text.contains("custom = \"unmodeled\""),
        "a key this port does not model survives"
    );
    // The disk carries the merge, while the store that wrote remembers the
    // value it was given, as the reference repository does.
    assert_eq!(store.load(), Some(UpdateCache::new("2.24.0", 42)));
    assert_eq!(
        UpdateCacheStore::new(directory.path()).load(),
        Some(UpdateCache {
            latest_version: "2.24.0".to_owned(),
            stored_at_timestamp: 42,
            seen_whats_new_version: None,
            // The merge never removes a key, so a dismissal the payload
            // omits outlives the write.
            dismissed_version: Some("1.0.0".to_owned()),
        })
    );
}

/// Reference `write_section`'s `isinstance` guard: a section that is not a
/// table is replaced rather than merged into.
#[test]
fn a_section_that_is_not_a_table_is_replaced_by_the_written_keys() {
    let directory = tempfile::tempdir().expect("temporary vibe home");
    let store = UpdateCacheStore::new(directory.path());
    fs::write(store.path(), "update_cache = 5\n\n[other]\nkept = true\n").expect("existing cache");
    let cache = UpdateCache::new("2.24.0", 42);
    store.store(&cache);
    assert_eq!(store.load(), Some(cache));
    assert!(
        fs::read_to_string(store.path())
            .expect("cache text")
            .contains("kept = true")
    );
}

/// The reference reads the file whatever its size, so a document padded
/// past a megabyte still answers and keeps its other tables on the next
/// write.
#[test]
fn a_large_cache_is_read_whole_and_keeps_its_other_tables() {
    let directory = tempfile::tempdir().expect("temporary vibe home");
    let store = UpdateCacheStore::new(directory.path());
    let padding = "#".repeat(1_100_000);
    fs::write(
        store.path(),
        format!(
            "[other]\nkept = true\n{padding}\n[update_cache]\nlatest_version = \"1.0.0\"\nstored_at_timestamp = 1\n"
        ),
    )
    .expect("large cache");
    assert_eq!(store.load(), Some(UpdateCache::new("1.0.0", 1)));
    store.store(&UpdateCache::new("2.24.0", 42));
    assert!(
        fs::read_to_string(store.path())
            .expect("cache text")
            .contains("kept = true"),
        "the unrelated table survives the write"
    );
}

/// Reference `FileSystemCacheStore.write_section` logs a failed write and
/// returns, and the repository remembers the value it was given, so the
/// same store answers with it while the disk keeps the previous entry.
#[cfg(unix)]
#[test]
fn a_failed_write_is_remembered_and_leaves_the_previous_file_intact() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().expect("temporary vibe home");
    let store = UpdateCacheStore::new(directory.path());
    let previous = UpdateCache::new("2.24.0", 42);
    store.store(&previous);

    let seal = |mode| {
        let mut permissions = fs::metadata(directory.path())
            .expect("vibe home metadata")
            .permissions();
        permissions.set_mode(mode);
        fs::set_permissions(directory.path(), permissions).expect("vibe home permissions");
    };
    seal(0o500);
    let ignores_permissions = fs::write(directory.path().join("probe"), b"").is_ok();
    let next = UpdateCache::new("2.25.0", 43);
    store.store(&next);
    // Restored before any assertion, so a failure still leaves a removable
    // temporary directory behind.
    seal(0o700);

    if ignores_permissions {
        // A process the mode bits do not bind cannot fail this write, so
        // only the sealed run proves anything.
        return;
    }
    assert_eq!(
        store.load(),
        Some(next),
        "the store remembers what it was given"
    );
    assert_eq!(
        UpdateCacheStore::new(directory.path()).load(),
        Some(previous),
        "a staged write that never renamed leaves the previous cache on disk"
    );
}

/// Reference `get`: the disk is read once per repository, so an edit made
/// by another process after the first read is not seen by that repository.
#[test]
fn a_store_reads_the_disk_once_and_clones_share_what_it_read() {
    let directory = tempfile::tempdir().expect("temporary vibe home");
    let store = UpdateCacheStore::new(directory.path());
    store.store(&UpdateCache::new("2.24.0", 42));
    let clone = store.clone();
    UpdateCacheStore::new(directory.path()).store(&UpdateCache::new("9.0.0", 1));
    assert_eq!(store.load(), Some(UpdateCache::new("2.24.0", 42)));
    assert_eq!(clone.load(), Some(UpdateCache::new("2.24.0", 42)));
    assert_eq!(
        UpdateCacheStore::new(directory.path()).load(),
        Some(UpdateCache::new("9.0.0", 1))
    );
}
