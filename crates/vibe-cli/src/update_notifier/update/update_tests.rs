use std::fs;

use super::*;
use crate::update_notifier::{UPDATE_CACHE_TTL_SECONDS, UpdateFetch, UpdateGatewayCause};

#[test]
fn fresh_cache_answers_without_contacting_the_gateway() {
    let cache = UpdateCache::new("2.24.0", 1_000);
    assert_eq!(
        plan_update_check(false, Some(&cache), "2.23.1", 1_100),
        CachePlan::Cached(Some(UpdateAvailability {
            latest_version: "2.24.0".to_owned(),
            should_notify: false,
        }))
    );
    assert_eq!(
        plan_update_check(false, Some(&cache), "2.24.0", 1_100),
        CachePlan::Cached(None)
    );
    assert_eq!(
        plan_update_check(true, Some(&cache), "2.23.1", 1_100),
        CachePlan::Fetch
    );
    assert_eq!(
        plan_update_check(
            false,
            Some(&cache),
            "2.23.1",
            1_000 + UPDATE_CACHE_TTL_SECONDS + 1
        ),
        CachePlan::Fetch
    );
    assert_eq!(
        plan_update_check(false, None, "not-a-version", 0),
        CachePlan::Unversioned
    );
}

#[test]
fn gateway_failure_refreshes_the_cache_without_a_new_version() {
    let previous = UpdateCache {
        latest_version: "2.24.0".to_owned(),
        stored_at_timestamp: 1,
        seen_whats_new_version: Some("2.23.1".to_owned()),
        dismissed_version: Some("2.24.0".to_owned()),
    };
    let resolution = resolve_fetch(
        Err(UpdateGatewayError::new(UpdateGatewayCause::RequestFailed)),
        Some(&previous),
        "2.23.1",
        99,
    );
    assert_eq!(
        resolution.error,
        Some(UpdateError::Gateway(
            "Network error while checking for updates.".to_owned()
        ))
    );
    assert_eq!(resolution.availability, None);
    let write = resolution.cache_write.expect("cache refresh");
    assert_eq!(write.latest_version, "2.23.1");
    assert_eq!(write.stored_at_timestamp, 99);
    assert_eq!(write.seen_whats_new_version.as_deref(), Some("2.23.1"));
    assert_eq!(write.dismissed_version.as_deref(), Some("2.24.0"));
}

#[test]
fn malformed_gateway_version_never_becomes_authoritative() {
    let resolution = resolve_fetch(Ok(Some("banana".to_owned())), None, "2.23.1", 5);
    assert_eq!(resolution.cache_write, None);
    assert_eq!(resolution.availability, None);
    assert_eq!(resolution.error, None);
}

#[test]
fn newer_gateway_version_notifies_and_persists() {
    let resolution = resolve_fetch(Ok(Some("2.24.0".to_owned())), None, "2.23.1", 5);
    assert_eq!(
        resolution.availability,
        Some(UpdateAvailability {
            latest_version: "2.24.0".to_owned(),
            should_notify: true,
        })
    );
    assert_eq!(resolution.cache_write, Some(UpdateCache::new("2.24.0", 5)));
}

#[test]
fn dismissal_and_release_notes_follow_the_cache() {
    let cache = UpdateCache::new("2.24.0", 10);
    assert_eq!(
        pending_update_from_cache(Some(&cache), "2.23.1").as_deref(),
        Some("2.24.0")
    );
    let dismissed = dismiss_update(Some(&cache), "2.24.0").expect("dismissal");
    assert_eq!(pending_update_from_cache(Some(&dismissed), "2.23.1"), None);
    assert!(should_show_whats_new(Some(&cache), "2.23.1"));
    let seen = mark_version_as_seen(Some(&cache), "2.23.1", 11);
    assert!(!should_show_whats_new(Some(&seen), "2.23.1"));
    assert_eq!(seen.latest_version, "2.24.0");
    // Without a cache the reference shows nothing and dismisses nothing.
    assert!(!should_show_whats_new(None, "2.23.1"));
    assert_eq!(dismiss_update(None, "2.24.0"), None);
}

struct StubGateway(Result<Option<String>, UpdateGatewayError>);

impl UpdateGateway for StubGateway {
    fn fetch_update(&self) -> UpdateFetch<'_> {
        let result = self.0.clone();
        Box::pin(async move { result })
    }
}

#[tokio::test]
async fn discovery_persists_exactly_what_the_reference_persists() {
    let directory = tempfile::tempdir().expect("temporary vibe home");
    let store = UpdateCacheStore::new(directory.path());
    let gateway = StubGateway(Ok(Some("2.24.0".to_owned())));
    let availability = get_update_if_available(&gateway, &store, "2.23.1", 7, false)
        .await
        .expect("discovery");
    assert_eq!(
        availability,
        Some(UpdateAvailability {
            latest_version: "2.24.0".to_owned(),
            should_notify: true,
        })
    );
    assert_eq!(store.load(), Some(UpdateCache::new("2.24.0", 7)));

    // A fresh cache answers the next non-forced check without notifying.
    let offline = StubGateway(Err(UpdateGatewayError::new(UpdateGatewayCause::Unknown)));
    let cached = get_update_if_available(&offline, &store, "2.23.1", 8, false)
        .await
        .expect("cached discovery");
    assert_eq!(
        cached,
        Some(UpdateAvailability {
            latest_version: "2.24.0".to_owned(),
            should_notify: false,
        })
    );

    let error = get_update_if_available(&offline, &store, "2.23.1", 9, true)
        .await
        .expect_err("forced discovery reaches the gateway");
    assert_eq!(
        error,
        UpdateError::Gateway("Unable to determine whether an update is available.".to_owned())
    );
}

/// The migration is reached from the same entry point the startup check and
/// `--check-upgrade` both call, and its values answer before the gateway is
/// contacted.
#[tokio::test]
async fn discovery_answers_from_a_migrated_legacy_cache_without_reaching_the_gateway() {
    let directory = tempfile::tempdir().expect("temporary vibe home");
    let store = UpdateCacheStore::new(directory.path());
    fs::write(
        store.legacy_path(),
        r#"{"latest_version": "2.24.0", "stored_at_timestamp": 100}"#,
    )
    .expect("legacy cache");
    let offline = StubGateway(Err(UpdateGatewayError::new(UpdateGatewayCause::Unknown)));
    let availability = get_update_if_available(&offline, &store, "2.23.1", 101, false)
        .await
        .expect("discovery");
    assert_eq!(
        availability,
        Some(UpdateAvailability {
            latest_version: "2.24.0".to_owned(),
            should_notify: false,
        })
    );
    assert!(
        fs::read_to_string(store.path())
            .expect("migrated cache")
            .contains("2.24.0"),
        "the migration wrote the section the next run reads"
    );
}
