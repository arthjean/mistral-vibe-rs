//! Guards the one number in `docs/parity.md` that is arithmetic rather than
//! judgement.
//!
//! Every row of the parity table carries a score, and the header states a
//! weighted Parity and Conformance together with their raw sums, the weight,
//! the axis lists the weighting comes from, the commit measured and the newest
//! upstream tag the census recorded. That total is hand-written, so a pass that restates a
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
    let stated_conformance = cell
        .split_once("/100** Parity and **")
        .and_then(|(_, rest)| rest.split_once("/100** Conformance"))
        .map(|(score, _)| score.to_owned())
        .expect("the total states its Conformance as `**NN.N/100** Conformance`");
    let stated_sum = cell
        .split_once(", from ")
        .and_then(|(_, rest)| rest.split_once(" over a weight of "))
        .and_then(|(sums, rest)| {
            let (parity, conformance) = sums.split_once(" and ")?;
            let weight = rest.split([' ', ',']).next()?;
            Some((number(parity)?, number(conformance)?, number(weight)?))
        })
        .expect("the total names its Parity and Conformance sums and its weight");
    let (stated_raw, stated_conformant, stated_weight) = stated_sum;
    let measured = format!(
        "measured at `{}`",
        super::figures::abbreviate(super::REFERENCE_COMMIT)
    );
    assert!(
        cell.contains(&measured),
        "the total does not say it was {measured}, the pin"
    );
    let census: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo_root().join(CENSUS)).expect("the upstream census is committed"),
    )
    .expect("the upstream census parses");
    let newest = census["newest"]["tag"]
        .as_str()
        .expect("the census names the newest upstream tag");
    assert!(
        cell.contains(&format!("the newest upstream tag `{newest}`")),
        "the total does not name `{newest}`, the newest upstream tag {CENSUS} records"
    );

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
    let conformance = stated_scores(&document);
    for axis in heavy.iter().chain(light.iter()) {
        assert!(
            scores.contains_key(axis),
            "the weighting names axis {axis}, which the parity table does not carry"
        );
    }

    let mut raw = 0;
    let mut conformant = 0;
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
        // A structural row measured nothing, so it conforms to nothing.
        let measured = conformance
            .get(axis)
            .and_then(|(_, cell)| cell.parse::<u32>().ok())
            .unwrap_or(0);
        conformant += measured * factor;
        weight += factor;
    }
    assert_eq!(
        conformant, stated_conformant,
        "the parity table's Conformance sums to {conformant} and the header says \
         {stated_conformant}"
    );
    let computed_conformance = format!("{:.1}", f64::from(conformant) / f64::from(weight));
    assert_eq!(
        computed_conformance, stated_conformance,
        "{conformant} over {weight} is {computed_conformance} and the header states \
         {stated_conformance} Conformance"
    );

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

/// The header row that states how far the pin is behind upstream.
const UPSTREAM_CELL: &str = "| Weighted score against upstream `HEAD` |";
/// What `scripts/parity/repin.py census` last recorded.
const CENSUS: &str = "scripts/parity/upstream-census.json";

/// `v3.1.0` as the numbers a release compares by.
fn release(tag: &str) -> Option<(u32, u32, u32)> {
    let mut parts = tag.strip_prefix('v')?.split('.').map(str::parse::<u32>);
    let release = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    parts.next().is_none().then_some(release)
}

#[test]
fn the_upstream_row_names_the_newest_tag_the_census_recorded() {
    let root = repo_root();
    let document = fs::read_to_string(root.join(SCORECARD)).expect("the scorecard is committed");
    let census: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join(CENSUS)).expect("the upstream census is committed"),
    )
    .expect("the upstream census parses");
    let newest = census["newest"]["tag"]
        .as_str()
        .expect("the census names the newest upstream tag");
    let cell = document
        .lines()
        .find(|line| line.starts_with(UPSTREAM_CELL))
        .expect("the header states the score against upstream, or this parser moved");
    let named = cell
        .split_once("newest upstream tag `")
        .and_then(|(_, rest)| rest.split_once('`'))
        .map(|(tag, _)| tag)
        .expect("the upstream row names the newest upstream tag as `vX.Y.Z`");
    let (Some(named_release), Some(newest_release)) = (release(named), release(newest)) else {
        panic!("{named} and {newest} are not both release tags");
    };
    assert!(
        named_release >= newest_release,
        "the upstream row names {named}, older than the newest tag {newest} that {CENSUS} records; \
         rerun `python3 scripts/parity/repin.py census`"
    );
    let summary = census["summary"]
        .as_str()
        .expect("the census states its summary");
    assert!(
        cell.contains(summary),
        "the upstream row does not state the census diff `{summary}`; rerun `python3 \
         scripts/parity/repin.py census`"
    );
}

