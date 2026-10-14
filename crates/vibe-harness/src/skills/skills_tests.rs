use serde_json::json;

use super::*;

const AVAILABLE: [&str; 4] = [
    "vibe:create-plugin",
    "vibe:skill-creator",
    "vibe:vibe",
    "vibe:worktree",
];

#[test]
fn an_exact_name_passes() {
    assert_eq!(
        check_request(&AVAILABLE, &json!({"name": "vibe:vibe"})),
        Ok(())
    );
}

#[test]
fn an_unqualified_name_is_refused_with_the_names_that_hold_it() {
    let refusal = check_request(&AVAILABLE, &json!({"name": "vibe"})).unwrap_err();
    assert!(refusal.starts_with(
        "Skill \"vibe\" is not available. Did you mean: \"vibe:create-plugin\", \
         \"vibe:skill-creator\", \"vibe:vibe\"?"
    ));
    assert!(refusal.ends_with(
        "Available skills: \"vibe:create-plugin\", \"vibe:skill-creator\", \"vibe:vibe\", \
         \"vibe:worktree\"."
    ));
}

#[test]
fn a_blank_name_is_refused() {
    assert_eq!(
        check_request(&AVAILABLE, &json!({"name": " "})),
        Err("skill name must be a non-empty string".to_owned())
    );
}

#[test]
fn edit_distance_counts_single_edits() {
    assert_eq!(edit_distance(b"kitten", b"sitting"), 3);
    assert!((similarity("abc", "abd") - (1.0 - 1.0 / 3.0)).abs() < 1e-9);
}
