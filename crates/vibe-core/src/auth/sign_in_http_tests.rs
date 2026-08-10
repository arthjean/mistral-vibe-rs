//! What the corpus cannot measure about the HTTP half: the provider entry the
//! bases resolve from, the configuration keys observably steering the
//! requests, and the redaction of every failure string. The wire behavior and
//! the 33 URL verdicts are replayed in `setup_auth_parity_tests`.

use std::future::Future;

use serde_json::json;
use toml::Table;

use super::sign_in::{SignInErrorCode, SignInGateway};
use super::sign_in_http::{
    DEFAULT_BROWSER_AUTH_API_BASE_URL, DEFAULT_BROWSER_AUTH_BASE_URL, HttpSignInGateway,
    browser_sign_in_bases,
};
use super::testing::ScriptedHttpClient;

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds")
        .block_on(future)
}

fn provider(entries: &[(&str, &str)]) -> Table {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), toml::Value::String((*value).to_owned())))
        .collect()
}

// --------------------------------------------------------------------------
// The bases a provider entry resolves to (US-187)
// --------------------------------------------------------------------------

#[test]
fn the_mistral_provider_falls_back_to_the_shipped_defaults() {
    let bases = browser_sign_in_bases(&provider(&[("name", "mistral"), ("backend", "mistral")]));
    assert_eq!(
        bases,
        Some((
            DEFAULT_BROWSER_AUTH_BASE_URL.to_owned(),
            DEFAULT_BROWSER_AUTH_API_BASE_URL.to_owned(),
        ))
    );
    let legacy_without_backend = browser_sign_in_bases(&provider(&[("name", "mistral")]));
    assert_eq!(bases, legacy_without_backend);
}

#[test]
fn explicit_urls_win_over_the_defaults() {
    let bases = browser_sign_in_bases(&provider(&[
        ("name", "mistral"),
        ("backend", "mistral"),
        ("browser_auth_base_url", "https://console.internal.example"),
        (
            "browser_auth_api_base_url",
            "https://console.internal.example/api",
        ),
    ]));
    assert_eq!(
        bases,
        Some((
            "https://console.internal.example".to_owned(),
            "https://console.internal.example/api".to_owned(),
        ))
    );
}

#[test]
fn an_explicitly_empty_url_disables_sign_in_rather_than_defaulting() {
    let bases = browser_sign_in_bases(&provider(&[
        ("name", "mistral"),
        ("backend", "mistral"),
        ("browser_auth_base_url", ""),
    ]));
    assert_eq!(bases, None);
}

#[test]
fn a_provider_without_the_keys_resolves_no_bases() {
    assert_eq!(
        browser_sign_in_bases(&provider(&[("name", "llamacpp"), ("backend", "generic")])),
        None
    );
    // An explicit generic backend on a mistral-named entry opts out of the
    // legacy defaults, as the reference's field-set check does.
    assert_eq!(
        browser_sign_in_bases(&provider(&[("name", "mistral"), ("backend", "generic")])),
        None
    );
}

/// The reference's `supports_browser_sign_in` gates on the backend or the
/// name before it looks at the URLs, so a third-party entry carrying both
/// keys still has no browser sign-in.
#[test]
fn a_non_mistral_entry_carrying_both_keys_still_resolves_no_bases() {
    assert_eq!(
        browser_sign_in_bases(&provider(&[
            ("name", "acme"),
            ("backend", "generic"),
            ("browser_auth_base_url", "https://console.acme.example"),
            (
                "browser_auth_api_base_url",
                "https://console.acme.example/api",
            ),
        ])),
        None
    );
    // The mistral backend under another name keeps it, as the reference's
    // first disjunct does.
    assert!(
        browser_sign_in_bases(&provider(&[
            ("name", "mistral-eu"),
            ("backend", "mistral"),
            ("browser_auth_base_url", "https://console.acme.example"),
            (
                "browser_auth_api_base_url",
                "https://console.acme.example/api",
            ),
        ]))
        .is_some()
    );
}

