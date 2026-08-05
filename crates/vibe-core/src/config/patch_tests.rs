//! US-066: addressing configuration values by JSON Pointer, and the write path
//! US-067 routes through it.

use toml::{Table, Value};

use super::patch::{
    ConfigMutation, JsonPointer, PatchError, PatchOperation, apply_all, escape_token,
    resolve_upsert,
};
use super::registry::default_document;
use super::*;

fn table(toml: &str) -> Table {
    toml.parse::<Table>().expect("fixture parses")
}

/// A store over the shipped defaults, with `user` written to the selected file.
fn store(user: &str) -> (tempfile::TempDir, LayeredConfig, PathBuf) {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    let user_path = home.join(CONFIG_FILE);
    if !user.is_empty() {
        fs::write(&user_path, user).expect("user fixture");
    }
    let config = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: temporary.path().join("project"),
        },
        default_document(),
    );
    (temporary, config, user_path)
}

fn set(path: &str, value: Value) -> ConfigMutation {
    ConfigMutation::new(pointer(path), PatchOperation::Set(value))
}

fn remove(path: &str) -> ConfigMutation {
    ConfigMutation::new(pointer(path), PatchOperation::Remove)
}

fn pointer(path: &str) -> JsonPointer {
    JsonPointer::parse(path).expect("pointer parses")
}

#[test]
fn escapes_are_decoded_in_the_order_rfc_6901_requires() {
    // `~1` becomes `/` before `~0` becomes `~`, so `~01` addresses the literal
    // key `m~1` rather than a second slash.
    let document = table("[models]\n\"a/b\" = {}\n\"a~b\" = {}\n\"m~1\" = {}\n");
    for (spelling, key) in [("a~1b", "a/b"), ("a~0b", "a~b"), ("m~01", "m~1")] {
        let patched = apply_all(
            &document,
            &[set(&format!("/models/{spelling}/temperature"), 0.9.into())],
        )
        .expect("the escaped key resolves");
        assert_eq!(
            patched["models"][key]["temperature"],
            Value::Float(0.9_f64),
            "`{spelling}` did not address `{key}`"
        );
    }

    assert_eq!(
        JsonPointer::from_segments(["models", "gpt/4~turbo"]).as_str(),
        "/models/gpt~14~0turbo"
    );
    assert_eq!(escape_token("a/b"), "a~1b");
    assert_eq!(escape_token("a~b"), "a~0b");

    assert_eq!(
        JsonPointer::parse("models/active"),
        Err(PatchError::InvalidPointer {
            pointer: "models/active".to_owned()
        })
    );
    // The empty pointer is legal and addresses the whole document, which no
    // configuration operation may replace.
    assert_eq!(
        apply_all(&Table::new(), &[set("", "local".into())]),
        Err(PatchError::WholeDocument)
    );
}

#[test]
fn a_set_creates_the_tables_its_leaf_needs() {
    let document = table("theme = \"nord\"\n");
    let patched = apply_all(
        &document,
        &[set(
            "/tools/bash/allowlist",
            Value::Array(vec!["git status".into()]),
        )],
    )
    .expect("parents are created");

    assert_eq!(
        patched["tools"]["bash"]["allowlist"],
        Value::Array(vec!["git status".into()])
    );
    assert_eq!(patched["theme"], Value::String("nord".to_owned()));
    // The caller's document is never touched.
    assert!(!document.contains_key("tools"));
}

#[test]
fn a_parent_that_holds_no_fields_fails_without_mutating() {
    let document = table("tools = \"all\"\nkept = true\n");
    assert_eq!(
        apply_all(&document, &[set("/tools/bash/allowlist", true.into())]),
        Err(PatchError::NonTableParent {
            pointer: "/tools/bash/allowlist".to_owned()
        })
    );
    assert_eq!(document["tools"], Value::String("all".to_owned()));

    // A later operation failing discards the earlier ones too, because the
    // whole batch is applied to a copy.
    let attempted = apply_all(
        &document,
        &[
            set("/theme", "nord".into()),
            set("/tools/bash/allowlist", true.into()),
        ],
    );
    assert!(attempted.is_err());
    assert!(!document.contains_key("theme"));
}

