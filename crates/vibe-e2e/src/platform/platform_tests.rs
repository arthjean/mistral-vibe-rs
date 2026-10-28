use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Platform, Reply};

fn request(platform: &Platform, method: &str, path: &str, body: &Value, extra: &str) -> TcpStream {
    let body = body.to_string();
    let mut stream = TcpStream::connect(platform.address()).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n{extra}\
         content-length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    stream
}

/// The raw response: status line and headers, then the body as sent.
fn exchange(platform: &Platform, path: &str, body: &Value) -> (String, String) {
    let mut stream = request(platform, "POST", path, body, "");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    (head.to_owned(), body.to_owned())
}

/// The chunks of a chunked body, and whether the terminating chunk arrived.
fn chunks(body: &str) -> (Vec<String>, bool) {
    let mut rest = body;
    let mut chunks = Vec::new();
    loop {
        let Some((size, after)) = rest.split_once("\r\n") else {
            return (chunks, false);
        };
        let size = usize::from_str_radix(size, 16).unwrap();
        if size == 0 {
            return (chunks, true);
        }
        chunks.push(after[..size].to_owned());
        rest = &after[size + 2..];
    }
}

fn chat(stream: bool) -> Value {
    json!({
        "model": "e2e-model-v1",
        "stream": stream,
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [{"type": "function", "function": {"name": "bash"}}],
    })
}

fn streamed(reply: Reply) -> (String, Vec<String>, bool) {
    let platform = Platform::start().unwrap();
    platform.reply(reply);
    let (head, body) = exchange(&platform, "/v1/chat/completions", &chat(true));
    let (events, terminated) = chunks(&body);
    (head, events, terminated)
}

