//! The session store: lazy persistence, directory naming, metadata rewrites,
//! lookup, lifecycle, handoff, and migration.

use std::time::{Duration, UNIX_EPOCH};

use serde_json::json;

use super::*;

const WORKSPACE: &str = "/workspace";
/// 2024-01-02T03:04:05.678Z.
const STAMP_MS: u64 = 1_704_164_645_678;

fn user(content: &str) -> ModelMessage {
    ModelMessage::user(content)
}

fn system(content: &str) -> ModelMessage {
    ModelMessage::System {
        content: content.to_owned(),
    }
}

fn assistant(content: &str) -> ModelMessage {
    ModelMessage::Assistant {
        message_id: None,
        reasoning_message_id: None,
        content: content.to_owned(),
        reasoning: None,
        reasoning_payloads: Vec::new(),
        tool_calls: Vec::new(),
    }
}

/// A store with a fixed pointer key, so no test depends on the terminal it
/// runs in.
fn store_in(root: &Path) -> SessionStore {
    SessionStore::new(root).with_pointer_key("test-tty")
}

/// Names `id` and writes one user message, which is what puts it on disk.
fn persisted(store: &SessionStore, id: &str, cwd: &str, now_ms: u64) -> SessionMetadata {
    let mut metadata = store
        .create(id, cwd, None, now_ms)
        .expect("session creates");
    store
        .append_message(&mut metadata, &user(&format!("hello from {id}")), now_ms)
        .expect("first message persists");
    metadata
}

fn set_log_mtime(store: &SessionStore, metadata: &SessionMetadata, seconds: u64) {
    File::options()
        .write(true)
        .open(store.session_path(metadata).join(MESSAGES_FILE))
        .expect("log opens")
        .set_modified(UNIX_EPOCH + Duration::from_secs(seconds))
        .expect("log mtime set");
}

fn read_meta(store: &SessionStore, metadata: &SessionMetadata) -> Value {
    let bytes =
        fs::read(store.session_path(metadata).join(METADATA_FILE)).expect("meta.json reads");
    serde_json::from_slice(&bytes).expect("meta.json parses")
}

fn write_meta(store: &SessionStore, metadata: &SessionMetadata, value: &Value) {
    fs::write(
        store.session_path(metadata).join(METADATA_FILE),
        serde_json::to_vec_pretty(value).expect("meta serializes"),
    )
    .expect("meta.json writes");
}

fn root_entries(root: &Path) -> Vec<String> {
    let mut names = fs::read_dir(root)
        .expect("root lists")
        .map(|entry| {
            entry
                .expect("entry reads")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// US-172: the synthetic pair a slash invocation appends is made of the
/// message shapes the store already persists, so it round-trips unchanged.
#[test]
fn the_invoked_skill_pair_round_trips_through_the_store() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let mut metadata = store
        .create("session-skill", WORKSPACE, None, 10)
        .expect("session creates");
    let pair = [
        user("/probe"),
        ModelMessage::Assistant {
            message_id: None,
            reasoning_message_id: None,
            content: String::new(),
            reasoning: None,
            reasoning_payloads: Vec::new(),
            tool_calls: vec![crate::events::ModelToolCall {
                id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
                name: "skill".to_owned(),
                arguments: "{\"name\":\"probe\"}".to_owned(),
            }],
        },
        ModelMessage::Tool {
            call_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
            content: format!(
                "name: probe\ncontent: {}\nBody\n</skill_content>\nskill_dir: None",
                crate::skills::skill_content_marker("probe")
            ),
            is_error: false,
        },
    ];
    store
        .append_messages(&mut metadata, &pair, 11)
        .expect("the pair persists");

    let hydrated = store.load("session-skill").expect("session loads");
    assert_eq!(hydrated.messages, pair.to_vec());
    assert_eq!(hydrated.metadata.message_count, 3);
}

#[test]
fn sessions_append_and_resume_with_the_current_system_context() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let mut metadata = store
        .create("session-alpha", WORKSPACE, None, 10)
        .expect("session creates");
    store
        .append_message(&mut metadata, &system("old system"), 11)
        .expect("system appends");
    store
        .append_message(&mut metadata, &user("hello"), 12)
        .expect("user appends");
    metadata
        .statistics
        .insert("tokens".to_owned(), Value::from(4));
    metadata.experiment_state = json!({"variant": "b"});
    store.update_metadata(&metadata).expect("metadata updates");

    let hydrated = store
        .resume(
            "session-alpha",
            "current system",
            BTreeMap::from([("model".to_owned(), Value::from("new"))]),
        )
        .expect("session resumes");
    assert_eq!(
        hydrated.messages,
        vec![system("current system"), user("hello")]
    );
    assert_eq!(hydrated.metadata.statistics["tokens"], 4);
    assert_eq!(hydrated.metadata.experiment_state, json!({"variant": "b"}));
    assert_eq!(hydrated.current_config["model"], "new");
}

#[test]
fn a_created_session_is_pending_until_its_first_message() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let mut metadata = store
        .create("pending-one", WORKSPACE, None, STAMP_MS)
        .expect("session creates");

    let opened = store.open("pending-one").expect("a pending session opens");
    assert_eq!(opened.metadata.id, "pending-one");
    assert!(opened.messages.is_empty());
    assert!(!metadata.is_persisted(&store));
    assert!(matches!(
        store.load("pending-one"),
        Err(StorageError::SessionNotFound(_))
    ));
    assert!(store.sessions(None).expect("listing").is_empty());
    assert!(root_entries(temporary.path()).is_empty());

    metadata.title = Some("kept in memory".to_owned());
    store
        .update_metadata(&metadata)
        .expect("a pending update succeeds");
    assert!(root_entries(temporary.path()).is_empty());
    assert_eq!(
        store
            .open("pending-one")
            .expect("still pending")
            .metadata
            .title
            .as_deref(),
        Some("kept in memory")
    );

    store
        .append_message(&mut metadata, &user("first"), STAMP_MS + 1)
        .expect("first message persists");
    assert!(metadata.is_persisted(&store));
    let loaded = store.load("pending-one").expect("persisted session loads");
    assert_eq!(loaded.messages, [user("first")]);
    assert_eq!(loaded.metadata.title.as_deref(), Some("kept in memory"));

    store
        .create("pending-two", WORKSPACE, None, STAMP_MS)
        .expect("second session creates");
    store.discard_pending("pending-two");
    assert!(matches!(
        store.open("pending-two"),
        Err(StorageError::SessionNotFound(_))
    ));
}

