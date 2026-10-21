use std::path::PathBuf;

use super::http::{event_data, take_event};
use super::stdio::stdio_program;
use super::*;

#[test]
fn an_event_stream_yields_its_message_data_and_skips_other_events() {
    let mut buffer =
        b"event: message\r\ndata: {\"id\":0}\r\n\r\n: keepalive\n\nevent: ping\ndata: x\n\n"
            .to_vec();
    let first = take_event(&mut buffer).expect("first event");
    assert_eq!(event_data(&first).as_deref(), Some("{\"id\":0}"));
    let comment = take_event(&mut buffer).expect("comment");
    assert_eq!(event_data(&comment), None);
    let ping = take_event(&mut buffer).expect("ping");
    assert_eq!(event_data(&ping), None);
    assert!(buffer.is_empty());
}

#[test]
fn a_tool_list_with_one_malformed_tool_fails_whole() {
    let error = parse_tools(&json!({"tools": [
        {"name": "echo", "inputSchema": {"type": "object"}},
        {"name": "schemaless"},
    ]}))
    .expect_err("the list is refused");
    assert!(matches!(error, SessionError::Protocol(_)));
    let tools = parse_tools(&json!({"tools": [
        {"name": "echo", "description": null, "inputSchema": {"type": "object"}},
    ]}))
    .expect("a null description is absent");
    assert_eq!(tools[0].description, None);
}

/// US-014 (Windows support): a program named with a directory or an
/// extension is launched as written, never searched for.
#[test]
fn a_program_with_a_directory_or_an_extension_is_used_as_written() {
    for written in [
        "server.cmd",
        "server.exe",
        "./server",
        r"C:\tools\server",
        "/opt/server",
    ] {
        assert_eq!(
            stdio_program(written).expect(written),
            PathBuf::from(written)
        );
    }
}

/// US-014 (Windows support): a bare name that resolves to nothing fails
/// naming every file it was looked for under.
#[cfg(windows)]
#[test]
fn a_bare_program_found_nowhere_names_every_file_tried() {
    let name = "vibe-no-such-mcp-server";
    let Err(SessionError::Transport(message)) = stdio_program(name) else {
        panic!("`{name}` resolved somewhere");
    };
    let tried = crate::platform::executable_names(name);
    assert!(tried.contains(&format!("{name}.cmd")), "{tried:?}");
    for file in tried {
        assert!(
            message.contains(&file),
            "`{file}` is missing from: {message}"
        );
    }
}

/// US-014 (Windows support): an argument std refuses to escape for a
/// batch script fails the launch naming the script and the argument.
#[cfg(windows)]
#[tokio::test]
async fn a_batch_argument_std_cannot_escape_is_named() {
    let directory = tempfile::tempdir().expect("scratch directory");
    let script = directory.path().join("server.cmd");
    std::fs::write(&script, "@echo off\r\n").expect("script");
    let (sender, _receiver) = mpsc::unbounded_channel();
    let arguments = vec!["--stdio".to_owned(), "two\nlines".to_owned()];
    let spawned = StdioChannel::spawn(
        &script.to_string_lossy(),
        &arguments,
        &BTreeMap::new(),
        None,
        sender,
    );
    let Err(SessionError::Transport(message)) = spawned else {
        panic!("the launch was not refused");
    };
    assert!(message.contains("server.cmd"), "{message}");
    assert!(message.contains(r#""two\nlines""#), "{message}");
    assert!(!message.contains("--stdio"), "{message}");
}