/// US-187: changing either browser-auth configuration key observably moves
/// the requests, proven end to end from the provider entry to the issued URL.
#[test]
fn a_changed_api_base_key_changes_the_request_host() {
    let entry = provider(&[
        ("name", "mistral"),
        ("backend", "mistral"),
        ("browser_auth_base_url", "https://console.internal.example"),
        (
            "browser_auth_api_base_url",
            "https://console.internal.example/api",
        ),
    ]);
    let (browser_base, api_base) =
        browser_sign_in_bases(&entry).expect("the entry resolves its bases");
    let client = ScriptedHttpClient::new(vec![json!({
        "body": {
            "process_id": "p",
            "sign_in_url": "https://console.internal.example/sign-in",
            "poll_url": "https://console.internal.example/api/poll",
            "expires_at": "2026-01-01T00:10:00Z",
        }
    })]);
    let mut gateway = HttpSignInGateway::new(&browser_base, &api_base, client);
    block_on(gateway.create_process("challenge")).expect("the scripted creation succeeds");
    assert_eq!(
        gateway.into_client().requests[0]
            .get("url")
            .and_then(serde_json::Value::as_str),
        Some("https://console.internal.example/api/vibe/sign-in"),
        "the request did not follow the configured key"
    );
}

// --------------------------------------------------------------------------
// Redaction (US-188, FR-16)
// --------------------------------------------------------------------------

/// Every failure a gateway operation can produce is printable verbatim: no
/// server URL, no exchange token, no verifier and no credential appears in
/// the error's display form.
#[test]
fn gateway_failures_carry_no_secret_and_no_url() {
    let secrets = [
        "https://console.mistral.ai",
        "oracle-token",
        "oracle-verifier",
        "oracle-key",
        "oracle-process",
    ];
    let failures = [
        // A transport failure on create.
        ("create", vec![json!({"transportError": true})]),
        // A rejected server-supplied URL on create.
        (
            "create",
            vec![json!({"body": {
                "process_id": "oracle-process",
                "sign_in_url": "https://evil.example/sign-in",
                "poll_url": "https://console.mistral.ai/api/poll",
                "expires_at": "2026-01-01T00:10:00Z",
            }})],
        ),
        // A poll URL rejected before any request.
        ("poll-evil", vec![]),
        // An exchange refused by the server.
        (
            "exchange",
            vec![json!({"status": 403, "body": {"detail": "denied"}})],
        ),
    ];
    for (operation, responses) in failures {
        let client = ScriptedHttpClient::new(responses);
        let mut gateway = HttpSignInGateway::new(
            "https://console.mistral.ai",
            "https://console.mistral.ai/api",
            client,
        );
        let error = block_on(async {
            match operation {
                "create" => gateway.create_process("oracle-challenge").await.map(|_| ()),
                "poll-evil" => gateway
                    .poll("https://evil.example/api/poll")
                    .await
                    .map(|_| ()),
                _ => gateway
                    .exchange("oracle-process", "oracle-token", "oracle-verifier")
                    .await
                    .map(|_| ()),
            }
        })
        .expect_err("the scripted failure fails the operation");
        let printed = error.to_string();
        for secret in secrets {
            assert!(
                !printed.contains(secret),
                "{operation}: {printed:?} leaks {secret:?}"
            );
        }
    }
}

// --------------------------------------------------------------------------
// The production constructor (US-187)
// --------------------------------------------------------------------------

#[test]
fn for_provider_builds_only_when_the_entry_resolves_bases() {
    assert!(
        HttpSignInGateway::for_provider(&provider(&[("name", "mistral"), ("backend", "mistral")]))
            .is_some()
    );
    assert!(
        HttpSignInGateway::for_provider(&provider(&[("name", "llamacpp"), ("backend", "generic")]))
            .is_none()
    );
}

#[test]
fn poll_rejects_an_off_base_url_with_the_poll_code() {
    let client = ScriptedHttpClient::new(vec![]);
    let mut gateway = HttpSignInGateway::new(
        "https://console.mistral.ai",
        "https://console.mistral.ai/api",
        client,
    );
    let error = block_on(gateway.poll("https://console.mistral.ai/elsewhere"))
        .expect_err("an off-base poll URL is refused");
    assert_eq!(error.code, SignInErrorCode::PollFailed);
}
