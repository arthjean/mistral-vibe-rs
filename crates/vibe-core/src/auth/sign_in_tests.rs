//! What the corpus cannot measure about the sign-in service: the PKCE
//! entropy floor, the timestamp normalization, the cancellation contract, the
//! detached browser launcher and the redaction of every derived string. The
//! behavioral surface itself is replayed in `setup_auth_parity_tests`.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll};

use serde_json::json;

use super::sign_in::{
    SignInErrorCode, SignInEvent, SignInService, UtcTimestamp, browser_launch_candidates,
    code_challenge, generate_code_verifier, launch_detached,
};
use super::testing::{ScriptedOpener, ScriptedSignInGateway, ScriptedSignInRuntime};

const VERIFIER: &str = "unit-test-verifier";

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds")
        .block_on(future)
}

fn service_with(
    polls: Vec<serde_json::Value>,
    opener: ScriptedOpener,
) -> SignInService<ScriptedSignInGateway, ScriptedSignInRuntime> {
    SignInService::new(
        ScriptedSignInGateway::new(None, 600.0, polls, json!("unit-test-key")),
        ScriptedSignInRuntime::new(opener, VERIFIER),
    )
}

// --------------------------------------------------------------------------
// PKCE (US-187)
// --------------------------------------------------------------------------