#[test]
fn list_positions_append_insert_and_stay_inside_the_list() {
    let document = table("names = [\"one\", \"three\"]\n");

    let appended =
        apply_all(&document, &[set("/names/-", "four".into())]).expect("append resolves");
    assert_eq!(
        appended["names"],
        Value::Array(vec!["one".into(), "three".into(), "four".into()])
    );

    let inserted = apply_all(&document, &[set("/names/1", "two".into())]).expect("insert resolves");
    assert_eq!(
        inserted["names"],
        Value::Array(vec!["one".into(), "two".into(), "three".into()])
    );

    // One position past the end is the append RFC 6902 allows; anything beyond
    // it names a position that does not exist.
    assert!(apply_all(&document, &[set("/names/2", "three".into())]).is_ok());
    assert_eq!(
        apply_all(&document, &[set("/names/9", "nine".into())]),
        Err(PatchError::IndexOutOfRange {
            pointer: "/names/9".to_owned()
        })
    );
    assert_eq!(
        apply_all(&document, &[remove("/names/9")]),
        Err(PatchError::IndexOutOfRange {
            pointer: "/names/9".to_owned()
        })
    );
    assert_eq!(
        apply_all(&document, &[remove("/names/last")]),
        Err(PatchError::ArrayIndex {
            pointer: "/names/last".to_owned()
        })
    );
    assert_eq!(
        apply_all(&document, &[remove("/names/-")]),
        Err(PatchError::MissingPath {
            pointer: "/names/-".to_owned()
        })
    );
}

#[test]
fn a_remove_on_an_absent_path_fails_and_changes_nothing() {
    let document = table("theme = \"nord\"\n[tools.bash]\nallowlist = [\"ls\"]\n");

    assert_eq!(
        apply_all(&document, &[remove("/missing")]),
        Err(PatchError::MissingPath {
            pointer: "/missing".to_owned()
        })
    );
    assert_eq!(
        apply_all(&document, &[remove("/tools/bash/denylist")]),
        Err(PatchError::MissingPath {
            pointer: "/tools/bash/denylist".to_owned()
        })
    );
    assert_eq!(
        apply_all(&document, &[remove("/tools/python/allowlist")]),
        Err(PatchError::MissingPath {
            pointer: "/tools/python/allowlist".to_owned()
        })
    );
    assert_eq!(document["theme"], Value::String("nord".to_owned()));

    let removed = apply_all(&document, &[remove("/tools/bash/allowlist")]).expect("removes");
    assert!(
        !removed["tools"]["bash"]
            .as_table()
            .expect("table")
            .contains_key("allowlist")
    );
}

#[test]
fn an_upsert_replaces_a_matching_entry_and_appends_anything_else() {
    let servers = pointer("/mcp_servers");
    let docs = Table::from_iter([
        ("name".to_owned(), Value::from("docs")),
        ("transport".to_owned(), Value::from("stdio")),
    ]);

    let existing = Value::Array(vec![
        Value::Table(Table::from_iter([(
            "name".to_owned(),
            Value::from("other"),
        )])),
        Value::Table(Table::from_iter([
            ("name".to_owned(), Value::from("docs")),
            ("transport".to_owned(), Value::from("streamable-http")),
        ])),
    ]);
    assert_eq!(
        resolve_upsert(Some(&existing), &servers, "name", docs.clone()),
        ConfigMutation::new(
            pointer("/mcp_servers/1"),
            PatchOperation::Replace(Value::Table(docs.clone()))
        )
    );

    let others = Value::Array(vec![Value::Table(Table::from_iter([(
        "name".to_owned(),
        Value::from("other"),
    )]))]);
    assert_eq!(
        resolve_upsert(Some(&others), &servers, "name", docs.clone()),
        ConfigMutation::new(
            pointer("/mcp_servers/-"),
            PatchOperation::Set(Value::Table(docs.clone()))
        )
    );

    // An absent, empty or non-list target is created holding this entry alone.
    let created = ConfigMutation::new(
        servers.clone(),
        PatchOperation::Set(Value::Array(vec![Value::Table(docs.clone())])),
    );
    assert_eq!(
        resolve_upsert(None, &servers, "name", docs.clone()),
        created
    );
    assert_eq!(
        resolve_upsert(
            Some(&Value::Array(Vec::new())),
            &servers,
            "name",
            docs.clone()
        ),
        created
    );
    assert_eq!(
        resolve_upsert(Some(&Value::Boolean(true)), &servers, "name", docs.clone()),
        created
    );

    // The resolved operation is applied by the same core that resolved it.
    let document = table("[[mcp_servers]]\nname = \"other\"\n");
    let patched = apply_all(
        &document,
        &[resolve_upsert(
            document.get("mcp_servers"),
            &servers,
            "name",
            docs,
        )],
    )
    .expect("upsert applies");
    assert_eq!(
        patched["mcp_servers"]
            .as_array()
            .map(|entries| entries.len()),
        Some(2)
    );
    assert_eq!(patched["mcp_servers"][1]["name"], Value::from("docs"));
}

