//! Guards the one number in `docs/parity.md` that is arithmetic rather than
//! judgement.
//!
//! Every row of the parity table carries a score, and the header states a
//! weighted total together with the raw sum, the weight and the axis lists the
//! weighting comes from. That total is hand-written, so a pass that restates a
//! row and forgets the header leaves the document contradicting itself: the
//! table says one thing and the cell summarizing it says another. This module
//! recomputes the total from the table, using the weights the same cell
//! declares, and fails when the two disagree.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const SCORECARD: &str = "docs/parity.md";
/// The header cell holding the stated total, its raw sum and its weight.
const TOTAL_CELL: &str = "| Weighted score against the pin |";
/// The section whose rows carry the scores the total sums.
const TABLE_SECTION: &str = "## Parity by part";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

/// Reads `5 700` and `65` the way the cell writes them, with the thousands
/// separator this document uses.
fn number(text: &str) -> Option<u32> {
    text.replace(' ', "").parse().ok()
}

/// Every `(axis, score)` the parity table publishes.
fn scores(document: &str) -> BTreeMap<u32, u32> {
    let mut rows = BTreeMap::new();
    let mut inside = false;
    for line in document.lines() {
        if line.starts_with("## ") {
            inside = line.trim() == TABLE_SECTION;
            continue;
        }
        if !inside || !line.starts_with("| ") {
            continue;
        }
        let mut cells = line.trim_matches('|').split(" | ").map(str::trim);
        let (Some(axis), Some(_), Some(score)) = (cells.next(), cells.next(), cells.next()) else {
            continue;
        };
        let (Ok(axis), Ok(score)) = (axis.parse::<u32>(), score.parse::<u32>()) else {
            continue;
        };
        rows.insert(axis, score);
    }
    rows
}

/// The axes one `weight N for ...` clause of the total cell names, read up to
/// the semicolon that ends the clause.
fn axes(cell: &str, clause: &str) -> Vec<u32> {
    let Some(rest) = cell.split_once(clause).map(|(_, rest)| rest) else {
        return Vec::new();
    };
    let clause = rest.split(';').next().unwrap_or(rest);
    clause
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|token| token.parse().ok())
        .collect()
}

#[test]
fn the_weighted_total_is_the_sum_the_parity_table_produces() {
    let document =
        fs::read_to_string(repo_root().join(SCORECARD)).expect("the scorecard is committed");
    let cell = document
        .lines()
        .find(|line| line.starts_with(TOTAL_CELL))
        .expect("the header states a weighted total, or this parser moved");

    let stated_score = cell
        .split_once("**")
        .and_then(|(_, rest)| rest.split_once("/100**"))
        .map(|(score, _)| score.to_owned())
        .expect("the total is written as `**NN.N/100**`");
    let stated_sum = cell
        .split_once(", from ")
        .and_then(|(_, rest)| rest.split_once(" over a weight of "))
        .and_then(|(sum, rest)| {
            let weight = rest.split(' ').next()?;
            Some((number(sum)?, number(weight)?))
        })
        .expect("the total names its raw sum and its weight");
    let (stated_raw, stated_weight) = stated_sum;

    let heavy = axes(cell, "weight 3 for");
    let light = axes(cell, "weight 1 for");
    assert!(
        !heavy.is_empty() && !light.is_empty(),
        "the total cell no longer names the axes its weighting comes from"
    );

    let scores = scores(&document);
    assert!(
        !scores.is_empty(),
        "no row of `{TABLE_SECTION}` parsed, so this guard would pass on an empty table"
    );
    for axis in heavy.iter().chain(light.iter()) {
        assert!(
            scores.contains_key(axis),
            "the weighting names axis {axis}, which the parity table does not carry"
        );
    }

    let mut raw = 0;
    let mut weight = 0;
    let mut middling = 0;
    for (axis, score) in &scores {
        let factor = if heavy.contains(axis) {
            3
        } else if light.contains(axis) {
            1
        } else {
            middling += 1;
            2
        };
        raw += score * factor;
        weight += factor;
    }

    assert_eq!(
        raw, stated_raw,
        "the parity table sums to {raw} and the header says {stated_raw}: a row moved and the \
         total did not follow it"
    );
    assert_eq!(
        weight, stated_weight,
        "the parity table weighs {weight} and the header says {stated_weight}"
    );
    let computed = format!("{:.1}", f64::from(raw) / f64::from(weight));
    assert_eq!(
        computed, stated_score,
        "{raw} over {weight} is {computed} and the header states {stated_score}"
    );
    assert!(
        cell.contains(&format!("the remaining {middling}")),
        "{middling} axes carry weight 2 and the total cell does not say so"
    );

    println!(
        "parity scorecard: {} axes, {raw} over a weight of {weight}, stated as {stated_score}",
        scores.len()
    );
}
