//! US-069: publishing configuration changes to subscribers.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::events::{ConfigChangeBus, ConfigChangeEvent, changed_keys};
use super::registry::default_document;
use super::*;

fn bus() -> Arc<ConfigChangeBus> {
    Arc::new(ConfigChangeBus::default())
}

fn event(keys: &[&str]) -> ConfigChangeEvent {
    ConfigChangeEvent {
        changed_keys: keys.iter().map(|key| (*key).to_owned()).collect(),
        before: json!({}),
        after: json!({}),
        reason: "test".to_owned(),
    }
}

fn filter(keys: &[&str]) -> Option<BTreeSet<String>> {
    Some(keys.iter().map(|key| (*key).to_owned()).collect())
}

fn recorder() -> (Arc<Mutex<Vec<String>>>, impl Fn(&ConfigChangeEvent)) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recording = Arc::clone(&seen);
    (seen, move |event: &ConfigChangeEvent| {
        if let Ok(mut seen) = recording.lock() {
            seen.extend(event.changed_keys.iter().cloned());
        }
    })
}

#[test]
fn a_key_filter_matches_ancestors_and_descendants_but_never_a_prefix() {
    let bus = bus();
    let (models, on_models) = recorder();
    let (singular, on_singular) = recorder();
    let (everything, on_everything) = recorder();
    // A subscription on a leaf hears about its parent being replaced whole, so
    // the rule is exercised in both directions.
    let (leaf, on_leaf) = recorder();
    bus.subscribe(filter(&["models"]), on_models);
    bus.subscribe(filter(&["model"]), on_singular);
    bus.subscribe(filter(&["models/active"]), on_leaf);
    bus.subscribe(None, on_everything);

    bus.publish(&event(&["models/active"]));
    bus.publish(&event(&["theme"]));

    assert_eq!(
        models.lock().expect("recorded").as_slice(),
        ["models/active"]
    );
    assert_eq!(leaf.lock().expect("recorded").as_slice(), ["models/active"]);
    assert!(singular.lock().expect("recorded").is_empty());
    assert_eq!(
        everything.lock().expect("recorded").as_slice(),
        ["models/active", "theme"]
    );
}

#[test]
fn an_unsubscribed_listener_stops_hearing_and_a_failing_one_spares_the_others() {
    let bus = bus();
    let (early, on_early) = recorder();
    let subscription = bus.subscribe(None, on_early);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    bus.subscribe(None, move |_| {
        counted.fetch_add(1, Ordering::Relaxed);
        // A subscriber that fails must not silence the ones behind it.
        unreachable!("subscriber failure is contained");
    });
    let (late, on_late) = recorder();
    bus.subscribe(None, on_late);

    subscription.unsubscribe();
    bus.publish(&event(&["theme"]));

    assert!(early.lock().expect("recorded").is_empty());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(late.lock().expect("recorded").as_slice(), ["theme"]);

    // Publishing again after the cancelled subscriber is gone is not an error.
    bus.publish(&event(&["theme"]));
    assert_eq!(late.lock().expect("recorded").len(), 2);
}

/// A real write is what publishes: the store composes the event, so a
/// subscriber sees the same document the next load would produce.
#[test]
fn a_write_publishes_the_documents_it_moved_between_and_the_reason_given() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    let config = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: temporary.path().join("project"),
        },
        default_document(),
    );
    let events: Arc<Mutex<Vec<ConfigChangeEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let recording = Arc::clone(&events);
    config.subscribe(filter(&["theme"]), move |event| {
        if let Ok(mut events) = recording.lock() {
            events.push(event.clone());
        }
    });
    let set = |value: &str| ConfigPatchOp {
        mutation: ConfigMutation::set(["theme"], Value::String(value.to_owned())),
        target: None,
    };

    let outcome = config
        .apply_patch(&[set("nord")], "settings screen")
        .expect("the patch applies");
    assert!(outcome.failures.is_empty());

    let published = events.lock().expect("recorded");
    let event = published.first().expect("one event was published");
    assert_eq!(event.changed_keys, BTreeSet::from(["theme".to_owned()]));
    assert_eq!(event.reason, "settings screen");
    assert_eq!(event.before["theme"], json!("auto"));
    assert_eq!(event.after["theme"], json!("nord"));
    assert_eq!(
        event.after["theme"],
        json!(outcome.snapshot.effective["theme"].as_str())
    );
    drop(published);

    // Writing the value already in place moves nothing, so nothing is
    // published.
    config
        .apply_patch(&[set("nord")], "settings screen")
        .expect("the repeat applies");
    assert_eq!(events.lock().expect("recorded").len(), 1);

    // A change to another field does not reach a subscription filtered on
    // `theme`.
    config
        .apply_patch(
            &[ConfigPatchOp {
                mutation: ConfigMutation::set(["default_agent"], Value::String("plan".to_owned())),
                target: None,
            }],
            "settings screen",
        )
        .expect("the unrelated patch applies");
    assert_eq!(events.lock().expect("recorded").len(), 1);
}

/// NFR: no event payload carries a value from a key the sensitive-key rule
/// covers.
#[test]
fn a_published_document_carries_no_secret() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    let config = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: temporary.path().join("project"),
        },
        default_document(),
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recording = Arc::clone(&seen);
    config.subscribe(None, move |event| {
        if let Ok(mut seen) = recording.lock() {
            seen.push(format!(
                "{}{}{}",
                event.before,
                event.after,
                event.changed_keys.iter().cloned().collect::<String>()
            ));
        }
    });
    let set = |value: &str| {
        vec![ConfigPatchOp {
            mutation: ConfigMutation::set(["proxy"], Value::String(value.to_owned())),
            target: None,
        }]
    };

    config
        .apply_patch(&set("https://proxy.example/s3cr3t"), "proxy setup")
        .expect("the patch applies");
    // A second secret is still a change, even though both read `[redacted]`
    // once the payload is built.
    config
        .apply_patch(&set("https://proxy.example/s3cr3t-two"), "proxy setup")
        .expect("the replacement applies");

    let published = seen.lock().expect("recorded");
    assert_eq!(published.len(), 2, "a secret rotation went unpublished");
    for payload in published.iter() {
        assert!(payload.contains("proxy"), "nothing was published");
        assert!(!payload.contains("s3cr3t"), "the event leaked a secret");
    }
}

#[test]
fn the_diff_names_the_deepest_node_two_documents_stop_agreeing_at() {
    let before = json!({
        "theme": "nord",
        "models": {"local": {"temperature": 0.2}, "gone": {}},
        "same": [1, 2],
    });
    let after = json!({
        "theme": "nord",
        "models": {"local": {"temperature": 0.9}, "added": {"name": "x"}},
        "same": [1, 2],
    });

    assert_eq!(
        changed_keys(&before, &after),
        BTreeSet::from([
            "models/local/temperature".to_owned(),
            "models/gone".to_owned(),
            "models/added".to_owned(),
        ])
    );
    assert!(changed_keys(&before, &before).is_empty());
    // A list is compared whole: the reference records the field, not the entry.
    assert_eq!(
        changed_keys(&json!({"names": [1]}), &json!({"names": [1, 2]})),
        BTreeSet::from(["names".to_owned()])
    );
}
