//! The properties the opcode contract rests on, independent of the corpus.
//!
//! The corpus proves this matcher answers what the reference answered on the
//! fixtures it captured. These assert the shape of the answer, so a divergence
//! that the fixtures happen not to reach still fails.

use super::matcher::{Match, SequenceMatcher, Tag};

fn lines(text: &str) -> Vec<String> {
    super::lines::split_lines(text)
}

fn tuples(a: &[String], b: &[String]) -> Vec<(&'static str, usize, usize, usize, usize)> {
    SequenceMatcher::new(a, b)
        .opcodes()
        .into_iter()
        .map(|opcode| {
            (
                opcode.tag.as_str(),
                opcode.i1,
                opcode.i2,
                opcode.j1,
                opcode.j2,
            )
        })
        .collect()
}

#[test]
fn identical_sequences_produce_one_equal_opcode() {
    let a = lines("one\ntwo\nthree\n");
    assert_eq!(tuples(&a, &a), vec![("equal", 0, 3, 0, 3)]);
}

#[test]
fn an_empty_side_produces_a_single_insert_or_delete() {
    let empty: Vec<String> = Vec::new();
    let full = lines("one\ntwo\n");
    assert_eq!(tuples(&empty, &full), vec![("insert", 0, 0, 0, 2)]);
    assert_eq!(tuples(&full, &empty), vec![("delete", 0, 2, 0, 0)]);
}

#[test]
fn two_sequences_sharing_no_line_produce_a_single_replace() {
    let a = lines("alpha\nbeta\n");
    let b = lines("gamma\ndelta\nepsilon\n");
    assert_eq!(tuples(&a, &b), vec![("replace", 0, 2, 0, 3)]);
}

#[test]
fn two_empty_sequences_produce_no_opcode_at_all() {
    let empty: Vec<String> = Vec::new();
    assert!(tuples(&empty, &empty).is_empty());
}

#[test]
fn a_shared_prefix_and_suffix_frame_one_replace() {
    let a = lines("head\nmiddle\ntail\n");
    let b = lines("head\nchanged\ntail\n");
    assert_eq!(
        tuples(&a, &b),
        vec![
            ("equal", 0, 1, 0, 1),
            ("replace", 1, 2, 1, 2),
            ("equal", 2, 3, 2, 3),
        ]
    );
}

#[test]
fn the_spans_tile_both_sequences_and_no_two_neighbours_share_a_tag() {
    let a = lines("one\ntwo\nthree\nfour\nfive\n");
    let b = lines("one\ntwo prime\nthree\nfive\nsix\n");
    let opcodes = SequenceMatcher::new(&a, &b).opcodes();

    let (mut i, mut j) = (0, 0);
    let mut previous: Option<Tag> = None;
    for opcode in &opcodes {
        assert_eq!((opcode.i1, opcode.j1), (i, j), "a span left a gap");
        assert!(opcode.i2 >= opcode.i1 && opcode.j2 >= opcode.j1);
        match opcode.tag {
            Tag::Equal => assert_eq!(opcode.i2 - opcode.i1, opcode.j2 - opcode.j1),
            Tag::Replace => assert!(opcode.i2 > opcode.i1 && opcode.j2 > opcode.j1),
            Tag::Delete => assert!(opcode.i2 > opcode.i1 && opcode.j2 == opcode.j1),
            Tag::Insert => assert!(opcode.i2 == opcode.i1 && opcode.j2 > opcode.j1),
        }
        // Two non-equal steps in a row would let one change carry two ordinals,
        // which would split a region identity in half.
        assert!(
            previous != Some(opcode.tag) || opcode.tag == Tag::Equal,
            "two adjacent {} opcodes",
            opcode.tag.as_str()
        );
        assert!(
            previous.is_none_or(|tag| tag == Tag::Equal || opcode.tag == Tag::Equal),
            "two non-equal opcodes in a row"
        );
        previous = Some(opcode.tag);
        i = opcode.i2;
        j = opcode.j2;
    }
    assert_eq!(
        (i, j),
        (a.len(), b.len()),
        "the spans left a tail uncovered"
    );
}

#[test]
fn the_matching_blocks_end_with_the_sentinel() {
    let a = lines("one\ntwo\n");
    let b = lines("one\ntwo\nthree\n");
    let blocks = SequenceMatcher::new(&a, &b).matching_blocks();
    assert_eq!(
        blocks.last().copied(),
        Some(Match {
            a: 2,
            b: 3,
            size: 0
        })
    );
    assert_eq!(
        blocks.first().copied(),
        Some(Match {
            a: 0,
            b: 0,
            size: 2
        })
    );
}

#[test]
fn the_sentinel_is_the_whole_answer_when_nothing_matches() {
    let a = lines("alpha\n");
    let b = lines("beta\n");
    assert_eq!(
        SequenceMatcher::new(&a, &b).matching_blocks(),
        vec![Match {
            a: 1,
            b: 1,
            size: 0
        }]
    );
}

/// The one construction that tells the heuristic apart from its absence.
///
/// CPython drops any element of `b` occurring more than `len(b) / 100 + 1`
/// times once `b` reaches 200 elements. Here the only line the two sides share
/// occurs five times in a sequence of 250, so the heuristic would purge it and
/// report the whole file as one replace. The reference passes `autojunk=False`,
/// so the line matches.
#[test]
fn a_popular_line_above_the_threshold_still_matches() {
    let mut b = Vec::new();
    for index in 0..250 {
        if index % 50 == 10 {
            b.push("shared\n".to_owned());
        } else {
            b.push(format!("body {index}\n"));
        }
    }
    assert_eq!(b.iter().filter(|line| *line == "shared\n").count(), 5);
    assert!(b.len() >= 200 && 5 > b.len() / 100 + 1);

    let a = lines("first\nshared\nlast\n");
    let opcodes = SequenceMatcher::new(&a, &b).opcodes();
    let equal: Vec<_> = opcodes
        .iter()
        .filter(|opcode| opcode.tag == Tag::Equal)
        .collect();
    assert_eq!(
        equal.len(),
        1,
        "the popular line was purged, so the heuristic is back: {opcodes:?}"
    );
    assert_eq!((equal[0].i1, equal[0].i2), (1, 2));
    assert_eq!(a[equal[0].i1], "shared\n");
    assert_eq!(b[equal[0].j1], "shared\n");
}

#[test]
fn the_earliest_longest_run_wins_a_tie() {
    // "same" occurs twice in each side; the leftmost pair is the match, which is
    // what makes the ordinal of a later region stable.
    let a = lines("same\nx\nsame\n");
    let b = lines("same\ny\nsame\n");
    assert_eq!(
        tuples(&a, &b),
        vec![
            ("equal", 0, 1, 0, 1),
            ("replace", 1, 2, 1, 2),
            ("equal", 2, 3, 2, 3),
        ]
    );
}

#[test]
fn two_runs_are_reported_once_when_they_touch() {
    // Recursion can find "one" and "two" separately; collapsing them keeps the
    // opcode list canonical.
    let a = lines("one\ntwo\ntail\n");
    let b = lines("one\ntwo\nother\n");
    assert_eq!(
        tuples(&a, &b),
        vec![("equal", 0, 2, 0, 2), ("replace", 2, 3, 2, 3),]
    );
}

#[test]
fn the_matcher_is_deterministic_across_runs() {
    let a = lines("alpha\nbeta\ngamma\nbeta\ndelta\n");
    let b = lines("beta\nalpha\nbeta\ngamma\nomega\n");
    let first = tuples(&a, &b);
    for _ in 0..8 {
        assert_eq!(tuples(&a, &b), first);
    }
}
