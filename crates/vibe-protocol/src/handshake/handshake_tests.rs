use super::*;
use serde_json::json;

#[test]
fn camel_case_is_strict_and_variants_are_closed() {
    let camel = json!({
        "clientInfo": {
            "name": "test",
            "version": "1",
            "title": null,
            "entrypoint": "unknown",
            "terminalEmulator": "vscode"
        }
    });
    let parsed = serde_json::from_value::<InitializeParams>(camel).expect("camelCase params");
    assert_eq!(
        parsed.client_info.terminal_emulator,
        TerminalEmulator::Vscode
    );
    assert_eq!(parsed.capabilities, ClientCapabilities::default());

    let snake = json!({
        "client_info": {"name": "test", "version": "1"}
    });
    assert!(serde_json::from_value::<InitializeParams>(snake).is_err());
    assert!(serde_json::from_value::<CallbackKind>(json!("unknown")).is_err());
}

#[test]
fn the_handshake_accepts_every_reference_capability_field() {
    let params = serde_json::from_value::<InitializeParams>(json!({
        "clientInfo": {"name": "editor", "version": "1"},
        "capabilities": {
            "callbackKinds": ["approval"],
            "clientTools": ["filesystem/read"],
            "disabledNotifications": ["warning"]
        }
    }))
    .expect("a conforming client completes the handshake");
    assert_eq!(params.capabilities.disabled_notifications, ["warning"]);

    // A field the reference does not declare still fails, which is what
    // keeps `deny_unknown_fields` discriminating.
    assert!(
        serde_json::from_value::<InitializeParams>(json!({
            "clientInfo": {"name": "editor", "version": "1"},
            "capabilities": {"invented": true}
        }))
        .is_err()
    );
}
