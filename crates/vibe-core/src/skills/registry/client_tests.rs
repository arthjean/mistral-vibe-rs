//! Differential tests for the catalog client, mirroring the behaviors
//! `tests/skills/registry/test_client.py` asserts upstream: the field mask,
//! the pagination and its cap, the version selector, the status taxonomy and
//! the malformed-payload handling, all observed through a recording
//! transport rather than a live service.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::json;

use super::client::{RegistrySkillsClient, RegistryTransport, TransportResponse};

/// One recorded request: the URL and its query parameters.
type RecordedCall = (String, Vec<(String, String)>);

/// A transport that records every request and answers from a queue.
#[derive(Default)]
struct FakeTransport {
    calls: Mutex<Vec<RecordedCall>>,
    responses: Mutex<VecDeque<Result<TransportResponse, String>>>,
}

impl FakeTransport {
    fn respond(self, response: Result<TransportResponse, String>) -> Self {
        self.responses
            .lock()
            .expect("the response queue is available")
            .push_back(response);
        self
    }

    fn respond_json(self, status: u16, body: serde_json::Value) -> Self {
        self.respond(Ok(TransportResponse {
            status,
            body: body.to_string().into_bytes(),
        }))
    }

    fn calls(&self) -> Vec<RecordedCall> {
        self.calls
            .lock()
            .expect("the call log is available")
            .clone()
    }
}

impl RegistryTransport for &FakeTransport {
    async fn get(
        &self,
        url: &str,
        params: &[(String, String)],
    ) -> Result<TransportResponse, String> {
        self.calls
            .lock()
            .expect("the call log is available")
            .push((url.to_owned(), params.to_vec()));
        self.responses
            .lock()
            .expect("the response queue is available")
            .pop_front()
            .unwrap_or_else(|| panic!("the transport was called more often than scripted"))
    }
}

fn client(transport: &FakeTransport) -> RegistrySkillsClient<&FakeTransport> {
    let mut client = RegistrySkillsClient::new("https://registry.example/v1/");
    client.open(transport);
    client
}

#[tokio::test]
async fn the_catalog_request_carries_the_field_mask_and_the_page_size() {
    let transport = FakeTransport::default().respond_json(200, json!({"data": []}));
    let items = client(&transport)
        .list_catalog(25)
        .await
        .expect("the listing succeeds");
    assert!(items.is_empty());
    let calls = transport.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "https://registry.example/v1/skills");
    assert_eq!(
        calls[0].1,
        [
            ("pageSize".to_owned(), "25".to_owned()),
            (
                "fields".to_owned(),
                "skillId,skill.skillName,skill.skillDescription,attributes,metadata,version"
                    .to_owned()
            ),
        ]
    );
}