#[test]
fn text_is_served_as_server_sent_events_one_chunk_each() {
    let (head, events, terminated) = streamed(Reply::text("hello there"));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("text/event-stream"), "{head}");
    assert!(terminated);
    assert_eq!(events.len(), 4, "{events:?}");
    assert!(
        events
            .iter()
            .all(|e| e.starts_with("data: ") && e.ends_with("\n\n"))
    );
    assert!(
        events[1].contains(r#""content":"hello there""#),
        "{events:?}"
    );
    assert!(
        events[2].contains(r#""finish_reason":"stop""#),
        "{events:?}"
    );
    assert_eq!(events[3], "data: [DONE]\n\n");
}

#[test]
fn tool_calls_carry_an_id_a_name_and_their_arguments() {
    let (_, events, _) = streamed(
        Reply::tool_call("bash", json!({"command": "ls"})).and_tool_call("todo", json!({})),
    );
    let calls: Vec<&String> = events.iter().filter(|e| e.contains("tool_calls")).collect();
    assert!(calls[0].contains(r#""id":"e2e000001""#), "{calls:?}");
    assert!(calls[0].contains(r#""name":"bash""#), "{calls:?}");
    assert!(
        calls[0].contains(r#""arguments":"{\"command\":\"ls\"}""#),
        "{calls:?}"
    );
    assert!(calls[1].contains(r#""id":"e2e000002""#), "{calls:?}");
    assert!(
        events
            .iter()
            .any(|e| e.contains(r#""finish_reason":"tool_calls""#))
    );
}

#[test]
fn generic_reasoning_is_streamed_as_reasoning_content() {
    let (_, events, _) = streamed(Reply::text("done").reasoning("pondering"));
    assert!(
        events
            .iter()
            .any(|e| e.contains(r#""reasoning_content":"pondering""#)),
        "{events:?}"
    );
}

#[test]
fn mistral_thinking_is_streamed_as_a_thinking_chunk() {
    let (_, events, _) = streamed(Reply::text("done").thinking("musing"));
    assert!(
        events.iter().any(|e| e.contains(
            r#""content":[{"type":"thinking","thinking":[{"type":"text","text":"musing"}]}]"#
        )),
        "{events:?}"
    );
}

#[test]
fn usage_counts_ride_on_the_last_chunk() {
    let (_, events, _) = streamed(Reply::text("x").usage(42, 7));
    let last = &events[events.len() - 2];
    assert!(last.contains(r#""prompt_tokens":42"#), "{last}");
    assert!(last.contains(r#""total_tokens":49"#), "{last}");
}

#[test]
fn finish_reasons_are_scripted_or_left_out() {
    let (_, events, _) = streamed(Reply::text("x").finish("length"));
    assert!(
        events
            .iter()
            .any(|e| e.contains(r#""finish_reason":"length""#))
    );
    let (_, events, _) = streamed(Reply::text("x").no_finish());
    assert!(
        events
            .iter()
            .all(|e| !e.contains(r#""finish_reason":"stop""#))
    );
    assert!(
        events[events.len() - 2].contains(r#""finish_reason":null"#),
        "{events:?}"
    );
}

#[test]
fn an_http_status_is_served_with_its_body_and_headers() {
    let platform = Platform::start().unwrap();
    platform.reply(
        Reply::status(429)
            .body(r#"{"message":"slow down"}"#)
            .header("Retry-After", "1"),
    );
    let (head, body) = exchange(&platform, "/v1/chat/completions", &chat(true));
    assert!(head.starts_with("HTTP/1.1 429"), "{head}");
    assert!(head.contains("retry-after: 1"), "{head}");
    assert!(head.contains("application/json"), "{head}");
    assert_eq!(body, r#"{"message":"slow down"}"#);
}

#[test]
fn raw_events_are_served_as_written() {
    let (_, events, terminated) = streamed(Reply::sse([
        "event: error\ndata: {\"error\":\"boom\"}",
        "data: [DONE]",
    ]));
    assert!(terminated);
    assert_eq!(
        events,
        [
            "event: error\ndata: {\"error\":\"boom\"}\n\n",
            "data: [DONE]\n\n"
        ]
    );
}

#[test]
fn a_cut_stream_ends_cleanly_before_its_last_event() {
    let (_, events, terminated) = streamed(Reply::text("x").cut_before_last());
    assert!(terminated);
    assert_eq!(events.len(), 3);
    assert!(events.iter().all(|e| !e.contains("[DONE]")));
}

#[test]
fn a_dropped_connection_ends_the_body_without_its_terminator() {
    let (_, events, terminated) = streamed(Reply::text("x").drop_after(2));
    assert!(!terminated);
    assert_eq!(events.len(), 2, "{events:?}");
}

#[test]
fn a_malformed_line_precedes_the_last_chunk() {
    let (_, events, _) = streamed(Reply::text("x").malformed());
    assert_eq!(events[events.len() - 3], "data: {this is not json\n\n");
}

#[test]
fn a_request_that_does_not_stream_gets_one_completion() {
    let platform = Platform::start().unwrap();
    platform.reply(Reply::text("whole"));
    let (head, body) = exchange(&platform, "/v1/chat/completions", &chat(false));
    assert!(head.contains("content-length"), "{head}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "whole");
    assert_eq!(body["model"], "e2e-model-v1");
}

#[test]
fn a_one_token_request_without_tools_is_answered_from_the_probe_queue() {
    let platform = Platform::start().unwrap();
    platform.reply(Reply::text("turn answer"));
    let probe =
        json!({"model": "m", "max_tokens": 1, "messages": [{"role": "user", "content": "hi"}]});
    let (_, body) = exchange(&platform, "/v1/chat/completions", &probe);
    assert!(body.contains("ok"), "{body}");
    let (_, body) = exchange(&platform, "/v1/chat/completions", &chat(false));
    assert!(body.contains("turn answer"), "{body}");
    let queues: Vec<String> = platform.journal().into_iter().map(|e| e.queue).collect();
    assert_eq!(queues, ["probe", "turn"]);
    assert!(platform.failures().is_empty(), "{:?}", platform.failures());
}

#[test]
fn a_marker_queue_answers_before_the_turn_queue() {
    let platform = Platform::start().unwrap();
    platform.replies([Reply::text("first turn"), Reply::text("second turn")]);
    platform.when("SUMMARIZE-NOW", Reply::text("the summary"));
    let (_, body) = exchange(&platform, "/v1/chat/completions", &chat(false));
    assert!(body.contains("first turn"));
    let marked =
        json!({"model": "m", "messages": [{"role": "user", "content": "please SUMMARIZE-NOW"}]});
    let (_, body) = exchange(&platform, "/v1/chat/completions", &marked);
    assert!(body.contains("the summary"), "{body}");
    let (_, body) = exchange(&platform, "/v1/chat/completions", &chat(false));
    assert!(body.contains("second turn"), "{body}");
    let queues: Vec<String> = platform.journal().into_iter().map(|e| e.queue).collect();
    assert_eq!(queues, ["turn", "when:SUMMARIZE-NOW", "turn"]);
}

#[test]
fn a_gate_holds_its_response_until_released() {
    let platform = Platform::start().unwrap();
    platform.reply(Reply::text("held"));
    let gate = platform.gate(1);
    let client = platform.clone();
    let answered = thread::spawn(move || exchange(&client, "/v1/chat/completions", &chat(false)).1);
    let entry = platform.await_request(1, Duration::from_secs(5)).unwrap();
    assert_eq!(entry.queue, "turn");
    thread::sleep(Duration::from_millis(200));
    assert!(
        !answered.is_finished(),
        "the gated response was served early"
    );
    gate.release();
    assert!(answered.join().unwrap().contains("held"));
}

#[test]
fn await_request_fails_at_its_deadline() {
    let platform = Platform::start().unwrap();
    let error = platform
        .await_request(1, Duration::from_millis(100))
        .unwrap_err();
    assert!(error.0.contains("chat request #1 never arrived"), "{error}");
}

#[test]
fn the_journal_keeps_every_request_in_arrival_order() {
    let platform = Platform::start().unwrap();
    platform.reply(Reply::text("x"));
    let mut stream = request(
        &platform,
        "POST",
        "/v1/chat/completions",
        &chat(false),
        "x-probe: 7\r\n",
    );
    stream.read_to_end(&mut Vec::new()).unwrap();
    let mut stream = request(
        &platform,
        "GET",
        "/api/v1/code/managed-config?x=1",
        &Value::Null,
        "",
    );
    stream.read_to_end(&mut Vec::new()).unwrap();
    let journal = platform.journal();
    assert_eq!(journal.len(), 2);
    assert_eq!(journal[0].path, "/v1/chat/completions");
    assert_eq!(journal[0].header("X-Probe"), Some("7"));
    assert_eq!(journal[0].body["messages"][0]["content"], "hello");
    assert_eq!(journal[0].queue, "turn");
    assert_eq!(journal[1].path, "/api/v1/code/managed-config");
    assert_eq!(journal[1].queue, "managed_config");
    assert!(journal[0].at_ms <= journal[1].at_ms);
    assert_eq!(platform.chat_requests().len(), 1);
}

#[test]
fn a_tool_message_answering_an_undeclared_call_fails_the_trial() {
    let platform = Platform::start().unwrap();
    platform.reply(Reply::text("x"));
    let body = json!({"model": "m", "messages": [
        {"role": "user", "content": "go"},
        {"role": "assistant", "content": "", "tool_calls": [{"id": "call_a", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]},
        {"role": "tool", "tool_call_id": "call_b", "content": "out"},
    ]});
    exchange(&platform, "/v1/chat/completions", &body);
    let failures = platform.failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].contains("call_b") && failures[0].contains("call_a"),
        "{failures:?}"
    );
}

#[test]
fn an_exhausted_turn_queue_answers_500_and_fails_the_trial() {
    let platform = Platform::start().unwrap();
    let (head, body) = exchange(&platform, "/v1/chat/completions", &chat(true));
    assert!(head.starts_with("HTTP/1.1 500"), "{head}");
    assert!(body.contains("no scripted turn response left"), "{body}");
    let failures = platform.failures();
    assert!(
        failures[0].contains("unexpected turn request #1"),
        "{failures:?}"
    );
    assert!(failures[0].contains("hello"), "{failures:?}");
}

#[test]
fn exact_counts_fail_on_a_missing_request() {
    let platform = Platform::start().unwrap();
    platform.replies([Reply::text("one"), Reply::text("two")]);
    platform.exact();
    exchange(&platform, "/v1/chat/completions", &chat(false));
    let failures = platform.failures();
    assert!(
        failures[0].contains("expected 1 more turn request"),
        "{failures:?}"
    );
}

#[test]
fn paced_chunks_arrive_at_least_ninety_milliseconds_apart() {
    let platform = Platform::start().unwrap();
    platform.reply(Reply::chunks(["a", "b", "c", "d", "e"]).paced(100));
    let stream = request(&platform, "POST", "/v1/chat/completions", &chat(true), "");
    let mut reader = BufReader::new(stream);
    let mut arrivals = Vec::new();
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap() > 0 {
        if line.starts_with("data: ")
            && line.contains(r#""content":""#)
            && !line.contains(r#""content":"""#)
        {
            arrivals.push(Instant::now());
        }
        line.clear();
    }
    assert_eq!(arrivals.len(), 5);
    for pair in arrivals.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(
            gap >= Duration::from_millis(90),
            "chunks arrived {gap:?} apart"
        );
    }
}

#[test]
fn served_events_are_counted_while_a_paced_stream_is_written() {
    let platform = Platform::start().unwrap();
    // The role chunk, five pieces, the finish chunk, and `[DONE]`.
    platform.reply(Reply::chunks(["a", "b", "c", "d", "e"]).paced(100));
    let client = platform.clone();
    let answered = thread::spawn(move || exchange(&client, "/v1/chat/completions", &chat(true)));
    platform.await_events(1, 3, Duration::from_secs(5)).unwrap();
    let served = platform.events_served(1);
    assert!((3..8).contains(&served), "{served} events were counted");
    answered.join().unwrap();
    assert_eq!(platform.events_served(1), 8);
    let error = platform
        .await_events(2, 1, Duration::from_millis(100))
        .unwrap_err();
    assert!(error.0.contains("chat request #2"), "{error}");
}
