use super::*;

#[test]
fn a_child_is_named_after_its_parent_and_the_spawning_action() {
    let id = child_id("session", "action");
    assert!(id.starts_with("child-"));
    assert_eq!(id.len(), "child-".len() + 24);
    assert_eq!(id, child_id("session", "action"));
    assert_ne!(id, child_id("session", "other"));
}
