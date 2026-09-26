//! The listing cache (`.session_index.json`) and the first-user-message label.

use std::time::{Duration, UNIX_EPOCH};

use serde_json::json;

use super::*;

const INDEX: &str = ".session_index.json";
/// 2024-01-02T03:04:05Z, on a whole second so the normalized stamp has no
/// fraction.
const STAMP_MS: u64 = 1_704_164_645_000;

fn store_in(root: &Path) -> SessionStore {
    SessionStore::new(root).with_pointer_key("test-tty")
}

fn persisted_with(
    store: &SessionStore,
    id: &str,
    cwd: &str,
    now_ms: u64,
    messages: &[ModelMessage],
) -> SessionMetadata {
    let mut metadata = store
        .create(id, cwd, None, now_ms)
        .expect("session creates");
    store
        .append_messages(&mut metadata, messages, now_ms)
        .expect("messages persist");
    metadata
}

fn persisted(store: &SessionStore, id: &str, cwd: &str, now_ms: u64) -> SessionMetadata {
    persisted_with(store, id, cwd, now_ms, &[ModelMessage::user("hello")])
}

fn read_index(root: &Path) -> serde_json::Map<String, Value> {
    let bytes = fs::read(root.join(INDEX)).expect("index reads");
    match serde_json::from_slice(&bytes).expect("index parses") {
        Value::Object(entries) => entries,
        other => panic!("index is not an object: {other}"),
    }
}

fn mtime_ns(path: &Path) -> u64 {
    let modified = fs::metadata(path)
        .expect("path stats")
        .modified()
        .expect("mtime");
    u64::try_from(
        modified
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_nanos(),
    )
    .expect("fits")
}

#[test]
fn the_first_listing_writes_the_index_keyed_by_directory() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let metadata = persisted(&store, "index-one", "/workspace", STAMP_MS);
    store
        .create("index-pending", "/workspace", None, STAMP_MS)
        .expect("pending session");
    assert!(!temporary.path().join(INDEX).exists());

    let sessions = store.sessions(None).expect("listing");
    let stamp = "2024-01-02T03:04:05+00:00".to_owned();
    assert_eq!(
        sessions,
        [SessionInfo {
            session_id: "index-one".to_owned(),
            cwd: "/workspace".to_owned(),
            origin_directory: Some("/workspace".to_owned()),
            parent_session_id: None,
            title: None,
            start_time: Some(stamp.clone()),
            end_time: Some(stamp.clone()),
            bumped_at: None,
            pinned_at: None,
            updated_at: stamp,
        }]
    );

    let index = read_index(temporary.path());
    assert_eq!(index.keys().collect::<Vec<_>>(), [&metadata.directory]);
    let entry = index[&metadata.directory]
        .as_object()
        .expect("entry is an object");
    let mut keys = entry.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "bumped_at",
            "cwd",
            "end_time",
            "mtime_ns",
            "origin_directory",
            "parent_session_id",
            "pinned_at",
            "session_id",
            "start_time",
            "title",
            "updated_at",
        ]
    );
    assert_eq!(
        entry["mtime_ns"],
        mtime_ns(&store.session_path(&metadata).join(METADATA_FILE))
    );
    assert_eq!(entry["session_id"], "index-one");
    assert_eq!(entry["cwd"], "/workspace");
}

#[test]
fn a_listing_of_a_missing_or_empty_save_directory_writes_nothing() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let missing = temporary.path().join("missing");
    assert!(
        store_in(&missing)
            .sessions(None)
            .expect("listing")
            .is_empty()
    );
    assert!(!missing.exists());
    assert!(
        store_in(temporary.path())
            .sessions(None)
            .expect("listing")
            .is_empty()
    );
    assert!(!temporary.path().join(INDEX).exists());
}

#[test]
fn an_unchanged_session_is_served_from_the_cache() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let metadata = persisted(&store, "cached-session", "/workspace", STAMP_MS);
    store.sessions(None).expect("first listing");

    // A title only the cache holds proves the metadata was not read again.
    let mut index = read_index(temporary.path());
    index[&metadata.directory]["title"] = json!("from cache");
    fs::write(
        temporary.path().join(INDEX),
        serde_json::to_vec(&index).expect("index serializes"),
    )
    .expect("index rewrites");
    assert_eq!(
        store.sessions(None).expect("cached listing")[0]
            .title
            .as_deref(),
        Some("from cache")
    );

    store
        .update_title("cached-session", "Fresh")
        .expect("title updates");
    let meta = store.session_path(&metadata).join(METADATA_FILE);
    File::options()
        .write(true)
        .open(&meta)
        .expect("meta opens")
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000))
        .expect("mtime set");
    assert_eq!(
        store.sessions(None).expect("refreshed listing")[0]
            .title
            .as_deref(),
        Some("Fresh")
    );
    let refreshed = read_index(temporary.path());
    assert_eq!(refreshed[&metadata.directory]["title"], "Fresh");
    assert_eq!(
        refreshed[&metadata.directory]["mtime_ns"],
        1_000_000_000_000_000_u64
    );
}

