use super::*;
use serde_json::json;

#[test]
fn detail_free_errors_keep_a_null_data_key() {
    // The reference dumps `ProtocolError` without a null filter, so the key is
    // on the wire whether or not a detail exists. Dropping it would make every
    // error frame this port emits one key shorter than the reference's.
    let error = ProtocolError {
        code: ProtocolErrorCode::NotFound,
        message: "gone".to_owned(),
        data: Value::Null,
    };
    assert_eq!(
        serde_json::to_value(&error).expect("error encodes"),
        json!({"code": "not_found", "message": "gone", "data": null})
    );
    assert_eq!(
        serde_json::from_value::<ProtocolError>(json!({
            "code": "not_found", "message": "gone"
        }))
        .expect("an absent data key still decodes"),
        error
    );
}

#[test]
fn invalid_params_detail_serializes_paths_as_segments() {
    let data = InvalidParamsData {
        error_count: 1,
        issues: vec![InvalidParamsIssue {
            path: vec![
                PathSegment::Field("input".to_owned()),
                PathSegment::Index(2),
                PathSegment::Field("text".to_owned()),
            ],
            message: "invalid type: integer".to_owned(),
        }],
    };
    assert_eq!(
        serde_json::to_value(&data).expect("detail encodes"),
        json!({
            "errorCount": 1,
            "issues": [{"path": ["input", 2, "text"], "message": "invalid type: integer"}]
        })
    );
}
