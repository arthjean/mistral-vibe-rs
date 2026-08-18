use super::*;
use serde_json::json;

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