#[tokio::test]
async fn the_listing_follows_the_page_token_and_accumulates() {
    let transport = FakeTransport::default()
        .respond_json(
            200,
            json!({"data": [{"skillId": "a"}], "nextPageToken": "t-2"}),
        )
        .respond_json(200, json!({"data": [{"skillId": "b"}]}));
    let items = client(&transport)
        .list_catalog(10)
        .await
        .expect("the listing succeeds");
    assert_eq!(
        items
            .iter()
            .map(|item| item.skill_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    let calls = transport.calls();
    assert_eq!(calls.len(), 2);
    assert!(!calls[0].1.iter().any(|(key, _)| key == "pageToken"));
    assert!(
        calls[1]
            .1
            .contains(&("pageToken".to_owned(), "t-2".to_owned()))
    );
}

#[tokio::test]
async fn a_never_terminating_chain_fails_at_the_page_cap() {
    let mut transport = FakeTransport::default();
    for page in 0..60 {
        transport = transport.respond_json(
            200,
            json!({"data": [], "nextPageToken": format!("t-{page}")}),
        );
    }
    let error = client(&transport)
        .list_catalog(10)
        .await
        .expect_err("the cap fails the listing");
    assert!(error.reason.contains("50-page cap"), "{}", error.reason);
    assert_eq!(transport.calls().len(), 50);
}

#[tokio::test]
async fn the_version_selector_is_version_xor_alias_xor_nothing() {
    let item = json!({"skillId": "s", "version": 2});
    let transport = FakeTransport::default()
        .respond_json(200, item.clone())
        .respond_json(200, item.clone())
        .respond_json(200, item);
    let skills = client(&transport);
    skills
        .get_skill("s", Some(2), Some("latest"))
        .await
        .expect("the fetch succeeds");
    skills
        .get_skill("s", None, Some("latest"))
        .await
        .expect("the fetch succeeds");
    skills
        .get_skill("s", None, None)
        .await
        .expect("the fetch succeeds");
    let calls = transport.calls();
    assert_eq!(calls[0].0, "https://registry.example/v1/skills/s");
    assert_eq!(calls[0].1, [("version".to_owned(), "2".to_owned())]);
    assert_eq!(calls[1].1, [("alias".to_owned(), "latest".to_owned())]);
    assert!(calls[2].1.is_empty());
}

#[tokio::test]
async fn versions_sort_newest_first_with_their_aliases() {
    let transport = FakeTransport::default().respond_json(
        200,
        json!({"items": [
            {"version": 1, "versionAttributes": {"aliases": ["stable"]}},
            {"version": 3},
            {"version": 2},
        ]}),
    );
    let versions = client(&transport)
        .list_versions("s")
        .await
        .expect("the listing succeeds");
    assert_eq!(
        versions.iter().map(|info| info.version).collect::<Vec<_>>(),
        [3, 2, 1]
    );
    assert_eq!(versions[2].aliases, ["stable"]);
    assert_eq!(
        transport.calls()[0].0,
        "https://registry.example/v1/skills/s/versions"
    );
}

#[tokio::test]
async fn the_status_taxonomy_names_each_failure() {
    for (status, expected) in [
        (401, "unauthorized (401)"),
        (403, "unauthorized (403)"),
        (404, "not found (404)"),
        (500, "unexpected status 500"),
    ] {
        let transport = FakeTransport::default().respond_json(status, json!({}));
        let error = client(&transport)
            .list_catalog(10)
            .await
            .expect_err("the status fails the request");
        assert_eq!(error.reason, expected);
    }
}

#[tokio::test]
async fn a_transport_failure_surfaces_its_reason() {
    let transport =
        FakeTransport::default().respond(Err("connection refused by the peer".to_owned()));
    let error = client(&transport)
        .list_catalog(10)
        .await
        .expect_err("the failure surfaces");
    assert!(
        error.reason.contains("connection refused by the peer"),
        "{}",
        error.reason
    );
}

#[tokio::test]
async fn a_malformed_success_body_names_the_payload_that_failed() {
    let not_json = FakeTransport::default().respond(Ok(TransportResponse {
        status: 200,
        body: b"<html>oops</html>".to_vec(),
    }));
    let error = client(&not_json)
        .list_catalog(10)
        .await
        .expect_err("non-JSON fails");
    assert!(error.reason.contains("not valid JSON"), "{}", error.reason);

    let wrong_shape = FakeTransport::default().respond_json(200, json!({"data": "not-a-list"}));
    let error = client(&wrong_shape)
        .list_catalog(10)
        .await
        .expect_err("the shape fails");
    assert!(error.reason.contains("catalog"), "{}", error.reason);

    let wrong_versions =
        FakeTransport::default().respond_json(200, json!({"items": [{"version": "x"}]}));
    let error = client(&wrong_versions)
        .list_versions("s")
        .await
        .expect_err("the shape fails");
    assert!(error.reason.contains("versions"), "{}", error.reason);
}

#[tokio::test]
async fn a_client_that_was_never_opened_errors_instead_of_building_a_transport() {
    let client: RegistrySkillsClient<&FakeTransport> =
        RegistrySkillsClient::new("https://registry.example");
    let error = client
        .list_catalog(10)
        .await
        .expect_err("the closed client refuses");
    assert!(
        error.reason.contains("before a transport was opened"),
        "{}",
        error.reason
    );
}
