//! US-106: the arity table and the pattern it derives.
//!
//! The differential half lives in `permission_parity_tests`, which replays the
//! committed capture. These cases pin the shape the capture cannot: that the
//! table stays sorted, which is what makes the lookup a binary search, and that
//! the two degenerate inputs answer rather than panic.

use super::*;

#[test]
fn the_table_is_sorted_and_holds_no_duplicate_prefix() {
    for pair in ARITY.windows(2) {
        let [(previous, _), (next, _)] = pair else {
            unreachable!("windows(2) yields pairs")
        };
        assert!(
            previous < next,
            "the arity table is looked up by binary search: `{previous}` precedes `{next}`"
        );
    }
}

#[test]
fn every_arity_is_at_least_one_token() {
    for (prefix, arity) in ARITY {
        assert!(arity >= 1, "`{prefix}` keeps no token");
        assert!(
            arity >= prefix.split_whitespace().count(),
            "`{prefix}` keeps fewer tokens than the prefix itself spells"
        );
    }
}

/// The longest matching prefix selects the arity, and the pattern is that many
/// leading tokens followed by ` *`.
#[test]
fn the_longest_matching_prefix_selects_the_arity() {
    assert_eq!(
        build_session_pattern(&["npm", "run", "build"]),
        "npm run build *"
    );
    assert_eq!(build_session_pattern(&["npm", "run"]), "npm run *");
    assert_eq!(build_session_pattern(&["npm"]), "npm *");
    assert_eq!(
        build_session_pattern(&["git", "config", "user.name"]),
        "git config user.name *"
    );
    assert_eq!(build_session_pattern(&["ls", "-la"]), "ls *");
}

/// A first token the table does not know falls back to that token alone.
#[test]
fn an_unknown_command_falls_back_to_its_first_token() {
    assert_eq!(
        build_session_pattern(&["unknown-binary", "--flag", "value"]),
        "unknown-binary *"
    );
    assert_eq!(build_session_pattern(&["./script.sh"]), "./script.sh *");
}

/// An empty command has no pattern, and asking for one is not a panic.
#[test]
fn an_empty_token_list_yields_an_empty_pattern() {
    assert_eq!(build_session_pattern(&[]), "");
}

/// A command shorter than the arity of its own prefix keeps what it has rather
/// than indexing past its end, which is what the reference slice does.
#[test]
fn a_command_shorter_than_its_arity_keeps_what_it_has() {
    assert_eq!(arity_of("docker compose"), Some(3));
    assert_eq!(
        build_session_pattern(&["docker", "compose"]),
        "docker compose *"
    );
}