#[test]
fn a_session_holding_only_a_system_message_is_not_written() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let mut metadata = store
        .create("system-only", WORKSPACE, None, 10)
        .expect("session creates");
    store
        .append_messages(&mut metadata, &[system("the prompt")], 11)
        .expect("system message accepted");

    assert!(root_entries(temporary.path()).is_empty());
    assert_eq!(metadata.message_count, 0);
    assert_eq!(
        metadata.system_prompt,
        Some(json!({"role": "system", "content": "the prompt"}))
    );

    store
        .append_message(&mut metadata, &user("now it counts"), 12)
        .expect("user message persists");
    let meta = read_meta(&store, &metadata);
    assert_eq!(
        meta["system_prompt"],
        json!({"role": "system", "content": "the prompt"})
    );
    assert_eq!(meta["total_messages"], 1);
    let log =
        fs::read_to_string(store.session_path(&metadata).join(MESSAGES_FILE)).expect("log reads");
    assert_eq!(log.lines().count(), 1);
    assert!(!log.contains("the prompt"));
}

#[test]
fn session_directories_are_named_by_prefix_stamp_and_short_id() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let metadata = persisted(&store, "abcdef12-3456-7890", WORKSPACE, STAMP_MS);

    assert_eq!(metadata.directory, "session_20240102_030405_abcdef12");
    assert_eq!(metadata.start_time, "2024-01-02T03:04:05.678000+00:00");
    let directory = temporary.path().join(&metadata.directory);
    assert!(directory.join(METADATA_FILE).is_file());
    assert!(directory.join(MESSAGES_FILE).is_file());
    assert_eq!(
        store
            .session_directory("abcdef12-3456-7890")
            .expect("directory resolves"),
        directory
    );
}

#[test]
fn a_directory_name_collision_moves_the_stamp_forward_a_second() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    persisted(&store, "abcdef12-aaaa", WORKSPACE, STAMP_MS);
    let second = persisted(&store, "abcdef12-bbbb", WORKSPACE, STAMP_MS);

    assert_eq!(second.directory, "session_20240102_030406_abcdef12");
    assert_eq!(
        store
            .load("abcdef12-aaaa")
            .expect("first loads")
            .metadata
            .id,
        "abcdef12-aaaa"
    );
    assert_eq!(
        store
            .load("abcdef12-bbbb")
            .expect("second loads")
            .metadata
            .id,
        "abcdef12-bbbb"
    );
}

#[test]
fn a_configured_prefix_names_directories_and_registers_for_the_save_directory() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let logging = SessionLogging {
        enabled: true,
        save_dir: temporary.path().to_path_buf(),
        session_prefix: "chat".to_owned(),
        generate_titles: false,
    };
    let store = logging.store().with_pointer_key("test-tty");
    assert_eq!(store.prefix(), "chat");
    assert_eq!(SessionStore::new(temporary.path()).prefix(), "chat");

    let metadata = persisted(&store, "prefixed-session", WORKSPACE, STAMP_MS);
    assert_eq!(metadata.directory, "chat_20240102_030405_prefixed");
    assert_eq!(store.sessions(None).expect("listing").len(), 1);
    assert!(
        SessionStore::new(temporary.path())
            .with_prefix("session")
            .sessions(None)
            .expect("listing under another prefix")
            .is_empty()
    );
    register_session_prefix(temporary.path(), DEFAULT_SESSION_PREFIX);
    assert_eq!(
        SessionStore::new(temporary.path()).prefix(),
        DEFAULT_SESSION_PREFIX
    );
}

