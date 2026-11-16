//! Decodes the OTLP trace exports both implementations post to the platform's
//! trace route (`/v1/traces`, protobuf over HTTP), so the journal holds their
//! spans as JSON instead of opaque bytes.
//!
//! Only the fields a scenario compares are read, following the field numbers
//! of `opentelemetry/proto/trace/v1/trace.proto` and
//! `opentelemetry/proto/common/v1/common.proto`; every other field is skipped
//! by its wire type.

use serde_json::{Map, Value, json};

/// `{"spans": [{"name", "scope", "kind", "parent", "attributes", "status"}]}`
/// for an `ExportTraceServiceRequest`, or [`None`] when `body` is not one.
pub(crate) fn decode_export(body: &[u8]) -> Option<Value> {
    let mut spans = Vec::new();
    for (field, value) in fields(body)? {
        if let (1, Wire::Bytes(resource_spans)) = (field, value) {
            for (field, value) in fields(resource_spans)? {
                if let (2, Wire::Bytes(scope_spans)) = (field, value) {
                    decode_scope_spans(scope_spans, &mut spans)?;
                }
            }
        }
    }
    Some(json!({"spans": spans}))
}

fn decode_scope_spans(bytes: &[u8], spans: &mut Vec<Value>) -> Option<()> {
    let mut scope = String::new();
    let mut raw = Vec::new();
    for (field, value) in fields(bytes)? {
        match (field, value) {
            (1, Wire::Bytes(instrumentation)) => {
                for (field, value) in fields(instrumentation)? {
                    if let (1, Wire::Bytes(name)) = (field, value) {
                        scope = String::from_utf8_lossy(name).into_owned();
                    }
                }
            }
            (2, Wire::Bytes(span)) => raw.push(span),
            _ => {}
        }
    }
    for span in raw {
        let mut decoded = decode_span(span)?;
        decoded["scope"] = json!(scope);
        spans.push(decoded);
    }
    Some(())
}

fn decode_span(bytes: &[u8]) -> Option<Value> {
    let mut name = String::new();
    let mut kind = 0;
    let mut parent = false;
    let mut attributes = Map::new();
    let mut status = json!({"code": 0, "message": ""});
    for (field, value) in fields(bytes)? {
        match (field, value) {
            (4, Wire::Bytes(id)) => parent = !id.is_empty(),
            (5, Wire::Bytes(text)) => name = String::from_utf8_lossy(text).into_owned(),
            (6, Wire::Varint(value)) => kind = value,
            (9, Wire::Bytes(pair)) => {
                let (key, value) = decode_key_value(pair)?;
                attributes.insert(key, value);
            }
            (15, Wire::Bytes(bytes)) => {
                for (field, value) in fields(bytes)? {
                    match (field, value) {
                        (2, Wire::Bytes(message)) => {
                            status["message"] = json!(String::from_utf8_lossy(message));
                        }
                        (3, Wire::Varint(code)) => status["code"] = json!(code),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Some(json!({
        "name": name,
        "kind": kind,
        "parent": parent,
        "attributes": attributes,
        "status": status,
    }))
}

fn decode_key_value(bytes: &[u8]) -> Option<(String, Value)> {
    let mut key = String::new();
    let mut value = Value::Null;
    for (field, wire) in fields(bytes)? {
        match (field, wire) {
            (1, Wire::Bytes(text)) => key = String::from_utf8_lossy(text).into_owned(),
            (2, Wire::Bytes(any)) => value = decode_any(any)?,
            _ => {}
        }
    }
    Some((key, value))
}

fn decode_any(bytes: &[u8]) -> Option<Value> {
    let mut value = Value::Null;
    for (field, wire) in fields(bytes)? {
        value = match (field, wire) {
            (1, Wire::Bytes(text)) => json!(String::from_utf8_lossy(text)),
            (2, Wire::Varint(flag)) => json!(flag != 0),
            // A two's-complement int64 travels as its unsigned varint.
            (3, Wire::Varint(number)) => json!(number.cast_signed()),
            (4, Wire::Fixed64(bits)) => json!(f64::from_bits(bits)),
            (5, Wire::Bytes(array)) => {
                let mut values = Vec::new();
                for (field, wire) in fields(array)? {
                    if let (1, Wire::Bytes(item)) = (field, wire) {
                        values.push(decode_any(item)?);
                    }
                }
                Value::Array(values)
            }
            (7, Wire::Bytes(raw)) => json!(raw.len()),
            _ => value,
        };
    }
    Some(value)
}

enum Wire<'a> {
    Varint(u64),
    Fixed64(u64),
    Fixed32,
    Bytes(&'a [u8]),
}

/// The `(field number, value)` pairs of one message, or [`None`] when the
/// bytes are not a well-formed message.
fn fields(mut bytes: &[u8]) -> Option<Vec<(u64, Wire<'_>)>> {
    let mut fields = Vec::new();
    while !bytes.is_empty() {
        let key = varint(&mut bytes)?;
        let wire = match key & 0x7 {
            0 => Wire::Varint(varint(&mut bytes)?),
            1 => {
                let (head, rest) = bytes.split_at_checked(8)?;
                bytes = rest;
                Wire::Fixed64(u64::from_le_bytes(head.try_into().ok()?))
            }
            2 => {
                let length = usize::try_from(varint(&mut bytes)?).ok()?;
                let (head, rest) = bytes.split_at_checked(length)?;
                bytes = rest;
                Wire::Bytes(head)
            }
            5 => {
                bytes = bytes.get(4..)?;
                Wire::Fixed32
            }
            _ => return None,
        };
        fields.push((key >> 3, wire));
    }
    Some(fields)
}

fn varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first()?;
        *bytes = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod otlp_tests;
