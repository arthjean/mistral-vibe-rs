//! The identity read, the two failure classes it distinguishes, and the cache
//! that keeps a session to one round trip.

use std::sync::Arc;
use std::time::Duration;

use super::recorder::{Outcome, RecordingIdentityTransport};
use super::{
    HttpIdentityGateway, IDENTITY_PATH, IdentityCache, IdentityFailure, IdentityGateway,
    IdentityResult, UnavailableCause, fetch_identity,
};

const ORACLE_BASE: &str = "https://api.mistral.ai/v1";
const ORACLE_KEY: &str = "oracle-identity-sentinel";
const ORACLE_TIMEOUT: Duration = Duration::from_millis(4_000);

/// The payload every success branch is driven with. Authored here rather than
/// captured: `NOTICE` keeps reference-authored text out, and an identity is a
/// shape rather than a text.
const ORACLE_IDENTITY: &str = r#"{
    "id": "oracle-user",
    "email": "operator@example.test",
    "first_name": "Oracle",
    "last_name": "Operator",
    "workspace": {"id": "oracle-workspace", "name": "Workspace"},
    "organization": {"id": "oracle-organization", "name": "Oracle"}
}"#;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime builds")
}

fn gateway_over(outcome: Outcome) -> (HttpIdentityGateway, Arc<RecordingIdentityTransport>) {
    let transport = Arc::new(RecordingIdentityTransport::new(outcome, ORACLE_KEY));
    let gateway = HttpIdentityGateway::with_transport(Arc::clone(&transport) as _);
    (gateway, transport)
}

#[test]
fn the_read_is_a_bearer_get_on_users_me_with_the_configured_budget() {
    let (gateway, transport) = gateway_over(Outcome::ok(ORACLE_IDENTITY));
    let identity = runtime()
        .block_on(gateway.read(ORACLE_BASE, ORACLE_KEY, Some(ORACLE_TIMEOUT)))
        .expect("the authored identity validates");
    assert_eq!(identity.id, "oracle-user");
    assert_eq!(identity.organization_id(), Some("oracle-organization"));

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.url, format!("{ORACLE_BASE}{IDENTITY_PATH}"));
    assert_eq!(request.header_names, vec!["Authorization".to_owned()]);
    assert!(request.bearer_carries_key);
    assert_eq!(request.timeout, Some(ORACLE_TIMEOUT));
}

#[test]
fn one_trailing_slash_on_the_base_addresses_the_same_endpoint() {
    assert_eq!(
        HttpIdentityGateway::url("https://api.mistral.ai/v1/"),
        HttpIdentityGateway::url("https://api.mistral.ai/v1")
    );
}

#[test]
fn a_refusal_is_distinguished_from_an_unavailable_service() {
    for status in [401, 403] {
        let (gateway, _) = gateway_over(Outcome::status(status));
        assert_eq!(
            runtime().block_on(gateway.read(ORACLE_BASE, ORACLE_KEY, None)),
            Err(IdentityFailure::Unauthorized),
            "status {status} is a refused credential"
        );
    }
    for status in [404, 500, 503] {
        let (gateway, _) = gateway_over(Outcome::status(status));
        assert_eq!(
            runtime().block_on(gateway.read(ORACLE_BASE, ORACLE_KEY, None)),
            Err(IdentityFailure::Unavailable(UnavailableCause::Status(
                status
            ))),
            "status {status} is an unavailable service"
        );
    }
}