#[test]
fn metadata_is_written_pretty_in_reference_key_order_without_a_trailing_newline() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let metadata = persisted(&store, "pretty-session", WORKSPACE, STAMP_MS);
    let text = fs::read_to_string(store.session_path(&metadata).join(METADATA_FILE))
        .expect("meta.json reads");

    assert!(text.starts_with("{\n  \"session_id\": \"pretty-session\","));
    assert!(text.ends_with('}'));
    let positions = [
        "\"session_id\":",
        "\"parent_session_id\":",
        "\"start_time\":",
        "\"end_time\":",
        "\"environment\":",
        "\"origin_directory\":",
        "\"username\":",
        "\"title\":",
        "\"title_source\":",
        "\"total_messages\":",
        "\"system_prompt\":",
    ]
    .map(|key| text.find(key).unwrap_or_else(|| panic!("{key} is written")));
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn unknown_metadata_keys_survive_a_rewrite() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let mut metadata = persisted(&store, "future-session", WORKSPACE, 10);
    let mut meta = read_meta(&store, &metadata);
    meta["future_key"] = json!({"nested": true});
    meta["format_version"] = json!(1);
    write_meta(&store, &metadata, &meta);

    // The in-memory record never saw the key; the rewrite keeps it anyway.
    store
        .append_message(&mut metadata, &user("second"), 20)
        .expect("append rewrites metadata");
    let rewritten = read_meta(&store, &metadata);
    assert_eq!(rewritten["future_key"], json!({"nested": true}));
    assert!(rewritten.get("format_version").is_none());
    assert_eq!(rewritten["total_messages"], 2);

    store
        .update_title("future-session", "Renamed")
        .expect("title updates");
    let renamed = read_meta(&store, &metadata);
    assert_eq!(renamed["future_key"], json!({"nested": true}));
    assert_eq!(renamed["title"], "Renamed");
}

#[test]
fn update_title_trims_rejects_empty_and_marks_the_title_manual() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let metadata = persisted(&store, "titled-session", WORKSPACE, 10);

    let updated = store
        .update_title("titled-session", "  Named session \n")
        .expect("title updates");
    assert_eq!(updated.title.as_deref(), Some("Named session"));
    assert_eq!(updated.title_source, "manual");
    let meta = read_meta(&store, &metadata);
    assert_eq!(meta["title"], "Named session");
    assert_eq!(meta["title_source"], "manual");

    let empty = store.update_title("titled-session", "   ");
    assert!(matches!(empty, Err(StorageError::InvalidTitle)));
    assert_eq!(
        empty.map(|_| ()).expect_err("empty").to_string(),
        "Session title cannot be empty."
    );
    assert!(matches!(
        store.update_title("missing-session", "Title"),
        Err(StorageError::SessionNotFound(_))
    ));
    assert_eq!(read_meta(&store, &metadata)["title"], "Named session");
}

#[test]
fn refresh_auto_title_never_overrides_a_manual_title() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let mut metadata = persisted(&store, "auto-session", WORKSPACE, 10);

    assert!(
        store
            .refresh_auto_title(&mut metadata, "  First  ")
            .expect("auto title")
    );
    assert_eq!(metadata.title.as_deref(), Some("First"));
    assert_eq!(metadata.title_source, "auto");
    assert_eq!(read_meta(&store, &metadata)["title"], "First");
    assert!(
        !store
            .refresh_auto_title(&mut metadata, "First")
            .expect("same title")
    );
    assert!(
        !store
            .refresh_auto_title(&mut metadata, "  ")
            .expect("empty title")
    );

    // A rename on disk wins over the stale in-memory record.
    store
        .update_title("auto-session", "Mine")
        .expect("manual rename");
    assert!(
        !store
            .refresh_auto_title(&mut metadata, "Second")
            .expect("manual on disk")
    );
    assert_eq!(metadata.title.as_deref(), Some("First"));
    assert_eq!(read_meta(&store, &metadata)["title"], "Mine");

    metadata.title_source = "manual".to_owned();
    assert!(
        !store
            .refresh_auto_title(&mut metadata, "Third")
            .expect("manual in memory")
    );
}

#[test]
fn persist_bumped_at_is_monotonic() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let mut metadata = persisted(&store, "bumped-session", WORKSPACE, 10);

    assert_eq!(
        store
            .persist_bumped_at(&mut metadata, 5_000)
            .expect("first bump"),
        5_000
    );
    let first = format_iso_timestamp(5_000);
    assert_eq!(metadata.bumped_at.as_deref(), Some(first.as_str()));
    assert_eq!(read_meta(&store, &metadata)["bumped_at"], first.as_str());

    assert_eq!(
        store
            .persist_bumped_at(&mut metadata, 3_000)
            .expect("earlier bump"),
        5_000
    );
    assert_eq!(read_meta(&store, &metadata)["bumped_at"], first.as_str());

    assert_eq!(
        store
            .persist_bumped_at(&mut metadata, 9_000)
            .expect("later bump"),
        9_000
    );
    assert_eq!(
        read_meta(&store, &metadata)["bumped_at"],
        format_iso_timestamp(9_000).as_str()
    );
}

