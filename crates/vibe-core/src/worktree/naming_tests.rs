//! The naming rules the corpus cannot pin: a random slug differs on every
//! call, so the replay only records that one was drawn. Its shape is held here.

use super::{is_portable_worktree_name, random_slug, worktree_name_from_text};

#[test]
fn a_random_slug_is_two_distinct_adjectives_and_a_noun() {
    for _ in 0..64 {
        let slug = random_slug();
        let words = slug.split('-').collect::<Vec<_>>();
        assert_eq!(words.len(), 3, "{slug}");
        assert_ne!(words[0], words[1], "{slug}");
        assert!(is_portable_worktree_name(&slug), "{slug}");
        assert_eq!(worktree_name_from_text(&slug), slug, "{slug}");
    }
}

#[test]
fn random_slugs_vary_between_calls() {
    let drawn = (0..32)
        .map(|_| random_slug())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(drawn.len() > 1, "every draw answered {drawn:?}");
}