/// Every row's `(Parity, Conformance)` cells, as the parity table writes
/// them: Parity a whole number, Conformance a whole number or `structural`.
fn stated_scores(document: &str) -> BTreeMap<u32, (u32, String)> {
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
        let cells: Vec<&str> = line.trim_matches('|').split(" | ").map(str::trim).collect();
        let (Some(axis), Some(parity), Some(conformance)) = (
            cells.first().and_then(|cell| cell.parse::<u32>().ok()),
            cells.get(2).and_then(|cell| cell.parse::<u32>().ok()),
            cells.get(3),
        ) else {
            continue;
        };
        rows.insert(axis, (parity, (*conformance).to_owned()));
    }
    rows
}

/// The ceiling a row that no replay measures may claim (US-011 of
/// `tasks/prd-trustworthy-parity-v3.md`).
const STRUCTURAL_CEILING: u32 = 50;

#[test]
fn every_row_states_the_parity_and_conformance_its_figures_derive() {
    use super::figures::{self, Figures};

    let document =
        fs::read_to_string(repo_root().join(SCORECARD)).expect("the scorecard is committed");
    let stated = stated_scores(&document);
    assert!(
        !stated.is_empty(),
        "no row of `{TABLE_SECTION}` states a Parity and a Conformance, or this parser moved"
    );
    let lines = figures::lines(&document);
    let accepted = super::pin_commits()
        .into_iter()
        .map(|commit| figures::abbreviate(commit).to_owned())
        .collect::<Vec<_>>();
    let mut keys = std::collections::BTreeSet::new();
    for line in &lines {
        assert!(
            keys.insert((line.heading.clone(), line.platform.clone())),
            "`{}` is declared twice for {}",
            line.heading,
            line.platform
        );
        assert!(
            ["all", "linux", "windows", "macos"].contains(&line.platform.as_str()),
            "`{}` names the platform {}, which no host is",
            line.heading,
            line.platform
        );
        assert!(
            accepted.contains(&line.commit),
            "`{}` was measured at {}, which the manifest no longer accepts",
            line.heading,
            line.commit
        );
        let numbers = line.figures;
        assert!(
            numbers.conformant + numbers.accepted <= numbers.total,
            "`{}` counts more conformant and accepted units than it compared",
            line.heading
        );
        let row = line.row.parse::<u32>().ok();
        assert!(
            row.is_some_and(|row| stated.contains_key(&row)),
            "`{}` measures row {}, which the parity table does not carry",
            line.heading,
            line.row
        );
    }
    let derived = figures::by_row(&lines);
    let mut mismatches = Vec::new();
    for (row, (parity, conformance)) in &stated {
        match derived.get(&row.to_string()) {
            Some(numbers) => {
                let Figures { total, .. } = *numbers;
                let (want_parity, want_conformance) = (numbers.parity(), numbers.conformance());
                if total == 0
                    || *parity as usize != want_parity
                    || conformance.parse::<usize>().ok() != Some(want_conformance)
                {
                    mismatches.push(format!(
                        "row {row} states Parity {parity} and Conformance {conformance}; its \
                         figures derive {want_parity} and {want_conformance}"
                    ));
                }
            }
            None => {
                if conformance != "structural" || *parity > STRUCTURAL_CEILING {
                    mismatches.push(format!(
                        "row {row} has no line under \"{}\", so it rests on a structural \
                         judgment: its Conformance reads `structural` and its Parity is at most \
                         {STRUCTURAL_CEILING}, not {parity} and {conformance}",
                        figures::FIGURES_SECTION
                    ));
                }
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "the parity table disagrees with its figures:\n  {}",
        mismatches.join("\n  ")
    );
}

/// The subsection of `## Parity by part` that lists the reference modules no
/// row measures, one path and its reason per line.
const OUT_OF_SCOPE: &str = "### Reference modules out of scope";

/// Claims so wide they name a tree rather than a module, so they claim
/// nothing: a row has to say which part of the tree it measures.
const TOO_BROAD: [&str; 4] = [
    "vibe/",
    "vibe/core/",
    "vibe/cli-rust/",
    "vibe/cli-rust/src/",
];

/// Where the reference's terminal client keeps its top-level modules.
const CLIENT_SOURCES: &str = "vibe/cli-rust/src/";

/// What the scorecard says about the reference tree: every path a row's
/// Reference cell cites, by row, and every path the out-of-scope list names,
/// with its reason.
struct Claims {
    rows: BTreeMap<u32, Vec<String>>,
    out_of_scope: Vec<(String, String)>,
}

fn claims(document: &str) -> Claims {
    let mut rows = BTreeMap::new();
    let mut out_of_scope = Vec::new();
    let mut inside = false;
    let mut listing = false;
    for line in document.lines() {
        if line.starts_with("## ") {
            inside = line.trim() == TABLE_SECTION;
            listing = false;
            continue;
        }
        if line.starts_with("### ") {
            listing = inside && line.trim() == OUT_OF_SCOPE;
            continue;
        }
        if !inside || !line.starts_with("| ") {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split(" | ").map(str::trim).collect();
        if listing {
            let path = cells
                .first()
                .and_then(|cell| cell.strip_prefix('`'))
                .and_then(|cell| cell.strip_suffix('`'));
            if let (Some(path), Some(reason)) = (path, cells.get(1)) {
                out_of_scope.push((path.to_owned(), (*reason).to_owned()));
            }
            continue;
        }
        let (Some(axis), Some(reference)) = (
            cells.first().and_then(|cell| cell.parse::<u32>().ok()),
            cells.get(4),
        ) else {
            continue;
        };
        let cited = reference
            .split('`')
            .skip(1)
            .step_by(2)
            // `file.py:120-140` cites lines of the file the claim is on.
            .map(|path| path.split(':').next().unwrap_or(path).to_owned())
            .collect();
        rows.insert(axis, cited);
    }
    Claims { rows, out_of_scope }
}

/// Whether `claim` names `path` or a directory `path` sits in.
fn covers(claim: &str, path: &str) -> bool {
    !TOO_BROAD.contains(&claim)
        && (claim == path || (claim.ends_with('/') && path.starts_with(claim)))
}

/// The top-level module of the terminal client a reference path belongs to,
/// as `ui` for both `vibe/cli-rust/src/ui.rs` and `vibe/cli-rust/src/ui/x.rs`.
fn client_module(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(CLIENT_SOURCES)?;
    let head = rest.split('/').next()?;
    let module = head.strip_suffix(".rs").unwrap_or(head);
    (!module.is_empty()).then_some(module)
}

/// The modules of `python` (paths of `.py` files under `vibe/`) and `client`
/// (top-level module names of the terminal client) that no row and no
/// out-of-scope line claims.
fn unclaimed(claims: &Claims, python: &[String], client: &[String]) -> Vec<String> {
    let named: Vec<&str> = claims
        .rows
        .values()
        .flatten()
        .map(String::as_str)
        .chain(claims.out_of_scope.iter().map(|(path, _)| path.as_str()))
        .collect();
    let mut missing: Vec<String> = python
        .iter()
        .filter(|module| !named.iter().any(|claim| covers(claim, module)))
        .cloned()
        .collect();
    missing.extend(
        client
            .iter()
            .filter(|module| {
                !named
                    .iter()
                    .any(|claim| client_module(claim) == Some(module.as_str()))
            })
            .map(|module| format!("{CLIENT_SOURCES}{module}")),
    );
    missing
}

/// Every `.py` file under `vibe/` outside the terminal client, and every
/// top-level module of the terminal client, in the tree at `source`.
fn reference_modules(source: &Path) -> (Vec<String>, Vec<String>) {
    fn walk(source: &Path, directory: &Path, found: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(source) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if relative.starts_with("vibe/cli-rust") {
                continue;
            }
            if path.is_dir() {
                walk(source, &path, found);
            } else if relative.ends_with(".py") {
                found.push(relative);
            }
        }
    }
    let mut python = Vec::new();
    walk(source, &source.join("vibe"), &mut python);
    python.sort();
    let mut client: Vec<String> = fs::read_dir(source.join(CLIENT_SOURCES))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rust = Path::new(&name)
                .extension()
                .is_some_and(|extension| extension == "rs");
            let module = name.strip_suffix(".rs").unwrap_or(&name).to_owned();
            (entry.path().is_dir() || rust).then_some(module)
        })
        .collect();
    client.sort();
    client.dedup();
    (python, client)
}