#[test]
fn relocate_keeps_the_origin_directory() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let metadata = persisted(&store, "moving-session", "/origin", 10);

    let moved = store
        .relocate("moving-session", "/moved")
        .expect("session relocates");
    assert_eq!(moved.origin_directory.as_deref(), Some("/origin"));
    assert_eq!(moved.working_directory, "/moved");
    let reloaded = store.metadata("moving-session").expect("metadata reads");
    assert_eq!(reloaded.origin_directory.as_deref(), Some("/origin"));
    assert_eq!(reloaded.working_directory, "/moved");
    assert_eq!(
        reloaded.environment["working_directory"].as_deref(),
        Some("/moved")
    );

    // A record that predates the field promotes the environment entry.
    let mut meta = read_meta(&store, &metadata);
    meta["origin_directory"] = Value::Null;
    write_meta(&store, &metadata, &meta);
    let promoted = store
        .relocate("moving-session", "/third")
        .expect("session relocates again");
    assert_eq!(promoted.origin_directory.as_deref(), Some("/moved"));
    assert_eq!(promoted.working_directory, "/third");

    assert!(matches!(
        store.relocate("missing-session", "/x"),
        Err(StorageError::SessionNotFound(_))
    ));
}

#[test]
fn delete_answers_whether_a_session_was_there_and_clears_matching_pointers() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let doomed = persisted(&store, "doomed-session", WORKSPACE, 10);
    persisted(&store, "kept-session", WORKSPACE, 20);
    store.record_pointer("doomed-session").expect("pointer a");
    let other_terminal = SessionStore::new(temporary.path()).with_pointer_key("other-tty");
    other_terminal
        .record_pointer("doomed-session")
        .expect("pointer b");
    let third_terminal = SessionStore::new(temporary.path()).with_pointer_key("third-tty");
    third_terminal
        .record_pointer("kept-session")
        .expect("pointer c");

    assert!(store.delete("doomed-session").expect("delete succeeds"));
    assert!(!store.session_path(&doomed).exists());
    assert_eq!(store.pointer(), None);
    assert_eq!(other_terminal.pointer(), None);
    assert_eq!(third_terminal.pointer().as_deref(), Some("kept-session"));
    assert!(matches!(
        store.load("doomed-session"),
        Err(StorageError::SessionNotFound(_))
    ));
    assert!(
        root_entries(temporary.path())
            .iter()
            .all(|name| !name.starts_with(".deleting-"))
    );

    assert!(!store.delete("doomed-session").expect("second delete"));
    store.record_pointer("never-written").expect("pointer");
    assert!(!store.delete("never-written").expect("absent delete"));
    assert_eq!(store.pointer(), None);
}

#[test]
fn session_logging_defaults_under_the_vibe_home() {
    let home = Path::new("/home/user/.vibe");
    let expected = SessionLogging {
        enabled: true,
        save_dir: home.join("logs").join("session"),
        session_prefix: "session".to_owned(),
        generate_titles: false,
    };
    assert_eq!(
        SessionLogging::from_effective(&toml::Table::new(), home),
        expected
    );
    assert_eq!(SessionLogging::defaults(home), expected);
    assert_eq!(default_save_dir(home), expected.save_dir);
}

#[test]
fn session_logging_reads_overrides_from_the_effective_configuration() {
    let home = Path::new("/home/user/.vibe");
    let effective: toml::Table = toml::from_str(
        "[session_logging]\nenabled = false\nsave_dir = \"/srv/sessions\"\n\
         session_prefix = \"chat\"\ngenerate_titles = true\n",
    )
    .expect("configuration parses");
    assert_eq!(
        SessionLogging::from_effective(&effective, home),
        SessionLogging {
            enabled: false,
            save_dir: PathBuf::from("/srv/sessions"),
            session_prefix: "chat".to_owned(),
            generate_titles: true,
        }
    );

    // Empty strings and mistyped values fall back to the defaults.
    let blank: toml::Table = toml::from_str(
        "[session_logging]\nenabled = \"no\"\nsave_dir = \"\"\nsession_prefix = \"\"\n",
    )
    .expect("configuration parses");
    assert_eq!(
        SessionLogging::from_effective(&blank, home),
        SessionLogging::defaults(home)
    );
}

#[test]
fn short_session_ids_keep_the_first_eight_characters() {
    assert_eq!(short_session_id("abcdef12-3456-7890"), "abcdef12");
    assert_eq!(short_session_id("short"), "short");
    assert_eq!(short_session_id(""), "");
}

#[test]
fn continue_prefers_a_valid_pointer_then_the_latest_valid_session() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let older = persisted(&store, "older", WORKSPACE, 10_000);
    let newer = persisted(&store, "newer", WORKSPACE, 20_000);
    let broken = persisted(&store, "broken", WORKSPACE, 30_000);
    let elsewhere = persisted(&store, "elsewhere", "/other", 40_000);
    fs::write(
        store.session_path(&broken).join(MESSAGES_FILE),
        b"{\"role\":\n",
    )
    .expect("corrupt the newest reachable log");
    set_log_mtime(&store, &older, 100);
    set_log_mtime(&store, &newer, 200);
    set_log_mtime(&store, &broken, 300);
    set_log_mtime(&store, &elsewhere, 400);

    store.record_pointer("older").expect("pointer");
    let continued = store
        .continue_session(WORKSPACE, "system", BTreeMap::new())
        .expect("valid pointer");
    assert_eq!(continued.metadata.id, "older");
    assert_eq!(continued.messages[0], system("system"));

    fs::write(
        temporary
            .path()
            .join(LAST_SESSION_DIRECTORY)
            .join("test-tty"),
        "stale\n",
    )
    .expect("stale pointer fixture");
    let fallback = store
        .continue_session(WORKSPACE, "system", BTreeMap::new())
        .expect("latest fallback");
    assert_eq!(fallback.metadata.id, "newer");

    store
        .record_pointer("elsewhere")
        .expect("unreachable pointer");
    assert_eq!(
        store.continue_target(WORKSPACE).expect("fallback target"),
        "newer"
    );
    assert!(matches!(
        store.continue_target("/nowhere"),
        Err(StorageError::NoSessions)
    ));
}

