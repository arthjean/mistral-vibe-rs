use serde_json::json;

use super::decode_export;

fn varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = u8::try_from(value & 0x7f).unwrap();
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn bytes(field: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    varint(field << 3 | 2, &mut out);
    varint(payload.len() as u64, &mut out);
    out.extend_from_slice(payload);
    out
}

fn number(field: u64, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    varint(field << 3, &mut out);
    varint(value, &mut out);
    out
}

fn attribute(key: &str, any: Vec<u8>) -> Vec<u8> {
    bytes(9, &[bytes(1, key.as_bytes()), bytes(2, &any)].concat())
}

#[test]
fn a_span_decodes_with_its_scope_attributes_and_status() {
    let mut double = vec![4 << 3 | 1];
    double.extend_from_slice(&0.5f64.to_le_bytes());
    let span = [
        bytes(1, &[7; 16]),
        bytes(4, &[1; 8]),
        bytes(5, b"chat e2e-model"),
        number(6, 3),
        // fixed64 start and end times, skipped.
        vec![7 << 3 | 1, 0, 0, 0, 0, 0, 0, 0, 1],
        attribute("gen_ai.request.model", bytes(1, b"e2e-model")),
        attribute("gen_ai.request.temperature", double),
        attribute("http.status_code", number(3, 200)),
        attribute("streaming", number(2, 1)),
        bytes(15, &[bytes(2, b"boom"), number(3, 2)].concat()),
    ]
    .concat();
    let scope = [bytes(1, &bytes(1, b"mistral_vibe")), bytes(2, &span)].concat();
    let request = bytes(1, &[bytes(1, &[]), bytes(2, &scope)].concat());
    assert_eq!(
        decode_export(&request).unwrap(),
        json!({"spans": [{
            "name": "chat e2e-model",
            "scope": "mistral_vibe",
            "kind": 3,
            "parent": true,
            "attributes": {
                "gen_ai.request.model": "e2e-model",
                "gen_ai.request.temperature": 0.5,
                "http.status_code": 200,
                "streaming": true,
            },
            "status": {"code": 2, "message": "boom"},
        }]})
    );
}

#[test]
fn bytes_that_are_not_a_message_decode_to_nothing() {
    assert_eq!(decode_export(b"{\"json\": true}\xff"), None);
    assert_eq!(decode_export(&[0x0a, 0x05, 0x01]), None);
}