#[test]
fn every_failure_class_resolves_to_no_identity_and_propagates_nothing() {
    for outcome in [
        Outcome::Failure,
        Outcome::status(500),
        Outcome::ok("not json at all"),
        // JSON the models refuse: `id` is declared and missing.
        Outcome::ok(r#"{"email": "operator@example.test"}"#),
        // JSON the models refuse: `id` is declared as a string.
        Outcome::ok(r#"{"id": 7}"#),
    ] {
        let (gateway, transport) = gateway_over(outcome);
        assert_eq!(
            runtime().block_on(fetch_identity(&gateway, ORACLE_BASE, ORACLE_KEY, None)),
            None
        );
        assert_eq!(transport.request_count(), 1);
    }
}

#[test]
fn an_unknown_field_is_ignored_rather_than_refused() {
    let (gateway, _) = gateway_over(Outcome::ok(
        r#"{"id": "oracle-user", "tier": "enterprise", "organization": null}"#,
    ));
    let identity = runtime()
        .block_on(fetch_identity(&gateway, ORACLE_BASE, ORACLE_KEY, None))
        .expect("an unknown field does not refuse the payload");
    assert_eq!(identity.id, "oracle-user");
    assert_eq!(identity.organization_id(), None);
}

#[test]
fn two_concurrent_callers_share_one_request() {
    let transport = Arc::new(RecordingIdentityTransport::answering(
        ORACLE_IDENTITY,
        ORACLE_KEY,
    ));
    let gateway = HttpIdentityGateway::with_transport(Arc::clone(&transport) as _);
    let cache = IdentityCache::new();
    let (first, second) = runtime().block_on(async {
        tokio::join!(
            cache.resolve(&gateway, ORACLE_BASE, ORACLE_KEY, None),
            cache.resolve(&gateway, ORACLE_BASE, ORACLE_KEY, None)
        )
    });
    assert_eq!(first, second);
    assert!(first.is_some());
    assert_eq!(
        transport.request_count(),
        1,
        "the second caller reads the cache the first filled"
    );
}

#[test]
fn a_different_pair_is_a_different_entry() {
    let transport = Arc::new(RecordingIdentityTransport::answering(
        ORACLE_IDENTITY,
        ORACLE_KEY,
    ));
    let gateway = HttpIdentityGateway::with_transport(Arc::clone(&transport) as _);
    let cache = IdentityCache::new();
    runtime().block_on(async {
        cache.resolve(&gateway, ORACLE_BASE, ORACLE_KEY, None).await;
        cache
            .resolve(&gateway, "https://other.example.test/v1", ORACLE_KEY, None)
            .await;
        // The bearer no longer matches the transport's sentinel, which is only
        // a recording detail: what matters is that the pair is a new key.
        cache
            .resolve(&gateway, ORACLE_BASE, "another-sentinel", None)
            .await;
    });
    assert_eq!(transport.request_count(), 3);
}

#[test]
fn a_failure_is_not_cached_so_a_later_caller_retries() {
    let transport = Arc::new(RecordingIdentityTransport::new(
        Outcome::Failure,
        ORACLE_KEY,
    ));
    let gateway = HttpIdentityGateway::with_transport(Arc::clone(&transport) as _);
    let cache = IdentityCache::new();
    runtime().block_on(async {
        assert_eq!(
            cache.resolve(&gateway, ORACLE_BASE, ORACLE_KEY, None).await,
            None
        );
        assert_eq!(cache.peek(ORACLE_BASE, ORACLE_KEY).await, None);
        assert_eq!(
            cache.resolve(&gateway, ORACLE_BASE, ORACLE_KEY, None).await,
            None
        );
    });
    assert_eq!(transport.request_count(), 2);
}

#[test]
fn a_success_is_readable_without_fetching() {
    let transport = Arc::new(RecordingIdentityTransport::answering(
        ORACLE_IDENTITY,
        ORACLE_KEY,
    ));
    let gateway = HttpIdentityGateway::with_transport(Arc::clone(&transport) as _);
    let cache = IdentityCache::new();
    let peeked = runtime().block_on(async {
        cache
            .resolve(&gateway, ORACLE_BASE, ORACLE_KEY, Some(ORACLE_TIMEOUT))
            .await;
        cache.peek(ORACLE_BASE, ORACLE_KEY).await
    });
    assert_eq!(
        peeked.and_then(|identity| identity.organization_id().map(ToOwned::to_owned)),
        Some("oracle-organization".to_owned())
    );
    assert_eq!(transport.request_count(), 1);
}

#[test]
fn an_identity_without_an_organization_reports_none() {
    let identity: IdentityResult =
        serde_json::from_str(r#"{"id": "oracle-user"}"#).expect("the minimal payload validates");
    assert_eq!(identity.organization_id(), None);
    assert_eq!(identity.workspace, None);
}