#[test]
fn exact_session_ids_win_over_a_shared_short_id() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    persisted(&store, "abcdefgh", WORKSPACE, 10);
    persisted(&store, "abcdefgh-1234", WORKSPACE, 1_020);

    assert_eq!(
        store.load("abcdefgh").expect("exact match").metadata.id,
        "abcdefgh"
    );
    assert_eq!(
        store
            .load("abcdefgh-1234")
            .expect("exact match")
            .metadata
            .id,
        "abcdefgh-1234"
    );
}

#[test]
fn corruption_and_ambiguous_short_ids_never_overwrite_evidence() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let first = persisted(&store, "abcd1234-one", WORKSPACE, 10);
    persisted(&store, "abcd1234-two", WORKSPACE, 20);
    persisted(&store, "wxyz5678-solo", WORKSPACE, 30);

    assert!(matches!(
        store.load("abcd1234"),
        Err(StorageError::AmbiguousSession(_))
    ));
    assert_eq!(
        store.load("wxyz5678").expect("unique short id").metadata.id,
        "wxyz5678-solo"
    );
    assert!(matches!(
        store.load("wxyz567"),
        Err(StorageError::SessionNotFound(_))
    ));

    let log = store.session_path(&first).join(MESSAGES_FILE);
    fs::write(&log, b"{\"role\":\"user\"").expect("truncate fixture log");
    let before = fs::read(&log).expect("fixture remains readable");
    assert!(matches!(
        store.load("abcd1234-one"),
        Err(StorageError::CorruptMessages { line: 1, .. })
    ));
    assert_eq!(fs::read(log).expect("evidence preserved"), before);
}

#[test]
fn path_traversal_session_ids_are_rejected() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let root = temporary.path().join("root");
    let store = store_in(&root);
    for id in ["../escape", "a/b", "", "dot.dot"] {
        assert!(matches!(
            store.create(id, WORKSPACE, None, 10),
            Err(StorageError::InvalidSessionId(_))
        ));
        assert!(matches!(
            store.create_child(id, WORKSPACE, "parent".to_owned(), 10),
            Err(StorageError::InvalidSessionId(_))
        ));
    }
    assert!(matches!(
        store.load("../escape"),
        Err(StorageError::SessionNotFound(_))
    ));
    assert!(!store.delete("../escape").expect("nothing to delete"));
    assert!(!root.exists());
    assert!(!temporary.path().join("escape").exists());
}

#[test]
fn a_durable_log_record_ahead_of_metadata_is_recovered_in_memory() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let metadata = persisted(&store, "session-recover", WORKSPACE, 10);
    let encoded = serde_json::to_string(&user("durable")).expect("message serializes");
    let mut log = OpenOptions::new()
        .append(true)
        .open(store.session_path(&metadata).join(MESSAGES_FILE))
        .expect("log opens");
    writeln!(log, "{encoded}").expect("simulated durable append");

    let mut recovered = store.load("session-recover").expect("record recovers");
    assert_eq!(recovered.metadata.message_count, 2);
    store
        .append_message(&mut recovered.metadata, &user("next"), 11)
        .expect("metadata catches up");
    let reloaded = store
        .load("session-recover")
        .expect("session remains loadable");
    assert_eq!(reloaded.messages.len(), 3);
    assert_eq!(read_meta(&store, &metadata)["total_messages"], 3);
}

