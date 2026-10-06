//! What the eval cache reads, what it refuses and the file it leaves behind.
//!
//! The `evalCache` family of
//! [`experiments_parity_tests`](crate::experiments_parity_tests) holds the
//! layout against the reference's own writes; this module covers the edges a
//! corpus case states less directly.

use std::fs;

use toml::Table;

use crate::config::registry::default_document;
use crate::config::{ConfigPaths, LayeredConfig};

use super::cache::{EVAL_CACHE_FILE_NAME, EVAL_CACHE_TTL, EvalCache};
use super::manager::hash_api_key;
use super::models::EvalResponse;

const ORACLE_VARIABLE: &str = "ORACLE_MISTRAL_KEY";
const ORACLE_KEY: &str = "oracle-mistral-sentinel";
const NOW: i64 = 1_800_000_000;

const MISTRAL_PROVIDER: &str = r#"
[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
"#;

const RESPONSE: &str = r#"{"features": {"vibe_cli_system_prompt": {"defaultValue": "cli",
    "rules": [{"force": "tests", "tracks": [{"experiment": {"key": "vibe_cli_system_prompt"},
    "result": {"key": "1", "variationId": 1, "inExperiment": true}}]}]}}}"#;

fn credentials(name: &str) -> Option<String> {
    (name == ORACLE_VARIABLE).then(|| ORACLE_KEY.to_owned())
}

fn effective(prefix: &str) -> Table {
    let temporary = tempfile::tempdir().expect("a scratch directory");
    let home = temporary.path().join("home/.vibe");
    let working = temporary.path().join("project");
    fs::create_dir_all(&home).expect("the home directory");
    fs::create_dir_all(&working).expect("the project directory");
    fs::write(
        home.join("config.toml"),
        format!("{prefix}{MISTRAL_PROVIDER}"),
    )
    .expect("the document writes");
    LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: working,
        },
        default_document(),
    )
    .load()
    .expect("the document composes")
    .effective
}

fn response() -> EvalResponse {
    serde_json::from_str(RESPONSE).expect("the response parses")
}

#[test]
fn a_stored_response_loads_back_while_it_is_recent() {
    let home = tempfile::tempdir().expect("a vibe home");
    let cache = EvalCache::new(home.path());
    let effective = effective("");
    assert_eq!(cache.load_at(&effective, &credentials, NOW), None);

    cache.store_at(&effective, &credentials, &response(), NOW);
    assert_eq!(
        cache.path(),
        home.path().join(EVAL_CACHE_FILE_NAME),
        "the file lives at the reference path under the vibe home"
    );
    assert_eq!(
        cache.load_at(&effective, &credentials, NOW),
        Some(response())
    );

    let ttl = i64::try_from(EVAL_CACHE_TTL.as_secs()).expect("the TTL fits");
    assert_eq!(
        cache.load_at(&effective, &credentials, NOW + ttl - 1),
        Some(response()),
        "one second before the bound the entry still applies"
    );
    assert_eq!(
        cache.load_at(&effective, &credentials, NOW + ttl),
        None,
        "an entry exactly as old as the bound is stale"
    );
}

#[test]
fn the_file_is_compact_keyed_by_the_digest_and_dumps_every_field() {
    let home = tempfile::tempdir().expect("a vibe home");
    let cache = EvalCache::new(home.path());
    cache.store_at(&effective(""), &credentials, &response(), NOW);

    let written = fs::read_to_string(cache.path()).expect("the cache is written");
    let key = hash_api_key(ORACLE_KEY);
    assert_eq!(
        written,
        format!(
            concat!(
                r#"{{"{key}":{{"stored_at_timestamp":{now},"payload":{{"features":{{"vibe_cli_system_prompt":"#,
                r#"{{"defaultValue":"cli","rules":[{{"force":"tests","tracks":[{{"experiment":{{"key":"vibe_cli_system_prompt"}},"#,
                r#""result":{{"key":"1","variationId":1,"value":null,"inExperiment":true,"hashAttribute":null,"#,
                r#""hashValue":null,"featureId":null}}}}]}}]}}}}}}}}}}"#
            ),
            key = key,
            now = NOW
        )
    );
    assert!(
        !written.contains(ORACLE_KEY),
        "the credential never reaches the file"
    );
    let staged = fs::read_dir(home.path())
        .expect("the home lists")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .count();
    assert_eq!(staged, 0, "the staging file is moved into place");
}

