//! The figures every replay declares in `docs/parity.md` and asserts itself.
//!
//! A row of the scorecard used to carry a score someone typed after reading a
//! summary line, so a replay could change what it measured without the row
//! moving. Every replay now ends by counting what it compared, scenario by
//! scenario, and asserting those counts equal its line of the "Measured
//! figures" table: a change in what the port answers, in the corpus, or in the
//! ledger fails the replay until the line is restated, and `scorecard_tests`
//! derives each row's Parity and Conformance from those lines rather than from
//! prose.
//!
//! The line is keyed by the replay's heading, so a replay run on its own
//! (`cargo test <filter>`) checks its own line and nothing else. Every replay
//! also appends what it measured to `target/parity/figures.jsonl`, entry by
//! entry, so one run of the suite on a host gives every line to restate.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the figures are test machinery: a check that fails is the calling test failing"
)]

use std::collections::BTreeSet;
use std::fmt;

use serde_json::{Value, json};

use super::manifest;
use super::replay::repository;

/// The scorecard, relative to the workspace root.
pub const SCORECARD: &str = "docs/parity.md";

/// The section of [`SCORECARD`] that holds one line per replay.
pub const FIGURES_SECTION: &str = "## Measured figures";

/// The section of [`SCORECARD`] whose rows a ledger entry may stand on.
pub const ACCEPTED_SECTION: &str = "## Accepted divergences";

/// The record every replay appends what it measured to.
pub const RECORD: &str = "figures.jsonl";

/// What one replay counted.
///
/// A unit is whatever the replay compares as one observation: a scenario, a
/// case, a comparison. A unit is conformant when the port answers it exactly
/// as the corpus records, and accepted when every difference it holds falls
/// under a ledger entry that stands on a decided divergence (an accepted
/// divergence or an own-words prose entry). The differences are counted by
/// the class of the entry that covered them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Figures {
    pub conformant: usize,
    pub accepted: usize,
    pub total: usize,
    pub accepted_differences: usize,
    pub gap_differences: usize,
    pub prose_differences: usize,
}

impl Figures {
    /// The figures of a replay that keeps its own ledger, whose every unit
    /// that is not conformant differs only where an accepted divergence says
    /// it may (the replay has already failed otherwise), by
    /// `accepted_differences` differences in all.
    #[must_use]
    pub fn decided(conformant: usize, total: usize, accepted_differences: usize) -> Self {
        Self {
            conformant,
            accepted: total.saturating_sub(conformant),
            total,
            accepted_differences,
            gap_differences: 0,
            prose_differences: 0,
        }
    }

    /// Parity, in whole percent rounded down: the units conformant or
    /// differing only by decided divergences, over every unit.
    #[must_use]
    pub fn parity(self) -> usize {
        percent(self.conformant + self.accepted, self.total)
    }

    /// Conformance, in whole percent rounded down: the conformant units over
    /// every unit.
    #[must_use]
    pub fn conformance(self) -> usize {
        percent(self.conformant, self.total)
    }

    fn add(self, other: Self) -> Self {
        Self {
            conformant: self.conformant + other.conformant,
            accepted: self.accepted + other.accepted,
            total: self.total + other.total,
            accepted_differences: self.accepted_differences + other.accepted_differences,
            gap_differences: self.gap_differences + other.gap_differences,
            prose_differences: self.prose_differences + other.prose_differences,
        }
    }
}

/// `part` over `whole` in whole percent, rounded down so only a whole
/// measurement reads 100.
#[must_use]
pub fn percent(part: usize, whole: usize) -> usize {
    (part * 100).checked_div(whole).unwrap_or(0)
}

/// One line of the "Measured figures" table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The heading the replay's summary opens with, which keys the line.
    pub heading: String,
    /// The host the figures hold on: `all`, or one platform (`linux`,
    /// `windows`) when a replay measures something else there, such as a
    /// subset of its surfaces. Scores derive from the lines that hold on
    /// Linux, where the corpora are captured.
    pub platform: String,
    /// The scorecard row the replay measures.
    pub row: String,
    /// The reference commit the corpus was captured from, abbreviated.
    pub commit: String,
    pub figures: Figures,
}

impl fmt::Display for Line {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let figures = self.figures;
        write!(
            formatter,
            "| `{}` | {} | {} | `{}` | {} | {} | {} | {} | {} | {} |",
            self.heading,
            self.platform,
            self.row,
            self.commit,
            figures.conformant,
            figures.accepted,
            figures.total,
            figures.accepted_differences,
            figures.gap_differences,
            figures.prose_differences
        )
    }
}

/// The lines of `document`'s "Measured figures" table, in order.
///
/// # Panics
///
/// When a line of the table does not parse, so a typo cannot drop a replay's
/// figures from the scores derived from them.
#[must_use]
pub fn lines(document: &str) -> Vec<Line> {
    section(document, FIGURES_SECTION)
        .filter(|line| line.starts_with("| `"))
        .map(|line| parse(line).unwrap_or_else(|| panic!("{SCORECARD}: unreadable line `{line}`")))
        .collect()
}

fn parse(line: &str) -> Option<Line> {
    let cells: Vec<&str> = line
        .trim()
        .strip_prefix('|')?
        .strip_suffix('|')?
        .split('|')
        .map(str::trim)
        .collect();
    let [heading, platform, row, commit, numbers @ ..] = cells.as_slice() else {
        return None;
    };
    let unquote = |cell: &str| -> Option<String> {
        Some(cell.strip_prefix('`')?.strip_suffix('`')?.to_owned())
    };
    let numbers: Vec<usize> = numbers
        .iter()
        .map(|cell| cell.parse().ok())
        .collect::<Option<_>>()?;
    let [
        conformant,
        accepted,
        total,
        accepted_differences,
        gap_differences,
        prose_differences,
    ] = numbers.as_slice()
    else {
        return None;
    };
    Some(Line {
        heading: unquote(heading)?,
        platform: (*platform).to_owned(),
        row: (*row).to_owned(),
        commit: unquote(commit)?,
        figures: Figures {
            conformant: *conformant,
            accepted: *accepted,
            total: *total,
            accepted_differences: *accepted_differences,
            gap_differences: *gap_differences,
            prose_differences: *prose_differences,
        },
    })
}