/// A patch pins each target's fingerprint from the snapshot it opened with, and
/// the write is refused when the file no longer matches. The window between the
/// two is internal, so the refusal is driven here with the exact write
/// [`LayeredConfig::apply_patch`] composes.
#[test]
fn a_target_that_changed_underneath_the_patch_is_refused_rather_than_overwritten() {
    let (_temporary, config, user_path) = store("theme = \"nord\"\n");
    let before = config.load().expect("the snapshot a patch would pin");

    let external = "theme = \"dracula\"\n";
    fs::write(&user_path, external).expect("an external client rewrites the file");

    let error = config
        .batch_write(&[ConfigWrite {
            target: ConfigTarget::User,
            expected_fingerprint: before
                .fingerprints
                .get(&ConfigTarget::User)
                .cloned()
                .flatten(),
            mutations: vec![ConfigMutation::set(
                ["theme"],
                Value::String("gruvbox".to_owned()),
            )],
        }])
        .expect_err("the stale write is refused");

    assert!(
        matches!(
            error,
            ConfigError::ConcurrentEdit {
                target: ConfigTarget::User
            }
        ),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(&user_path).expect("the file survives"),
        external,
        "the refused write overwrote the external edit"
    );
}

/// A pointer addresses a model through the alias-keyed map the merged document
/// exposes, and the file keeps the `[[models]]` list the reference reads.
#[test]
fn a_model_is_patched_through_its_alias_and_written_back_as_a_list() {
    let (_temporary, config, user_path) = store(
        "[[models]]\nname = \"devstral\"\nprovider = \"llamacpp\"\nalias = \"local\"\ntemperature = 0.2\n",
    );

    let outcome = config
        .apply_patch(
            &[ConfigPatchOp {
                mutation: ConfigMutation::set(
                    ["models", "local", "temperature"],
                    Value::Float(0.7),
                ),
                target: None,
            }],
            "settings screen",
        )
        .expect("the patch applies");

    assert!(outcome.failures.is_empty());
    assert_eq!(
        outcome.snapshot.effective["models"]["local"]["temperature"].as_float(),
        Some(0.7)
    );
    let persisted = fs::read_to_string(&user_path).expect("the file was written");
    assert!(
        persisted.contains("[[models]]"),
        "the persisted form stopped being a list: {persisted}"
    );
    assert!(persisted.contains("temperature = 0.7"), "{persisted}");
    // The entry the operator never touched is still the one the defaults ship.
    assert_eq!(
        outcome.snapshot.effective["models"]["devstral-small"]["provider"].as_str(),
        Some("mistral")
    );
}

/// Patching addresses models through a sorted map, and the file keeps the order
/// the operator wrote. The reference walks a dictionary that preserves file
/// order, and `model_order` reads the persisted list to pick the fallback an
/// unknown `active_model` resolves to, so re-sorting on write would move it.
#[test]
fn a_write_keeps_the_model_order_the_file_already_had() {
    let (_temporary, config, user_path) = store(
        "[[models]]\nname = \"zeta\"\nprovider = \"llamacpp\"\n\n[[models]]\nname = \"alpha\"\nprovider = \"llamacpp\"\n",
    );
    let aliases = |persisted: &str| {
        persisted
            .parse::<Table>()
            .expect("the persisted document parses")
            .get("models")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.get("alias")?.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .expect("models stayed a list")
    };

    // A write that never mentions models leaves their order alone.
    config
        .apply_patch(
            &[ConfigPatchOp {
                mutation: ConfigMutation::set(["theme"], Value::String("nord".to_owned())),
                target: None,
            }],
            "settings screen",
        )
        .expect("the unrelated patch applies");
    assert_eq!(
        aliases(&fs::read_to_string(&user_path).expect("the file was written")),
        ["zeta", "alpha"]
    );

    // A model added by pointer lands after the entries already on file.
    config
        .apply_patch(
            &[ConfigPatchOp {
                mutation: ConfigMutation::set(
                    ["models", "beta"],
                    Value::Table(Table::from_iter([(
                        "provider".to_owned(),
                        Value::from("llamacpp"),
                    )])),
                ),
                target: None,
            }],
            "settings screen",
        )
        .expect("the model patch applies");
    assert_eq!(
        aliases(&fs::read_to_string(&user_path).expect("the file was written")),
        ["zeta", "alpha", "beta"]
    );
}
