use super::*;
use crate::ProtocolErrorCode;
use serde_json::json;

#[test]
fn valid_frames_round_trip_canonically() {
    let input = br#"{"jsonrpc":"2.0","id":"client-1","result":{"ok":true}}"#;
    let first = decode_frame(input).expect("valid fixture");
    let encoded = encode_frame(&first);
    let second = decode_frame(&encoded).expect("round-trip fixture");
    assert_eq!(first, second);
    assert_eq!(
        encoded,
        br#"{"jsonrpc":"2.0","id":"client-1","result":{"ok":true}}"#
    );
}

#[test]
fn each_envelope_variant_is_decoded_unambiguously() {
    let cases = [
        (
            json!({"jsonrpc": "2.0", "method": "turn/started", "params": {}}),
            "notification",
        ),
        (
            json!({"jsonrpc": "2.0", "method": "turn/started"}),
            "notification",
        ),
        (
            json!({"jsonrpc": "2.0", "id": 1, "method": "turn/start", "params": {}}),
            "request",
        ),
        (
            json!({"jsonrpc": "2.0", "id": 1, "method": "turn/start"}),
            "request",
        ),
        (json!({"jsonrpc": "2.0", "id": 1, "result": {}}), "success"),
        (
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": "not_found", "message": "gone"}}),
            "error",
        ),
    ];
    for (value, expected) in cases {
        let encoded = serde_json::to_vec(&value).expect("JSON fixture");
        let decoded = decode_frame(&encoded).expect("valid frame");
        let actual = match decoded {
            Envelope::Notification(_) => "notification",
            Envelope::Request(_) => "request",
            Envelope::Success(_) => "success",
            Envelope::Error(_) => "error",
        };
        assert_eq!(actual, expected, "misrouted {value}");
    }
}

#[test]
fn malformed_envelopes_are_rejected() {
    for value in [
        json!({"jsonrpc": "2.0", "id": "client-1"}),
        json!({"jsonrpc": "2.0", "id": "client-1", "result": {}, "error": {
            "code": "internal_error", "message": "failed"
        }}),
        json!({"jsonrpc": "2.0", "method": "initialized", "params": {}, "extra": true}),
        json!({"jsonrpc": "2.0", "id": true, "result": {}}),
        json!({"jsonrpc": "2.0", "id": 1, "result": []}),
        json!({"jsonrpc": "1.0", "id": 1, "result": {}}),
    ] {
        let encoded = serde_json::to_vec(&value).expect("JSON fixture");
        assert!(decode_frame(&encoded).is_err(), "accepted {value}");
    }
}

#[test]
fn null_error_data_stays_off_the_wire() {
    let frame = Envelope::Error(ErrorResponse {
        jsonrpc: JsonRpcVersion::V2,
        id: RequestId::Integer(1),
        error: ProtocolError {
            code: ProtocolErrorCode::NotFound,
            message: "gone".to_owned(),
            data: Value::Null,
        },
    });
    assert_eq!(
        encode_frame(&frame),
        br#"{"jsonrpc":"2.0","id":1,"error":{"code":"not_found","message":"gone"}}"#
    );
    assert_eq!(
        decode_frame(&encode_frame(&frame)).expect("round trip"),
        frame
    );
}