#[test]
fn a_malformed_index_is_rebuilt_from_the_directories() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let metadata = persisted(&store, "rebuilt-session", "/workspace", STAMP_MS);
    let index_path = temporary.path().join(INDEX);

    fs::write(&index_path, b"{not json").expect("malformed index");
    let sessions = store.sessions(None).expect("listing");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "rebuilt-session");
    assert!(read_index(temporary.path()).contains_key(&metadata.directory));

    // One malformed record discards the whole cache, including the valid
    // entry beside it, so its stale title is not served.
    let mut index = read_index(temporary.path());
    index[&metadata.directory]["title"] = json!("stale");
    index.insert("ghost".to_owned(), json!({"session_id": "ghost"}));
    fs::write(
        &index_path,
        serde_json::to_vec(&index).expect("index serializes"),
    )
    .expect("index rewrites");
    let sessions = store.sessions(None).expect("listing");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].title, None);
    let rebuilt = read_index(temporary.path());
    assert!(!rebuilt.contains_key("ghost"));
    assert_eq!(rebuilt[&metadata.directory]["title"], Value::Null);
}

#[test]
fn listings_filter_by_working_or_origin_directory() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    persisted(&store, "alpha-session", "/alpha", STAMP_MS);
    persisted(&store, "beta-session", "/beta", STAMP_MS + 1_000);
    store
        .relocate("beta-session", "/gamma")
        .expect("session relocates");

    let ids = |cwd: Option<&str>| {
        store
            .sessions(cwd)
            .expect("listing")
            .into_iter()
            .map(|info| info.session_id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(Some("/alpha")), ["alpha-session"]);
    assert_eq!(ids(Some("/gamma")), ["beta-session"]);
    assert_eq!(ids(Some("/beta")), ["beta-session"]);
    assert!(ids(Some("/elsewhere")).is_empty());
    assert_eq!(ids(None).len(), 2);
}

#[test]
fn listings_are_ordered_by_most_recent_update() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    persisted(&store, "first-session", "/workspace", STAMP_MS);
    persisted(&store, "third-session", "/workspace", STAMP_MS + 3_000);
    persisted(&store, "second-session", "/workspace", STAMP_MS + 2_000);

    let ids = store
        .sessions(None)
        .expect("listing")
        .into_iter()
        .map(|info| info.session_id)
        .collect::<Vec<_>>();
    assert_eq!(ids, ["third-session", "second-session", "first-session"]);
}

#[test]
fn a_deleted_session_leaves_the_index() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let metadata = persisted(&store, "gone-session", "/workspace", STAMP_MS);
    store.sessions(None).expect("first listing");
    assert!(read_index(temporary.path()).contains_key(&metadata.directory));

    assert!(store.delete("gone-session").expect("delete"));
    assert!(store.sessions(None).expect("listing").is_empty());
    assert!(read_index(temporary.path()).is_empty());
}

#[test]
fn an_empty_log_lists_only_when_no_messages_were_recorded() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let mut emptied = store
        .create("emptied-session", "/workspace", None, STAMP_MS)
        .expect("session creates");
    store
        .replace_messages(&mut emptied, &[], STAMP_MS)
        .expect("an empty save is written");
    let truncated = persisted(&store, "truncated-session", "/workspace", STAMP_MS);
    fs::write(store.session_path(&truncated).join(MESSAGES_FILE), b"").expect("log truncated");

    let ids = store
        .sessions(None)
        .expect("listing")
        .into_iter()
        .map(|info| info.session_id)
        .collect::<Vec<_>>();
    assert_eq!(ids, ["emptied-session"]);
}

#[test]
fn first_user_message_labels_every_case() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let assistant = ModelMessage::Assistant {
        message_id: None,
        reasoning_message_id: None,
        content: "only me".to_owned(),
        reasoning: None,
        reasoning_payloads: Vec::new(),
        tool_calls: Vec::new(),
    };

    assert_eq!(
        store.first_user_message("missing-session"),
        "(session not found)"
    );

    persisted_with(
        &store,
        "silent-session",
        "/workspace",
        STAMP_MS,
        std::slice::from_ref(&assistant),
    );
    assert_eq!(
        store.first_user_message("silent-session"),
        "(no user messages)"
    );

    persisted_with(
        &store,
        "multiline-session",
        "/workspace",
        STAMP_MS,
        &[assistant, ModelMessage::user("  hello\nworld  ")],
    );
    assert_eq!(store.first_user_message("multiline-session"), "hello world");

    let long = "a".repeat(250);
    persisted_with(
        &store,
        "long-session",
        "/workspace",
        STAMP_MS,
        &[ModelMessage::user(long)],
    );
    assert_eq!(
        store.first_user_message("long-session"),
        format!("{}…", "a".repeat(200))
    );

    let corrupted = persisted(&store, "corrupt-session", "/workspace", STAMP_MS);
    fs::write(
        store.session_path(&corrupted).join(MESSAGES_FILE),
        b"{not json\n",
    )
    .expect("log corrupted");
    assert_eq!(
        store.first_user_message("corrupt-session"),
        "(corrupted session)"
    );
}