#[test]
fn a_module_is_claimed_by_a_row_or_listed_out_of_scope() {
    let document = "\
## Parity by part

| # | Part | Parity | Conformance | Reference | State |
|---|---|---|---|---|---|
| 1 | One | 0 | structural | `vibe/core/tools/`, `vibe/cli-rust/src/ui/mod.rs`, `vibe/core/types.py:12-40` | x |
| 2 | Two | 0 | structural | `vibe/`, `vibe/cli-rust/src/` | x |

### Reference modules out of scope

| Reference path | Why no row measures it |
|---|---|
| `vibe/__init__.py` | A package marker |

## Measured figures
";
    let claims = claims(document);
    assert_eq!(claims.out_of_scope.len(), 1);
    assert_eq!(claims.rows[&1][2], "vibe/core/types.py");
    let python = [
        "vibe/__init__.py",
        "vibe/core/tools/base.py",
        "vibe/core/types.py",
        "vibe/core/loop.py",
    ]
    .map(str::to_owned);
    let client = ["ui", "keymap"].map(str::to_owned);
    assert_eq!(
        unclaimed(&claims, &python, &client),
        ["vibe/core/loop.py", "vibe/cli-rust/src/keymap"],
        "a claim on `vibe/` or on the whole client tree claims nothing"
    );
}