#[test]
fn lifecycle_operations_are_durable_and_parent_linked() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let mut parent = persisted(&store, "parent-session", WORKSPACE, 10);
    store
        .append_message(&mut parent, &user("two"), 12)
        .expect("second message");
    let first = user("hello from parent-session");
    store
        .update_title("parent-session", "Named session")
        .expect("title updates");

    let child = store
        .fork(
            "parent-session",
            "child-session",
            "current prompt",
            BTreeMap::from([("model".to_owned(), Value::from("current"))]),
            20_000,
        )
        .expect("session forks");
    assert_eq!(
        child.metadata.parent_session_id.as_deref(),
        Some("parent-session")
    );
    assert_eq!(child.messages.len(), 3);
    assert_eq!(child.messages[0], system("current prompt"));
    assert_eq!(child.current_config["model"], "current");
    assert_eq!(store.pointer().as_deref(), Some("child-session"));
    let loaded_child = store.load("child-session").expect("child loads");
    assert_eq!(loaded_child.messages, [first.clone(), user("two")]);
    assert_eq!(loaded_child.metadata.title, None);
    assert_eq!(
        loaded_child.metadata.system_prompt,
        Some(json!({"role": "system", "content": "current prompt"}))
    );

    let rewind = store
        .rewind("parent-session", 1, BTreeMap::new(), 30_000)
        .expect("session rewinds");
    assert_eq!(rewind.messages, std::slice::from_ref(&first));
    assert_eq!(
        store
            .load("parent-session")
            .expect("rewind survives restart")
            .messages,
        [first]
    );
    assert!(matches!(
        store.rewind("parent-session", 5, BTreeMap::new(), 30_500),
        Err(StorageError::InvalidRewind {
            requested: 5,
            available: 1
        })
    ));

    store
        .close("parent-session", 31_000)
        .expect("session closes durably");
    assert_eq!(
        store
            .metadata("parent-session")
            .expect("metadata")
            .end_time
            .as_deref(),
        Some(format_iso_timestamp(31_000).as_str())
    );
    store
        .close("never-written", 31_000)
        .expect("closing an unwritten session is a no-op");

    assert!(store.delete("child-session").expect("child deletes"));
    assert!(matches!(
        store.load("child-session"),
        Err(StorageError::SessionNotFound(_))
    ));
}

#[test]
fn rewound_fork_is_published_once_without_mutating_its_parent() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let mut parent = store
        .create("parent", WORKSPACE, None, 10)
        .expect("parent session");
    parent
        .config
        .insert("model".to_owned(), Value::from("parent-model"));
    store.update_metadata(&parent).expect("parent metadata");
    store
        .append_messages(&mut parent, &[user("one"), user("two"), user("three")], 11)
        .expect("parent messages");

    let child = store
        .fork_rewound(
            "parent",
            "child",
            2,
            BTreeMap::from([("tokens".to_owned(), Value::from(42))]),
            20_000,
        )
        .expect("rewound fork");

    assert_eq!(child.metadata.parent_session_id.as_deref(), Some("parent"));
    assert_eq!(child.messages, [user("one"), user("two")]);
    assert_eq!(child.current_config["model"], "parent-model");
    assert_eq!(child.metadata.statistics["tokens"], 42);
    assert_eq!(
        store
            .load("parent")
            .expect("parent remains intact")
            .messages,
        [user("one"), user("two"), user("three")]
    );
    let entries = root_entries(temporary.path());
    assert_eq!(
        entries
            .iter()
            .filter(|name| name.ends_with("_child"))
            .count(),
        1
    );
    assert!(entries.iter().all(|name| !name.starts_with(".handoff-")));
    assert!(matches!(
        store.fork_rewound("parent", "too-far", 4, BTreeMap::new(), 21_000),
        Err(StorageError::InvalidRewind {
            requested: 4,
            available: 3
        })
    ));
}

#[test]
fn a_rewind_draft_reaches_disk_only_when_published() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let mut parent = persisted(&store, "draft-parent", WORKSPACE, 10);
    store
        .append_message(&mut parent, &user("two"), 11)
        .expect("second message");
    let hydrated = store.load("draft-parent").expect("parent loads");

    let draft = store.draft_handoff(&hydrated, "draft-child", 1, BTreeMap::new(), 5_000);
    assert_eq!(
        draft.metadata.parent_session_id.as_deref(),
        Some("draft-parent")
    );
    assert!(matches!(
        store.load("draft-child"),
        Err(StorageError::SessionNotFound(_))
    ));

    store.publish_draft(&draft, 6_000).expect("draft publishes");
    let published = store.load("draft-child").expect("draft loads");
    assert_eq!(published.messages, [user("hello from draft-parent")]);
    assert_eq!(
        published.metadata.parent_session_id.as_deref(),
        Some("draft-parent")
    );
    assert_eq!(
        store
            .load("draft-parent")
            .expect("parent intact")
            .messages
            .len(),
        2
    );
}

#[test]
fn legacy_migration_isolates_bad_entries_and_is_retryable() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let legacy = serde_json::to_vec(&json!({
        "sessionId": "legacy-session",
        "workingDirectory": WORKSPACE,
        "messages": [{"role": "user", "content": "legacy"}],
        "statistics": {"tokens": 2},
        "experiments": {"variant": "a"},
        "createdAtMs": 10,
        "updatedAtMs": 11
    }))
    .expect("legacy serializes");
    fs::write(temporary.path().join("valid.json"), &legacy).expect("legacy fixture");
    fs::write(temporary.path().join("broken.json"), b"{").expect("broken legacy fixture");

    let report = store.migrate_legacy().expect("migration completes");
    assert_eq!(report.migrated, 1);
    assert_eq!(report.issues.len(), 1);
    assert_eq!(report.issues[0].path, temporary.path().join("broken.json"));
    let migrated = store
        .load("legacy-session")
        .expect("migrated session loads");
    assert_eq!(migrated.messages, [user("legacy")]);
    assert_eq!(migrated.metadata.statistics["tokens"], 2);
    assert_eq!(migrated.metadata.experiment_state, json!({"variant": "a"}));
    assert_eq!(migrated.metadata.working_directory, WORKSPACE);
    assert!(temporary.path().join("valid.json.legacy.bak").is_file());
    assert!(!temporary.path().join("valid.json").exists());

    // The same record again is skipped, not duplicated.
    fs::write(temporary.path().join("valid.json"), &legacy).expect("legacy fixture again");
    let retry = store.migrate_legacy().expect("migration retry completes");
    assert_eq!(retry.migrated, 0);
    assert_eq!(retry.skipped, 1);
    assert_eq!(retry.issues.len(), 1);
    assert_eq!(store.sessions(None).expect("listing").len(), 1);
}