/// The lines of `document` under the `## ` heading `title`, up to the next
/// one.
pub fn section<'d>(document: &'d str, title: &'d str) -> impl Iterator<Item = &'d str> {
    document
        .lines()
        .skip_while(move |line| line.trim() != title)
        .skip(1)
        .take_while(|line| !line.starts_with("## "))
}

/// The names of the decided divergences `document` carries: the first cell
/// of every row of its "Accepted divergences" table.
#[must_use]
pub fn accepted_divergences(document: &str) -> BTreeSet<&str> {
    section(document, ACCEPTED_SECTION)
        .filter_map(|line| line.strip_prefix("| "))
        .filter_map(|line| line.split_once(" |"))
        .map(|(name, _)| name.trim())
        .filter(|name| !name.is_empty() && !name.starts_with("---") && *name != "Part")
        .collect()
}

/// The platform value a line that holds on every host carries.
pub const ALL: &str = "all";

/// Whether `line` holds on `host`, a platform as [`std::env::consts::OS`]
/// names it.
#[must_use]
pub fn holds_on(line: &Line, host: &str) -> bool {
    line.platform == ALL || line.platform == host
}

/// Every row's figures summed over the lines that measure it on Linux.
#[must_use]
pub fn by_row(lines: &[Line]) -> std::collections::BTreeMap<String, Figures> {
    let mut rows = std::collections::BTreeMap::new();
    for line in lines.iter().filter(|line| holds_on(line, "linux")) {
        let entry: &mut Figures = rows.entry(line.row.clone()).or_default();
        *entry = entry.add(line.figures);
    }
    rows
}

/// The scorecard as committed.
#[must_use]
pub fn scorecard() -> String {
    std::fs::read_to_string(repository().join(SCORECARD)).expect("the scorecard is committed")
}

/// The abbreviation the table writes a commit with.
#[must_use]
pub fn abbreviate(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// Records `figures`, what the replay opening with `heading` counted over the
/// corpus at `corpus` (a workspace-relative path the manifest lists), with
/// `detail` (what each ledger entry covered, and anything unexplained), to
/// `target/parity/figures.jsonl`, and returns the line they make.
///
/// The row and the commit come from the manifest entry, so a line naming
/// another row or a stale commit fails [`check`].
#[must_use]
pub fn record(heading: &str, corpus: &str, figures: Figures, detail: Value) -> Line {
    let manifest = manifest::load();
    let listing = manifest
        .entry(corpus)
        .unwrap_or_else(|| panic!("{corpus} is not listed in {}", manifest::MANIFEST));
    let commit = listing
        .commit
        .as_deref()
        .unwrap_or_else(|| panic!("{} records no commit for {corpus}", manifest::MANIFEST));
    let measured = Line {
        heading: heading.to_owned(),
        platform: std::env::consts::OS.to_owned(),
        row: listing.row.clone(),
        commit: abbreviate(commit).to_owned(),
        figures,
    };
    super::live::record(
        RECORD,
        &json!({
            "heading": heading,
            "platform": measured.platform,
            "corpus": corpus,
            "row": measured.row,
            "commit": measured.commit,
            "line": measured.to_string(),
            "figures": {
                "conformant": figures.conformant,
                "accepted": figures.accepted,
                "total": figures.total,
                "acceptedDifferences": figures.accepted_differences,
                "gapDifferences": figures.gap_differences,
                "proseDifferences": figures.prose_differences,
            },
            "detail": detail,
            "test": super::live::test_name(),
        }),
    );
    measured
}

/// Asserts `measured` is the line `docs/parity.md` "Measured figures"
/// declares under its heading for this host: the line marked `all`, or the
/// one marked with the host's platform.
pub fn check(measured: &Line) {
    let heading = &measured.heading;
    let host = &measured.platform;
    let document = scorecard();
    let declared: Vec<Line> = lines(&document)
        .into_iter()
        .filter(|line| line.heading == *heading && holds_on(line, host))
        .collect();
    let mut expected = measured.clone();
    match declared.as_slice() {
        [] => {
            expected.platform = ALL.to_owned();
            panic!(
                "{SCORECARD} declares no figures for `{heading}` on {host}; add this line to its \
                 \"{FIGURES_SECTION}\" table (marked `{host}` instead of `{ALL}` when another \
                 platform counts otherwise):\n{expected}"
            );
        }
        [line] => {
            expected.platform.clone_from(&line.platform);
            assert!(
                *line == expected,
                "the figures of `{heading}` moved on {host}; update its line of {SCORECARD} \
                 \"{FIGURES_SECTION}\" (and the row it measures) to what this run counted:\n  \
                 declared: {line}\n  measured: {expected}"
            );
        }
        _ => panic!("{SCORECARD} declares the figures of `{heading}` more than once on {host}"),
    }
}

/// [`record`] then [`check`]: the shared helper a replay that keeps its own
/// ledger, rather than a [`super::replay::Tally`], ends with.
pub fn assert_figures(heading: &str, corpus: &str, figures: Figures, detail: Value) {
    check(&record(heading, corpus, figures, detail));
}
