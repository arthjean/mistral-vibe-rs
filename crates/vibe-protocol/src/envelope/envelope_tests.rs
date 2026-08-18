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
            json!({"jsonrpc": "2.0", "id": 1, "method": "turn/start", "params": {}}),
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
fn error_frames_carry_the_null_data_key() {
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
        br#"{"jsonrpc":"2.0","id":1,"error":{"code":"not_found","message":"gone","data":null}}"#
    );
    assert_eq!(
        decode_frame(&encode_frame(&frame)).expect("round trip"),
        frame
    );
}

#[test]
fn an_absent_params_key_is_refused() {
    // The reference declares `params` without a default on both inbound
    // shapes, so a frame that omits it fails validation there. Reading it as
    // empty here would let a client through that upstream turns away.
    for value in [
        json!({"jsonrpc": "2.0", "method": "turn/started"}),
        json!({"jsonrpc": "2.0", "id": 1, "method": "turn/start"}),
    ] {
        let encoded = serde_json::to_vec(&value).expect("JSON fixture");
        assert!(decode_frame(&encoded).is_err(), "accepted {value}");
    }
}

#[test]
fn outbound_frames_always_carry_params() {
    let frame = Envelope::Notification(Notification {
        jsonrpc: JsonRpcVersion::V2,
        method: "turn/started".to_owned(),
        params: BTreeMap::new(),
    });
    assert_eq!(
        encode_frame(&frame),
        br#"{"jsonrpc":"2.0","method":"turn/started","params":{}}"#
    );
    assert_eq!(
        decode_frame(&encode_frame(&frame)).expect("round trip"),
        frame
    );
}

#[test]
fn a_rejection_names_the_envelope_and_the_offending_field() {
    // An untagged rejection says only that no variant matched, and the caller
    // answers a rejection by closing the connection. These assert that the one
    // message it gets to log identifies the frame and the cause.
    let cases: [(&[u8], &str, &str); 5] = [
        (
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":"not_found"}}"#,
            "error response",
            "missing field `message`",
        ),
        (
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":"nope","message":"x"}}"#,
            "error response",
            "unknown variant `nope`",
        ),
        (
            br#"{"jsonrpc":"2.0","id":1,"method":"turn/start","params":{},"extra":1}"#,
            "request",
            "unknown field `extra`",
        ),
        (
            br#"{"jsonrpc":"2.0","method":"turn/started"}"#,
            "notification",
            "missing field `params`",
        ),
        (
            br#"{"jsonrpc":"2.0","id":1,"result":[]}"#,
            "success response",
            "invalid type",
        ),
    ];
    for (frame, expected_kind, expected_cause) in cases {
        let error = decode_frame(frame).expect_err("invalid frame");
        let ProtocolValidationError::Malformed { kind, .. } = &error else {
            unreachable!("{} produced {error:?}", String::from_utf8_lossy(frame));
        };
        assert_eq!(*kind, expected_kind, "{}", String::from_utf8_lossy(frame));
        assert!(
            error.to_string().contains(expected_cause),
            "`{error}` does not name `{expected_cause}`"
        );
    }
}

#[test]
fn frames_with_no_envelope_shape_are_named_as_such() {
    assert!(matches!(
        decode_frame(br#"{"jsonrpc":"2.0","id":1}"#).expect_err("no shape"),
        ProtocolValidationError::UnknownShape
    ));
    assert!(matches!(
        decode_frame(br#"[1,2,3]"#).expect_err("not an object"),
        ProtocolValidationError::NotAnObject(_)
    ));
    assert!(matches!(
        decode_frame(b"not json").expect_err("not json"),
        ProtocolValidationError::NotAnObject(_)
    ));
}

#[test]
fn exactly_one_envelope_claims_each_valid_frame() {
    // `Envelope` is untagged, so the variant a frame decodes to is whichever
    // one serde tries first that accepts it. That is only safe while no two
    // variants accept the same frame, an invariant `deny_unknown_fields` holds
    // up and nothing else does: relaxing it on one struct would silently make
    // the declaration order above load-bearing. Counting claimants here fails
    // the moment that happens, rather than at the next reordering.
    let frames: [&[u8]; 4] = [
        br#"{"jsonrpc":"2.0","method":"turn/started","params":{}}"#,
        br#"{"jsonrpc":"2.0","id":1,"method":"turn/start","params":{}}"#,
        br#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
        br#"{"jsonrpc":"2.0","id":1,"error":{"code":"not_found","message":"gone"}}"#,
    ];
    for frame in frames {
        let claimants = [
            refusal_of::<Notification>(frame).is_none(),
            refusal_of::<ServerRequest>(frame).is_none(),
            refusal_of::<SuccessResponse>(frame).is_none(),
            refusal_of::<ErrorResponse>(frame).is_none(),
        ]
        .into_iter()
        .filter(|claimed| *claimed)
        .count();
        assert_eq!(
            claimants,
            1,
            "{} is claimed by {claimants} envelopes",
            String::from_utf8_lossy(frame)
        );
    }
}
