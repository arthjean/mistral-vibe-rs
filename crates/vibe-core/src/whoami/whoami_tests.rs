use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;

use super::*;

fn account(plan_type: &str, plan_name: &str) -> WhoAmIResult {
    serde_json::from_value(json!({"plan_type": plan_type, "plan_name": plan_name}))
        .expect("the account parses")
}

#[test]
fn the_plan_label_follows_the_reference_mapping() {
    let cases = [
        (Some("chat"), Some("free"), Some("Free")),
        (Some("chat"), Some(" individual "), Some("Pro")),
        (Some("chat"), Some("EDU"), Some("Student")),
        (Some("chat"), Some("team"), Some("Team")),
        (Some("chat"), Some("enterprise"), None),
        (Some("mistral_code"), Some("f"), Some("Free Codestral")),
        (Some("mistral_code"), Some("E"), Some("Code Enterprise")),
        (Some("mistral_code"), Some("x"), None),
        (Some("api"), Some("FREE_TRIAL"), Some("Free API")),
        (Some("api"), Some("scale"), Some("PAYG API")),
        (Some("api"), Some("  "), None),
        (Some("api"), None, None),
        (Some("unknown"), Some("free"), None),
        (None, Some("free"), None),
        (Some(NO_PLAN_DATA), None, Some(NO_PLAN_DATA)),
        (None, Some(NO_PLAN_DATA), Some(NO_PLAN_DATA)),
    ];
    for (plan_type, plan_name, expected) in cases {
        assert_eq!(
            resolve_user_plan(plan_type, plan_name).as_deref(),
            expected,
            "{plan_type:?} {plan_name:?}"
        );
    }
}

#[test]
fn the_account_is_read_strictly_and_the_type_case_insensitively() {
    assert_eq!(account(" API ", "x").plan_type, AccountPlanKind::Api);
    for refused in [
        json!({"plan_type": "gold", "plan_name": "x"}),
        json!({"plan_type": 1, "plan_name": "x"}),
        json!({"plan_type": "api", "plan_name": 3}),
        json!({"plan_type": "api"}),
        json!({"plan_type": "api", "plan_name": "x", "prompt_switching_to_pro_plan": "yes"}),
    ] {
        assert!(
            serde_json::from_value::<WhoAmIResult>(refused.clone()).is_err(),
            "{refused}"
        );
    }
}

#[test]
fn the_disk_cache_round_trips_expires_and_clears() {
    let home = tempfile::tempdir().expect("a home");
    let path = whoami_cache_path(home.path());
    let result = account("api", "FREE_TRIAL");
    assert_eq!(load_cached_whoami(&path, "key"), None);
    store_cached_whoami(&path, "key", &result);
    assert_eq!(load_cached_whoami(&path, "key"), Some(result.clone()));
    assert_eq!(load_cached_whoami(&path, "other"), None);
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("the file")).expect("json");
    let entry = &stored[hash_api_key("key")];
    assert_eq!(
        entry["payload"],
        json!({
            "plan_type": "api",
            "plan_name": "FREE_TRIAL",
            "prompt_switching_to_pro_plan": false,
            "organization_kind": null,
            "customer_id": null,
            "api_base": null,
            "vibe_base": null,
        })
    );

    let mut stale = stored.clone();
    stale[hash_api_key("key")]["stored_at_timestamp"] =
        json!(now_seconds() - WHOAMI_CACHE_TTL_SECONDS);
    std::fs::write(&path, stale.to_string()).expect("written");
    assert_eq!(load_cached_whoami(&path, "key"), None);

    store_cached_whoami(&path, "key", &result);
    clear_cached_whoami(&path, "key");
    assert_eq!(load_cached_whoami(&path, "key"), None);
}

struct Counting {
    calls: AtomicUsize,
    answer: Result<WhoAmIResult, WhoAmIFailure>,
}

impl WhoAmIGateway for Counting {
    fn read<'a>(
        &'a self,
        _base_url: &'a str,
        _api_key: &'a str,
        _timeout: Option<Duration>,
    ) -> IdentityFuture<'a, Result<WhoAmIResult, WhoAmIFailure>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(self.answer.clone()))
    }
}

#[tokio::test]
async fn the_cache_reads_memory_then_disk_then_the_network_and_keeps_successes_only() {
    let home = tempfile::tempdir().expect("a home");
    let failing = Arc::new(Counting {
        calls: AtomicUsize::new(0),
        answer: Err(WhoAmIFailure::Unavailable),
    });
    let cache = WhoAmICache::new(home.path(), Some(failing.clone()));
    assert_eq!(cache.resolve("https://console", "key", None).await, None);
    assert_eq!(cache.resolve("https://console", "key", None).await, None);
    assert_eq!(
        failing.calls.load(Ordering::SeqCst),
        2,
        "failures are not cached"
    );

    let answering = Arc::new(Counting {
        calls: AtomicUsize::new(0),
        answer: Ok(account("chat", "individual")),
    });
    let cache = WhoAmICache::new(home.path(), Some(answering.clone()));
    let first = cache.resolve("https://console", "key", None).await;
    let second = cache.resolve("https://console", "key", None).await;
    assert_eq!(first, second);
    assert_eq!(answering.calls.load(Ordering::SeqCst), 1);

    // A later process reads the disk entry without a request.
    let later = Arc::new(Counting {
        calls: AtomicUsize::new(0),
        answer: Err(WhoAmIFailure::Unavailable),
    });
    let cache = WhoAmICache::new(home.path(), Some(later.clone()));
    assert_eq!(cache.resolve("https://console", "key", None).await, first);
    assert_eq!(later.calls.load(Ordering::SeqCst), 0);

    cache.invalidate("key").await;
    assert_eq!(cache.peek("https://console", "key").await, None);
    assert_eq!(cache.resolve("https://console", "key", None).await, None);
    assert_eq!(later.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn derive_user_plan_reads_the_account() {
    assert_eq!(derive_user_plan(None), None);
    assert_eq!(
        derive_user_plan(Some(&account("api", "FREE_TRIAL"))).as_deref(),
        Some("Free API")
    );
}