#[test]
fn reference_single_file_sessions_migrate_into_their_stem_directory() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let record = json!({
        "metadata": {
            "session_id": "abcdefgh-ref",
            "start_time": "2024-01-01T00:00:00+00:00",
            "environment": {"working_directory": WORKSPACE},
            "total_messages": 1
        },
        "messages": [{"role": "user", "content": "hi"}]
    });
    let file = temporary
        .path()
        .join("session_20240101_000000_abcdefgh.json");
    fs::write(
        &file,
        serde_json::to_vec(&record).expect("record serializes"),
    )
    .expect("reference fixture");
    let foreign = temporary.path().join("other_20240101_000000_abcdefgh.json");
    fs::write(
        &foreign,
        serde_json::to_vec(&record).expect("record serializes"),
    )
    .expect("foreign fixture");

    let report = store.migrate_legacy().expect("migration completes");
    assert_eq!(report.migrated, 1);
    assert_eq!(report.skipped, 1);
    assert!(report.issues.is_empty());
    assert!(!file.exists());
    assert!(foreign.is_file());
    assert!(
        temporary
            .path()
            .join("session_20240101_000000_abcdefgh")
            .join(METADATA_FILE)
            .is_file()
    );
    assert_eq!(
        store.load("abcdefgh-ref").expect("migrated loads").messages,
        [user("hi")]
    );
}

#[test]
fn a_store_with_nothing_to_migrate_is_left_untouched() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let empty = MigrationReport {
        migrated: 0,
        skipped: 0,
        issues: Vec::new(),
    };
    let store = store_in(temporary.path());
    assert_eq!(store.migrate_legacy().expect("empty root"), empty);
    assert!(root_entries(temporary.path()).is_empty());

    let missing = temporary.path().join("missing");
    assert_eq!(
        store_in(&missing).migrate_legacy().expect("missing root"),
        empty
    );
    assert!(!missing.exists());
}

#[test]
fn migration_lock_and_interrupted_artifacts_fail_safe() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    fs::write(temporary.path().join("queued.json"), b"{").expect("candidate fixture");
    let lock_path = temporary.path().join(MIGRATION_LOCK_FILE);
    let lock = FileLock::try_acquire(&lock_path, StorageError::MigrationInProgress)
        .expect("migration lock");
    assert!(matches!(
        store.migrate_legacy(),
        Err(StorageError::MigrationInProgress)
    ));
    drop(lock);
    fs::write(&lock_path, b"stale process marker").expect("stale lock fixture");
    drop(
        FileLock::try_acquire(&lock_path, StorageError::MigrationInProgress)
            .expect("OS lock ignores stale file contents"),
    );

    let staging = temporary.path().join(".migrating-7-interrupted");
    fs::create_dir(&staging).expect("staging fixture");
    let report = store.migrate_legacy().expect("migration recovers");
    assert!(!staging.exists());
    assert_eq!(report.issues.len(), 1);

    let tombstone = temporary.path().join(".deleting-1-stale");
    fs::create_dir(&tombstone).expect("tombstone fixture");
    fs::write(tombstone.join(METADATA_FILE), b"not a session").expect("tombstone content");
    assert!(store.sessions(None).expect("listing").is_empty());
    assert_eq!(
        store
            .recover_interrupted_deletes()
            .expect("delete recovers"),
        1
    );
    assert!(!tombstone.exists());
    assert_eq!(
        store_in(&temporary.path().join("missing"))
            .recover_interrupted_deletes()
            .expect("missing root"),
        0
    );
}

#[test]
fn handoff_publishes_complete_hydration_before_the_pointer_switch() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let mut parent = store
        .create("parent", WORKSPACE, None, 1)
        .expect("parent session");
    parent
        .config
        .insert("model".to_owned(), Value::from("parent"));
    parent.agent_profile = Some(json!({"name": "reviewer"}));
    parent.tools_available = vec![json!({"name": "read_file"})];
    store
        .update_metadata(&parent)
        .expect("parent hydration metadata");

    let child = store
        .handoff_messages(
            &parent,
            "child",
            &[system("system"), user("complete")],
            2_000,
            true,
        )
        .expect("handoff");
    assert_eq!(child.id, "child");
    let hydrated = store.load("child").expect("published child loads");
    assert_eq!(hydrated.messages, [user("complete")]);
    assert_eq!(hydrated.metadata.config["model"], "parent");
    assert_eq!(hydrated.metadata.agent_profile, parent.agent_profile);
    assert_eq!(hydrated.metadata.tools_available, parent.tools_available);
    assert_eq!(
        hydrated.metadata.parent_session_id.as_deref(),
        Some("parent")
    );
    assert_eq!(
        hydrated.metadata.system_prompt,
        Some(json!({"role": "system", "content": "system"}))
    );
    assert_eq!(store.pointer().as_deref(), Some("child"));
    assert!(
        root_entries(temporary.path())
            .iter()
            .all(|name| !name.starts_with(".handoff-"))
    );
    assert!(
        root_entries(&temporary.path().join(LAST_SESSION_DIRECTORY))
            .iter()
            .all(|name| !name.starts_with(HANDOFF_JOURNAL_PREFIX))
    );

    // A clearing records no parent: what it continues was discarded.
    let cleared = store
        .handoff_messages(&parent, "cleared", &[user("fresh")], 3_000, false)
        .expect("clearing handoff");
    assert_eq!(cleared.parent_session_id, None);
    assert_eq!(store.pointer().as_deref(), Some("cleared"));
}