/// RFC 7636 appendix B: the published verifier and its challenge.
#[test]
fn the_challenge_matches_the_rfc_7636_test_vector() {
    assert_eq!(
        code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

/// 64 random bytes as unpadded base64url: 86 characters, all unreserved,
/// 512 bits of entropy over the RFC 7636 §7.1 floor of 256.
#[test]
fn the_generated_verifier_is_86_unreserved_characters_and_unique() {
    let first = generate_code_verifier().expect("the OS random source answers");
    let second = generate_code_verifier().expect("the OS random source answers");
    assert_eq!(first.len(), 86);
    assert!(
        first
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')),
        "the verifier strays outside the unreserved set: {first}"
    );
    assert_ne!(first, second, "two draws answered the same verifier");
}

// --------------------------------------------------------------------------
// Timestamps (US-187)
// --------------------------------------------------------------------------

#[test]
fn a_trailing_z_is_accepted_and_normalized_to_utc() {
    let parsed = UtcTimestamp::parse_iso8601("2026-01-01T00:10:00Z").expect("the zulu form parses");
    assert_eq!(parsed.to_iso8601(), "2026-01-01T00:10:00+00:00");
}

#[test]
fn an_offset_timestamp_is_normalized_to_utc() {
    let parsed =
        UtcTimestamp::parse_iso8601("2026-01-01T01:30:00+01:00").expect("the offset form parses");
    assert_eq!(parsed.to_iso8601(), "2026-01-01T00:30:00+00:00");
    let negative =
        UtcTimestamp::parse_iso8601("2025-12-31T19:30:00-05:00").expect("the offset form parses");
    assert_eq!(negative.to_iso8601(), "2026-01-01T00:30:00+00:00");
}

#[test]
fn fractional_seconds_survive_the_round_trip() {
    let parsed = UtcTimestamp::parse_iso8601("2026-01-01T00:00:00.500000+00:00")
        .expect("the fractional form parses");
    assert_eq!(parsed.to_iso8601(), "2026-01-01T00:00:00.500000+00:00");
    assert!(parsed > UtcTimestamp::parse_iso8601("2026-01-01T00:00:00Z").expect("parses"));
}

#[test]
fn malformed_timestamps_parse_to_none() {
    for value in [
        "",
        "not a date",
        "2026-13-01T00:00:00Z",
        "2026-01-32T00:00:00Z",
        "2026-01-01T24:00:00Z",
        "2026-01-01",
        "2026-01-01T00:00:00+1:00",
    ] {
        assert!(
            UtcTimestamp::parse_iso8601(value).is_none(),
            "{value:?} parsed"
        );
    }
}

// --------------------------------------------------------------------------
// Cancellation (US-189)
// --------------------------------------------------------------------------

/// Dropping the flow mid-wait and closing the service closes the gateway,
/// and no exchange ever ran, so there is no credential to have persisted.
#[test]
fn a_cancelled_wait_closes_the_gateway_and_exchanges_nothing() {
    let gateway = ScriptedSignInGateway::new(
        None,
        600.0,
        vec![json!({"status": "pending"})],
        json!("key"),
    );
    let mut runtime = ScriptedSignInRuntime::new(ScriptedOpener::Accept, VERIFIER);
    runtime.hang_on_sleep = true;
    let mut service = SignInService::new(gateway, runtime);
    block_on(async {
        {
            let mut on_event = |_: SignInEvent| {};
            let mut future = pin!(service.authenticate(&mut on_event));
            let waker = futures_util::task::noop_waker();
            let mut context = Context::from_waker(&waker);
            assert!(
                matches!(future.as_mut().poll(&mut context), Poll::Pending),
                "the flow should be parked in the hung sleep"
            );
        }
        service.close().await;
    });
    let (gateway, _) = service.into_parts();
    assert_eq!(gateway.closed, 1, "the gateway was not closed");
    assert_eq!(gateway.calls, ["create", "poll"]);
}

// --------------------------------------------------------------------------
// The sign-in URL stays retrievable (US-190)
// --------------------------------------------------------------------------

#[test]
fn the_sign_in_url_stays_retrievable_after_the_browser_opened() {
    let mut service = service_with(
        vec![json!({"status": "completed", "exchangeToken": "token"})],
        ScriptedOpener::Accept,
    );
    let mut started_url = None;
    block_on(async {
        let mut on_event = |event: SignInEvent| {
            if let SignInEvent::AttemptStarted { sign_in_url, .. } = event {
                started_url = Some(sign_in_url);
            }
        };
        let key = service
            .authenticate(&mut on_event)
            .await
            .expect("the scripted flow completes");
        assert_eq!(key, "unit-test-key");
    });
    assert_eq!(
        started_url.as_deref(),
        Some("https://console.mistral.ai/oracle/sign-in"),
        "the caller cannot reopen a URL it never received"
    );
}

#[test]
fn a_refused_browser_still_hands_the_caller_the_url() {
    let mut service = service_with(vec![], ScriptedOpener::Refuse);
    let mut started_url = None;
    let error = block_on(async {
        let mut on_event = |event: SignInEvent| {
            if let SignInEvent::AttemptStarted { sign_in_url, .. } = event {
                started_url = Some(sign_in_url);
            }
        };
        service
            .authenticate(&mut on_event)
            .await
            .expect_err("a refused opener fails the flow")
    });
    assert_eq!(error.code, SignInErrorCode::OpenBrowserFailed);
    assert!(started_url.is_some(), "the URL for manual use was lost");
}

/// A launcher that fails outright and one that merely declines both mean no
/// browser opened, and the reference collapses them into the same code.
#[test]
fn a_raising_opener_produces_the_same_code_as_a_refusing_one() {
    let mut service = service_with(vec![], ScriptedOpener::Raise);
    let error = block_on(async {
        let mut on_event = |_: SignInEvent| {};
        service
            .authenticate(&mut on_event)
            .await
            .expect_err("a raising opener fails the flow")
    });
    assert_eq!(error.code, SignInErrorCode::OpenBrowserFailed);
    let (gateway, runtime) = service.into_parts();
    assert_eq!(
        gateway.calls,
        ["create"],
        "the flow polled after the failure"
    );
    assert_eq!(runtime.opened.len(), 1);
}

// --------------------------------------------------------------------------
// The detached launcher (US-190)
// --------------------------------------------------------------------------

#[test]
fn the_launch_candidates_match_the_platform() {
    let candidates = browser_launch_candidates();
    if cfg!(target_os = "macos") {
        assert_eq!(candidates, [("open", &[] as &[&str])]);
    } else if cfg!(windows) {
        assert_eq!(candidates, [("cmd", &["/C", "start", ""] as &[&str])]);
    } else {
        assert_eq!(
            candidates,
            [("xdg-open", &[] as &[&str]), ("gio", &["open"] as &[&str])]
        );
    }
}

#[test]
fn a_missing_launcher_reports_the_spawn_failure() {
    assert!(launch_detached("vibe-no-such-launcher", &[], "https://example.invalid").is_err());
}

/// The launcher must never draw over the terminal ratatui owns: every child
/// descriptor points at the null device, proven from the child's own view.
#[cfg(target_os = "linux")]
#[test]
fn the_launched_child_holds_only_null_descriptors() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let report = scratch.path().join("fd-report");
    let script = format!(
        "exec 3>&1 4>&2; readlink /proc/$$/fd/0 > {report}; \
         readlink /proc/$$/fd/3 >> {report}; readlink /proc/$$/fd/4 >> {report}",
        report = report.display()
    );
    launch_detached("sh", &["-c", &script], "ignored-url").expect("the probe child spawns");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let lines = loop {
        if let Ok(contents) = std::fs::read_to_string(&report)
            && contents.lines().count() == 3
        {
            break contents;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the probe child never reported"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    for line in lines.lines() {
        assert_eq!(
            line, "/dev/null",
            "a child descriptor escaped the null device"
        );
    }
}

/// A launcher that blocks must not stall the flow: the spawn returns without
/// waiting on the child.
#[test]
fn a_blocking_launcher_does_not_block_the_spawn() {
    let started = std::time::Instant::now();
    let spawned = if cfg!(windows) {
        launch_detached(
            "cmd",
            &["/C", "timeout", "/T", "5", "&&", "rem"],
            "ignored-url",
        )
    } else {
        launch_detached("sh", &["-c", "sleep 5"], "ignored-url")
    };
    assert!(spawned.is_ok(), "the blocking probe failed to spawn");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the spawn waited on the child"
    );
}

// --------------------------------------------------------------------------
// Redaction (US-188, FR-16)
// --------------------------------------------------------------------------

#[test]
fn the_attempt_debug_output_redacts_the_verifier() {
    let mut service = service_with(vec![], ScriptedOpener::Accept);
    let attempt = block_on(service.start_attempt()).expect("the scripted attempt starts");
    let printed = format!("{attempt:?}");
    assert!(printed.contains("<redacted>"));
    assert!(
        !printed.contains(VERIFIER),
        "the PKCE verifier leaked into Debug output"
    );
}

#[test]
fn every_error_code_names_itself_in_its_display_form() {
    for code in SignInErrorCode::ALL {
        let error = super::sign_in::SignInError::new(code);
        let printed = error.to_string();
        assert!(
            printed.contains(code.as_str()),
            "{printed:?} does not name {}",
            code.as_str()
        );
    }
}

#[test]
fn a_provider_error_repeats_the_server_sentence() {
    let mut service = service_with(
        vec![json!({"status": "error", "message": "the server said no"})],
        ScriptedOpener::Accept,
    );
    let error = block_on(async {
        let mut on_event = |_: SignInEvent| {};
        service
            .authenticate(&mut on_event)
            .await
            .expect_err("the scripted provider error fails the flow")
    });
    assert_eq!(error.code, SignInErrorCode::ProviderError);
    assert_eq!(error.message(), "the server said no");
}
