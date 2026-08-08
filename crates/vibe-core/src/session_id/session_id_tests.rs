use super::*;

/// US-158: the shape is the reference's, and every segment is hexadecimal, so
/// an identifier this port mints validates wherever a stored one does.
#[test]
fn a_minted_identifier_is_uuid_shaped() {
    let minted = generate_session_id(None);
    let segments: Vec<&str> = minted.split('-').collect();
    assert_eq!(
        segments
            .iter()
            .map(|segment| segment.len())
            .collect::<Vec<_>>(),
        vec![8, 4, 4, 4, 12],
        "{minted}"
    );
    assert!(
        minted
            .chars()
            .all(|character| character.is_ascii_hexdigit() || character == '-'),
        "{minted}"
    );
}

/// US-158: the trailing segment is the identity a rotation carries over, and
/// two rotations of the same session differ only in their heads.
#[test]
fn a_rotation_keeps_the_trailing_segment_and_refreshes_the_head() {
    let original = generate_session_id(None);
    let suffix = extract_suffix(&original).to_owned();

    let first = rotate_session_id(&original);
    let second = rotate_session_id(&first);

    assert_eq!(extract_suffix(&first), suffix);
    assert_eq!(extract_suffix(&second), suffix);
    assert_ne!(first, second, "two rotations mint two identifiers");
    assert_ne!(
        first.rsplit_once('-').map(|(head, _)| head),
        second.rsplit_once('-').map(|(head, _)| head),
        "the heads differ"
    );
}

/// US-158: an identifier this port did not mint still rotates, because the
/// right split the reference performs has no shape requirement.
#[test]
fn an_identifier_without_the_shape_still_yields_a_suffix() {
    assert_eq!(extract_suffix("client-named-session"), "session");
    assert_eq!(extract_suffix("bare"), "bare");
    assert_eq!(extract_suffix(""), "");
    assert!(rotate_session_id("bare").ends_with("-bare"));
}

/// US-158: an identifier whose trailing segment is empty rotates onto a fresh
/// one rather than onto nothing, which is what the reference's
/// `suffix or token_hex(6)` does with the same input.
#[test]
fn an_empty_suffix_is_replaced_rather_than_carried() {
    let rotated = rotate_session_id("trailing-");
    assert!(!rotated.ends_with('-'), "{rotated}");
    assert_eq!(extract_suffix(&rotated).len(), 12, "{rotated}");
}
