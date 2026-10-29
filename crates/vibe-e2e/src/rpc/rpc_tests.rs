#![cfg(unix)]

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::rpc::{Answer, Rpc};
use crate::world::{Setup, World};

/// A peer running `script` under `sh`, with piped stdin.
fn peer(world: &World, script: &str) -> Rpc {
    Rpc::new(
        world
            .program("sh")
            .args(["-c", script])
            .stdin_piped()
            .spawn()
            .unwrap(),
    )
}

#[test]
fn responses_are_correlated_by_id_and_notifications_wait_in_order() {
    let world = World::new(Setup::default()).unwrap();
    // Answers the second request first, around two notifications.
    let mut rpc = peer(
        &world,
        r#"read a; read b
           echo '{"jsonrpc":"2.0","method":"note","params":{"n":1}}'
           echo '{"jsonrpc":"2.0","id":2,"result":"second"}'
           echo '{"jsonrpc":"2.0","method":"note","params":{"n":2}}'
           echo '{"jsonrpc":"2.0","id":1,"result":"first"}'
           sleep 5"#,
    );
    let first = rpc.send("one", json!({})).unwrap();
    let second = rpc.send("two", json!({})).unwrap();
    assert_eq!(rpc.response(second).unwrap()["result"], "second");
    assert_eq!(rpc.response(first).unwrap()["result"], "first");
    let note = rpc
        .await_notification(|m| m["params"]["n"] == 2, Duration::from_secs(5))
        .unwrap();
    assert_eq!(note["params"]["n"], 2);
    let earlier = rpc.await_method("note", Duration::from_secs(5)).unwrap();
    assert_eq!(earlier["params"]["n"], 1);
    assert_eq!(rpc.notifications().len(), 2);
    rpc.child().kill();
    assert_eq!(world.finish(), Vec::<String>::new());
}

#[test]
fn a_responder_answers_and_the_exchange_is_journaled() {
    let world = World::new(Setup::default()).unwrap();
    // Asks the client twice, then echoes each answer back as a notification.
    let mut rpc = peer(
        &world,
        r#"echo '{"jsonrpc":"2.0","id":"s1","method":"fs/read_text_file","params":{"path":"a"}}'
           read answer
           echo "{\"jsonrpc\":\"2.0\",\"method\":\"echo\",\"params\":$answer}"
           echo '{"jsonrpc":"2.0","id":"s2","method":"callback/call","params":{"callback":{"callbackId":"c1"}}}'
           read ack; read follow
           echo "{\"jsonrpc\":\"2.0\",\"method\":\"echo\",\"params\":$follow}"
           echo '{"jsonrpc":"2.0","id":"e2e-client-1","result":{"accepted":true}}'
           echo '{"jsonrpc":"2.0","id":"s3","method":"terminal/create","params":{}}'
           sleep 5"#,
    );
    rpc.respond("fs/read_text_file", |params| {
        Answer::result(
            json!({"content": format!("buffer of {}", params["path"].as_str().unwrap())}),
        )
    });
    rpc.respond("callback/call", |params| {
        let id = params["callback"]["callbackId"].clone();
        Answer::result(json!({"callbackId": id, "accepted": true}))
            .then("callback/result", json!({"result": {"callbackId": id}}))
    });
    let echoed = rpc.await_method("echo", Duration::from_secs(5)).unwrap();
    assert_eq!(echoed["params"]["result"]["content"], "buffer of a");
    assert_eq!(echoed["params"]["id"], "s1");
    let follow = rpc.await_method("echo", Duration::from_secs(5)).unwrap();
    assert_eq!(follow["params"]["method"], "callback/result");
    // An unanswered request waits for the scenario.
    let pending = rpc
        .await_request(|m| m["method"] == "terminal/create", Duration::from_secs(5))
        .unwrap();
    assert_eq!(pending["id"], "s3");
    let exchanges = rpc.exchanges();
    assert_eq!(exchanges.len(), 2);
    assert_eq!(exchanges[0].method, "fs/read_text_file");
    assert_eq!(exchanges[0].params, json!({"path": "a"}));
    assert_eq!(exchanges[1].follow_up_ids, vec!["e2e-client-1".to_owned()]);
    assert_eq!(exchanges[1].follow_ups[0]["result"]["accepted"], true);
    rpc.child().kill();
    assert_eq!(world.finish(), Vec::<String>::new());
}

#[test]
fn raw_mode_writes_unframed_bytes_and_reads_unparsed_lines() {
    let world = World::new(Setup::default()).unwrap();
    let mut rpc = peer(
        &world,
        "read line; printf '%s|\\n' \"$line\"; echo 'not json'",
    );
    rpc.write_raw(b"\xef\xbb\xbf{\"partial\":").unwrap();
    rpc.write_raw(b"1}\r\n").unwrap();
    assert_eq!(
        rpc.raw_line(Duration::from_secs(5)).unwrap(),
        "\u{feff}{\"partial\":1}\r|"
    );
    assert_eq!(rpc.raw_line(Duration::from_secs(5)).unwrap(), "not json");
}

#[test]
fn a_child_that_exits_while_a_request_is_pending_fails_with_its_status_and_stderr() {
    let world = World::new(Setup::default()).unwrap();
    let mut rpc = peer(&world, "read line; echo 'server gave up' >&2; exit 3");
    let started = Instant::now();
    let error = rpc.request("hello", Value::Null).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5), "{error}");
    assert!(error.0.contains("the response to request 1"), "{error}");
    assert!(error.0.contains("exit status: 3"), "{error}");
    assert!(error.0.contains("server gave up"), "{error}");
}