#[test]
fn every_reference_module_is_claimed_and_every_cited_path_exists_at_the_pin() {
    let Some(root) = super::live::gate_native("scorecard coverage of the reference tree") else {
        return;
    };
    let source = super::live::source_tree(&root);
    let document =
        fs::read_to_string(repo_root().join(SCORECARD)).expect("the scorecard is committed");
    let claims = claims(&document);
    let (python, client) = reference_modules(&source);
    assert!(
        python.len() > 100 && client.len() > 50,
        "the tree at {} holds {} Python modules and {} client modules, so this walk read the \
         wrong directory",
        source.display(),
        python.len(),
        client.len()
    );

    let missing = unclaimed(&claims, &python, &client);
    assert!(
        missing.is_empty(),
        "no row of `{TABLE_SECTION}` claims these reference modules and `{OUT_OF_SCOPE}` does \
         not list them with a reason:\n  {}",
        missing.join("\n  ")
    );

    let mut absent = Vec::new();
    for (row, cited) in &claims.rows {
        for path in cited {
            if !source.join(path.trim_end_matches('/')).exists() {
                absent.push(format!("row {row} cites `{path}`"));
            }
        }
    }
    for (path, reason) in &claims.out_of_scope {
        if !source.join(path.trim_end_matches('/')).exists() {
            absent.push(format!("`{OUT_OF_SCOPE}` lists `{path}`"));
        }
        assert!(
            !reason.is_empty(),
            "`{path}` is out of scope without a reason"
        );
    }
    assert!(
        absent.is_empty(),
        "these reference paths do not exist at {}:\n  {}",
        &super::REFERENCE_COMMIT[..12],
        absent.join("\n  ")
    );
    println!(
        "scorecard coverage: {} Python modules and {} client modules, all claimed",
        python.len(),
        client.len()
    );
}
