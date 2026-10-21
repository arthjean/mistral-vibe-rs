//! Which user resources a prompt carries, and the policy that bounds them.

use super::*;

#[test]
fn model_and_display_resources_remain_separately_typed_and_policy_bounded() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let root = temporary.path().join("project");
    fs::create_dir_all(&root).expect("project root");
    let file = root.join("note.txt");
    let image = root.join("image.png");
    fs::write(&file, "content").expect("file fixture");
    fs::write(&image, [137, 80, 78, 71]).expect("image fixture");
    let prepared = prepare_user_resources(
        &[
            UserResource {
                kind: UserResourceKind::Text,
                path: None,
                text: Some("hello".to_owned()),
                mime_type: None,
                metadata: Value::Null,
            },
            UserResource {
                kind: UserResourceKind::File,
                path: Some(file),
                text: None,
                mime_type: None,
                metadata: Value::Null,
            },
            UserResource {
                kind: UserResourceKind::Image,
                path: Some(image),
                text: None,
                mime_type: None,
                metadata: Value::Null,
            },
        ],
        std::slice::from_ref(&root),
        true,
    )
    .expect("resources prepare");
    assert_eq!(prepared.model_content.len(), 3);
    assert_eq!(prepared.display_content.len(), 3);
    assert!(matches!(
        prepared.model_content[2],
        ModelContent::Image { .. }
    ));

    let outside = temporary.path().join("outside.txt");
    fs::write(&outside, "secret").expect("outside fixture");
    assert!(matches!(
        prepare_user_resources(
            &[UserResource {
                kind: UserResourceKind::File,
                path: Some(outside),
                text: None,
                mime_type: None,
                metadata: Value::Null,
            }],
            &[root],
            true,
        ),
        Err(PromptError::OutOfPolicyPath(_))
    ));
}