/// Writes a complete session into a handoff staging directory and the
/// journal naming it: where a crash after the journal leaves a handoff.
fn stage_handoff(
    store: &SessionStore,
    id: &str,
    message: &ModelMessage,
) -> (PathBuf, PathBuf, PathBuf) {
    store
        .record_pointer("parent")
        .expect("pointer names the parent");
    let staging_directory = format!(".handoff-crash-{id}");
    let destination_directory = session_directory_name(store.prefix(), 2_000, id);
    let mut staged = store.compose_session(id, WORKSPACE, Some("parent".to_owned()), 2_000);
    staged.directory.clone_from(&staging_directory);
    store
        .replace_messages(&mut staged, std::slice::from_ref(message), 2_000)
        .expect("complete staged transcript");
    let journal_path = store
        .handoff_journal_path()
        .expect("pointer key names a journal");
    store
        .write_handoff_journal(
            &journal_path,
            &HandoffJournal {
                session_id: id.to_owned(),
                staging_directory: staging_directory.clone(),
                destination_directory: destination_directory.clone(),
            },
        )
        .expect("durable handoff intent");
    (
        store.root().join(staging_directory),
        store.root().join(destination_directory),
        journal_path,
    )
}

#[test]
fn handoff_journal_rolls_forward_after_publication_before_the_pointer_switch() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let parent = store
        .create("parent", WORKSPACE, None, 1)
        .expect("parent session");
    let (staging, destination, journal_path) = stage_handoff(&store, "recovered", &user("durable"));
    fs::rename(&staging, &destination).expect("published before the simulated crash");
    assert_eq!(store.pointer().as_deref(), Some("parent"));

    let recovered = store
        .handoff_messages(&parent, "recovered", &[user("replacement")], 2_000, true)
        .expect("the same handoff retried rolls forward");
    assert_eq!(recovered.id, "recovered");
    assert_eq!(
        store.load("recovered").expect("recovered child").messages,
        [user("durable")]
    );
    assert_eq!(store.pointer().as_deref(), Some("recovered"));
    assert!(!journal_path.exists());
}

#[test]
fn continue_rolls_forward_a_journal_whose_staging_directory_was_never_renamed() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    let (staging, destination, journal_path) = stage_handoff(&store, "recovered", &user("durable"));

    let continued = store
        .continue_session(WORKSPACE, "system", BTreeMap::new())
        .expect("continue recovers the handoff");
    assert_eq!(continued.metadata.id, "recovered");
    assert_eq!(continued.messages, [system("system"), user("durable")]);
    assert!(!staging.exists());
    assert!(destination.join(MESSAGES_FILE).is_file());
    assert!(!journal_path.exists());
    assert_eq!(store.pointer().as_deref(), Some("recovered"));
}

#[test]
fn a_handoff_journal_escaping_the_save_directory_is_refused() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = store_in(temporary.path());
    store.record_pointer("parent").expect("pointer directory");
    let journal_path = store.handoff_journal_path().expect("journal path");
    store
        .write_handoff_journal(
            &journal_path,
            &HandoffJournal {
                session_id: "evil".to_owned(),
                staging_directory: "../outside".to_owned(),
                destination_directory: "session_19700101_000000_evil".to_owned(),
            },
        )
        .expect("journal fixture");

    assert!(matches!(
        store.continue_session(WORKSPACE, "system", BTreeMap::new()),
        Err(StorageError::InvalidHandoffJournal(_))
    ));
    assert!(journal_path.is_file());
}

#[test]
fn child_creation_rejects_an_id_left_by_a_previous_process() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let first_process = store_in(temporary.path());
    let mut child = first_process
        .create_child("child-restart", WORKSPACE, "parent".to_owned(), 1_000)
        .expect("first process child");
    assert!(root_entries(temporary.path()).is_empty());
    first_process
        .append_message(&mut child, &assistant("done"), 1_001)
        .expect("child persists");

    let restarted_process = store_in(temporary.path());
    assert!(matches!(
        restarted_process.create_child(
            "child-restart",
            WORKSPACE,
            "parent".to_owned(),
            2_000,
        ),
        Err(StorageError::DuplicateSessionId(id)) if id == "child-restart"
    ));
    assert_eq!(
        restarted_process
            .sessions(None)
            .expect("unambiguous sessions")
            .len(),
        1
    );
}
