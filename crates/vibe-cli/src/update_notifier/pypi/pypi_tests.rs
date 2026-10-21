use super::*;

#[test]
fn pypi_selection_skips_yanked_and_unpublished_versions() {
    let payload = serde_json::json!({
        "versions": ["2.23.1", "2.24.0", "2.25.0", "nonsense"],
        "files": [
            {"filename": "mistral_vibe-2.23.1-py3-none-any.whl", "yanked": false},
            {"filename": "mistral_vibe-2.24.0-py3-none-any.whl", "yanked": true},
            {"filename": "mistral_vibe-2.24.0.tar.gz", "yanked": true}
        ]
    });
    assert_eq!(
        select_latest_version(&payload).as_deref(),
        Some("2.23.1"),
        "a yanked artifact and an unpublished version are both ignored"
    );
    assert_eq!(select_latest_version(&serde_json::json!({})), None);
}
