//! An in-process request reaches the saved-session handlers without the
//! routing that validates a stdio one first. The handlers validate it through
//! [`super::validate_params`], so a client reads the same answer over either
//! transport, and that answer is the schema's.

use serde_json::{Value, json};
use vibe_protocol::{Envelope, TransportKind, decode_frame};

use super::{Issue, validate_method};
use crate::server::{AppServer, ServerConnection};

/// The methods only a root answers, over either transport.
const ROOTED: &[&str] = &[
    "session/turns/list",
    "session/log/read",
    "session/history/clear",
];

fn frame(id: i64, method: &str, params: &Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
        .expect("the request encodes")
}

fn ready(server: &AppServer, transport: TransportKind) -> ServerConnection {
    let mut connection = server.connect(transport);
    connection.dispatch(&frame(
        1,
        "initialize",
        &json!({"clientInfo": {"name": "test", "version": "1"}, "capabilities": {}}),
    ));
    connection.dispatch(
        &serde_json::to_vec(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
            .expect("the notification encodes"),
    );
    connection
}

/// The error `method` answers with, or `None` for a success.
fn answer(connection: &mut ServerConnection, method: &str, params: &Value) -> Option<Value> {
    let batch = connection.dispatch(&frame(7, method, params));
    match decode_frame(&batch.outbound[0]).expect("an answer") {
        Envelope::Error(error) => Some(serde_json::to_value(&error.error).expect("it encodes")),
        Envelope::Success(_) => None,
        other => unreachable!("{method} answered {other:?}"),
    }
}

/// Parameters each saved-session method refuses, for every reason its schema
/// has. Extra keys are written in sorted order: an in-process request keeps
/// no other, since its envelope sorts them.
fn refused() -> Vec<(&'static str, Value)> {
    vec![
        ("session/list", json!({"cwd": 42, "limit": 0})),
        (
            "session/list",
            json!({"limit": 9_223_372_036_854_775_808_u64}),
        ),
        (
            "session/list",
            json!({"limit": "abc", "cwds": ["a", 1, null]}),
        ),
        (
            "session/list",
            json!({"pinned": "maybe", "alpha": 1, "zeta": 2}),
        ),
        ("session/read", json!({"sessionId": null})),
        ("session/read", json!({"sessionId": "s", "historyLimit": 3})),
        (
            "session/read",
            json!({"sessionId": "s", "history": {"limit": 0, "extra": 1}}),
        ),
        ("session/history/get", json!({"historyLimit": 0})),
        (
            "session/history/get",
            json!({"sessionId": "s", "historyLimit": "x", "z": 1}),
        ),
        (
            "session/history/list",
            json!({"sessionId": "s", "page": null}),
        ),
        (
            "session/history/list",
            json!({"sessionId": "s", "page": {"direction": null}}),
        ),
        (
            "session/history/list",
            json!({
                "page": {"a": 2, "b": 1, "cursor": 4, "direction": "sideways", "limit": 0},
                "q": 1,
                "sessionId": "s",
                "turnId": 9,
            }),
        ),
        ("session/turns/list", json!({})),
        (
            "session/turns/list",
            json!({"page": {"limit": 501}, "sessionId": 1, "x": 1}),
        ),
        ("session/rename", json!({"extra": 2, "title": 1})),
        ("session/relocate", json!({"cwd": 1, "sessionId": [1]})),
        ("session/log/read", json!({"sessionId": 1, "x": 2})),
        ("session/resume", json!({"historyLimit": 0})),
        ("session/resume", json!({"agentConfig": 5})),
        (
            "session/resume",
            json!({"agentConfig": {"bogus": 1}, "sessionId": "s"}),
        ),
        (
            "session/resume",
            json!({"cwd": "/x", "sessionId": "s", "worktree": {}}),
        ),
        ("session/continue", json!({"sessionId": "s"})),
        (
            "session/continue",
            json!({"agentConfig": [], "historyLimit": "nope"}),
        ),
        ("session/fork", json!({})),
        (
            "session/fork",
            json!({
                "afterTurnId": 5,
                "attach": 2,
                "entryId": 3,
                "historyLimit": 600,
                "idempotencyKey": 4,
                "sourceSessionId": "s",
            }),
        ),
        (
            "session/fork",
            json!({"agentConfig": 1, "sourceSessionId": "s"}),
        ),
        ("session/delete", json!({"a": 1, "b": 1, "sessionId": 1})),
        ("session/history/clear", json!({"sessionId": 3})),
    ]
}

#[test]
fn a_saved_session_method_refuses_an_in_process_request_as_the_schema_does() {
    let server = AppServer::default();
    for (index, (method, params)) in refused().into_iter().enumerate() {
        let mut local = ready(&server, TransportKind::InProcess);
        let rooted = ROOTED.contains(&method);
        if rooted {
            let session = format!("in-process-{index}");
            let start = json!({"sessionId": session, "workingDirectory": "/workspace"});
            assert_eq!(answer(&mut local, "session/start", &start), None);
        }
        let refusal = answer(&mut local, method, &params)
            .unwrap_or_else(|| unreachable!("{method} {params} was accepted"));
        let expected = validate_method(method, &params).expect_err("the schema refuses it too");
        assert_eq!(
            refusal["code"],
            json!("invalid_params"),
            "{method} {params}"
        );
        assert_eq!(
            refusal["data"]["issues"],
            serde_json::to_value(expected.iter().map(Issue::wire).collect::<Vec<_>>())
                .expect("the issues encode"),
            "{method} {params}"
        );
        // A method any connection answers is refused over stdio in the same
        // words, which the routing validates.
        if !rooted {
            let mut remote = ready(&server, TransportKind::Stdio);
            assert_eq!(
                answer(&mut remote, method, &params),
                Some(refusal),
                "{method} {params}"
            );
        }
    }
}

/// What the reference models declare is accepted however it arrives: the
/// archived filter of a listing, a read's history page, a fork's null
/// configuration, and a float read as a boolean.
#[test]
fn a_saved_session_method_accepts_what_the_reference_model_declares() {
    let server = AppServer::default();
    for (method, params) in [
        (
            "session/list",
            json!({"cwd": "/nowhere-in-process", "includeArchived": true, "pinned": 1.0}),
        ),
        (
            "session/read",
            json!({"history": {"limit": 3}, "sessionId": "missing"}),
        ),
        (
            "session/read",
            json!({"history": null, "sessionId": "missing"}),
        ),
        (
            "session/fork",
            json!({"agentConfig": null, "attach": "off", "sourceSessionId": "missing"}),
        ),
    ] {
        let local = answer(
            &mut ready(&server, TransportKind::InProcess),
            method,
            &params,
        );
        assert_ne!(
            local.as_ref().map(|error| &error["code"]),
            Some(&json!("invalid_params")),
            "{method} {params}: {local:?}"
        );
        let remote = answer(&mut ready(&server, TransportKind::Stdio), method, &params);
        assert_eq!(local, remote, "{method} {params}");
    }
}