#[test]
fn a_store_keeps_every_other_entry_in_place() {
    let home = tempfile::tempdir().expect("a vibe home");
    let cache = EvalCache::new(home.path());
    let key = hash_api_key(ORACLE_KEY);
    fs::write(
        cache.path(),
        format!(
            r#"{{"other": {{"stored_at_timestamp": 1, "payload": {{"features": {{}}}}}}, "{key}": {{"stored_at_timestamp": 2, "payload": {{}}}}, "last": "kept as written"}}"#
        ),
    )
    .expect("the seed writes");

    cache.store_at(&effective(""), &credentials, &response(), NOW);
    let written = fs::read_to_string(cache.path()).expect("the cache is rewritten");
    let other = written.find("\"other\"").expect("the first entry is kept");
    let replaced = written.find(&key).expect("the entry is replaced in place");
    let last = written.find("\"last\"").expect("the last entry is kept");
    assert!(other < replaced && replaced < last);
    assert!(written.contains(r#""last":"kept as written""#));
    assert!(written.contains(&format!(r#""stored_at_timestamp":{NOW}"#)));
}

#[test]
fn a_configuration_that_may_not_resolve_a_rollout_neither_reads_nor_writes() {
    for prefix in [
        "enable_telemetry = false\n",
        "[experiments]\nenable = false\n",
    ] {
        let home = tempfile::tempdir().expect("a vibe home");
        let cache = EvalCache::new(home.path());
        let gated = effective(prefix);
        cache.store_at(&gated, &credentials, &response(), NOW);
        assert!(!cache.path().exists(), "{prefix:?} wrote the cache");

        cache.store_at(&effective(""), &credentials, &response(), NOW);
        assert_eq!(
            cache.load_at(&gated, &credentials, NOW),
            None,
            "{prefix:?} read the cache"
        );
    }

    let home = tempfile::tempdir().expect("a vibe home");
    let cache = EvalCache::new(home.path());
    cache.store_at(&effective(""), &|_: &str| None, &response(), NOW);
    assert!(!cache.path().exists(), "no credential, no key, no write");
}

#[test]
fn an_entry_that_does_not_read_is_not_applied() {
    let key = hash_api_key(ORACLE_KEY);
    for document in [
        "not json".to_owned(),
        "[]".to_owned(),
        format!(r#"{{"{key}": "text"}}"#),
        format!(r#"{{"{key}": {{"stored_at_timestamp": "{NOW}", "payload": {{}}}}}}"#),
        format!(r#"{{"{key}": {{"stored_at_timestamp": {NOW}.0, "payload": {{}}}}}}"#),
        format!(r#"{{"{key}": {{"stored_at_timestamp": {NOW}, "payload": []}}}}"#),
        format!(r#"{{"{key}": {{"stored_at_timestamp": {NOW}, "payload": {{"features": 3}}}}}}"#),
        format!(r#"{{"{key}": {{"payload": {{}}}}}}"#),
        format!(r#"{{"{key}": {{"stored_at_timestamp": true, "payload": {{}}}}}}"#),
    ] {
        let home = tempfile::tempdir().expect("a vibe home");
        let cache = EvalCache::new(home.path());
        fs::write(cache.path(), &document).expect("the seed writes");
        assert_eq!(
            cache.load_at(&effective(""), &credentials, NOW),
            None,
            "{document}"
        );
    }

    let home = tempfile::tempdir().expect("a vibe home");
    let cache = EvalCache::new(home.path());
    fs::write(
        cache.path(),
        format!(r#"{{"{key}": {{"stored_at_timestamp": {NOW}, "payload": {{}}}}}}"#),
    )
    .expect("the seed writes");
    assert_eq!(
        cache.load_at(&effective(""), &credentials, NOW),
        Some(EvalResponse::default()),
        "an empty payload is a response with no feature"
    );
}

/// A file that is not UTF-8 reads as no entry and is replaced by the next
/// store. The reference raises `UnicodeDecodeError` out of `_read_entries`
/// here, which its session build does not catch; the ledger in
/// `docs/parity.md` keeps this answer on purpose.
#[test]
fn a_file_that_is_not_utf8_reads_as_empty_and_is_replaced() {
    let home = tempfile::tempdir().expect("a vibe home");
    let cache = EvalCache::new(home.path());
    fs::write(cache.path(), b"\xff\xfe{}").expect("the seed writes");
    let effective = effective("");
    assert_eq!(cache.load_at(&effective, &credentials, NOW), None);

    cache.store_at(&effective, &credentials, &response(), NOW);
    assert_eq!(
        cache.load_at(&effective, &credentials, NOW),
        Some(response()),
        "the store writes a readable file over the unreadable one"
    );
}
