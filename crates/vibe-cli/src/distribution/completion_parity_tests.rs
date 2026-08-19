//! Guards that hold the shipped completion files to the flags clap declares.
//!
//! `scripts/ci/package-release.sh` copies `completions/` into every release
//! archive, and both installers stage the four files beside the binaries. The
//! clap definition in `crate::Arguments` is the only declaration of what the
//! binary accepts, and nothing used to compare the two: a flag added there was
//! invisible to tab completion until someone noticed, and a flag deleted there
//! kept being offered.
//!
//! The reference ships no completions, so there is no upstream oracle here.
//! This is the same one-declaration-plus-a-scanner shape
//! `super::release_parity_tests` applies to the version literal, pointed at
//! clap's own builder rather than at the manifest.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use clap::CommandFactory;

use crate::Arguments;

/// How one completion format spells a long option.
#[derive(Debug, Clone, Copy)]
enum LongOptionSyntax {
    /// The option is written the way a user types it: `--check-upgrade`.
    AsTyped,
    /// `complete` names the option with `-l check-upgrade`, no dashes.
    FishLongFlag,
}

/// Every completion file the release archive carries, with the syntax its
/// shell uses. A file dropped from this list stops being measured, so the list
/// is checked against the set `package-release.sh` packages.
const COMPLETION_FILES: [(&str, LongOptionSyntax); 4] = [
    ("completions/vibe.bash", LongOptionSyntax::AsTyped),
    ("completions/_vibe", LongOptionSyntax::AsTyped),
    ("completions/vibe.fish", LongOptionSyntax::FishLongFlag),
    ("completions/vibe.ps1", LongOptionSyntax::AsTyped),
];

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{relative} is readable: {error}"))
}

/// Every long flag the running binary accepts and does not hide, including the
/// visible aliases, each spelled the way a user types it.
///
/// `Command::build` is what clap documents for introspection: it is what adds
/// the generated `--help` argument, which the completion files offer and the
/// derive alone does not declare.
fn declared_long_flags() -> BTreeSet<String> {
    let mut command = Arguments::command();
    command.build();
    let mut flags = BTreeSet::new();
    for argument in command.get_arguments() {
        if argument.is_hide_set() {
            continue;
        }
        let Some(long) = argument.get_long() else {
            continue;
        };
        flags.insert(format!("--{long}"));
        for alias in argument.get_visible_aliases().unwrap_or_default() {
            flags.insert(format!("--{alias}"));
        }
    }
    flags
}

/// Every long flag the running binary accepts but hides from its own help.
fn hidden_long_flags() -> BTreeSet<String> {
    let mut command = Arguments::command();
    command.build();
    command
        .get_arguments()
        .filter(|argument| argument.is_hide_set())
        .filter_map(|argument| argument.get_long().map(|long| format!("--{long}")))
        .collect()
}

/// Every long option `text` offers, read with `syntax`.
fn offered_long_flags(text: &str, syntax: LongOptionSyntax) -> BTreeSet<String> {
    match syntax {
        LongOptionSyntax::AsTyped => {
            let mut flags = BTreeSet::new();
            let mut rest = text;
            while let Some(position) = rest.find("--") {
                rest = &rest[position + 2..];
                let name: String = rest
                    .chars()
                    .take_while(|value| value.is_ascii_alphanumeric() || *value == '-')
                    .collect();
                // A bare `--` ends an option list rather than naming one, and
                // `--` followed by punctuation is not a flag either.
                if name.starts_with(|value: char| value.is_ascii_alphanumeric()) {
                    flags.insert(format!("--{name}"));
                }
            }
            flags
        }
        LongOptionSyntax::FishLongFlag => {
            let mut flags = BTreeSet::new();
            for line in text.lines() {
                let mut tokens = line.split_whitespace();
                while let Some(token) = tokens.next() {
                    if token == "-l"
                        && let Some(name) = tokens.next()
                    {
                        flags.insert(format!("--{name}"));
                    }
                }
            }
            flags
        }
    }
}

/// Every disagreement between `expected` and what the committed files offer.
///
/// Taking the expected set as an argument is what makes the scan falsifiable:
/// a test can drive it with a flag no file carries and assert the report names
/// both the flag and every file that omits it.
fn completion_offenses(expected: &BTreeSet<String>) -> Vec<String> {
    let mut offenses = Vec::new();
    for (file, syntax) in COMPLETION_FILES {
        let offered = offered_long_flags(&read(file), syntax);
        assert!(
            !offered.is_empty(),
            "{file} offers no long option at all, so this scan would pass without measuring \
             anything"
        );
        for flag in expected.difference(&offered) {
            offenses.push(format!(
                "{file} omits {flag}, which the clap definition declares"
            ));
        }
        for flag in offered.difference(expected) {
            offenses.push(format!(
                "{file} offers {flag}, which the clap definition does not declare"
            ));
        }
    }
    offenses
}

#[test]
fn every_completion_file_offers_exactly_the_flags_clap_declares() {
    let expected = declared_long_flags();
    assert!(
        expected.len() > 1,
        "the clap definition declares no long flag, so this scan measures nothing"
    );
    let offenses = completion_offenses(&expected);
    assert!(
        offenses.is_empty(),
        "a completion file drifted from the clap definition: {}",
        offenses.join("; ")
    );
}

#[test]
fn the_auto_approve_alias_is_offered_everywhere_the_flag_is() {
    // `--yolo` is a visible alias rather than an argument of its own, so a scan
    // reading only `Arg::get_long` would let it stay missing from every file.
    let expected = declared_long_flags();
    assert!(
        expected.contains("--yolo"),
        "--auto-approve no longer carries the visible alias the completions offer"
    );
    for (file, syntax) in COMPLETION_FILES {
        let offered = offered_long_flags(&read(file), syntax);
        assert!(
            offered.contains("--yolo"),
            "{file} does not offer --yolo, the visible alias of --auto-approve"
        );
    }
}

#[test]
fn a_hidden_flag_is_not_required_of_the_completion_files() {
    let hidden = hidden_long_flags();
    assert!(
        !hidden.is_empty(),
        "no flag is hidden any more, so this test measures nothing; drop it or hide one"
    );
    let expected = declared_long_flags();
    let leaked: Vec<&String> = hidden.intersection(&expected).collect();
    assert!(
        leaked.is_empty(),
        "a hidden flag reached the expected completion set, which would force it into every \
         committed file: {leaked:?}"
    );
}

#[test]
fn a_flag_no_completion_file_carries_is_reported_against_every_file() {
    // The failure a new flag produces, exercised without adding one: the scan
    // must name the flag and each file that omits it, not just the first.
    let mut expected = declared_long_flags();
    expected.insert("--brand-new-flag".to_owned());
    let offenses = completion_offenses(&expected);
    for (file, _) in COMPLETION_FILES {
        assert!(
            offenses
                .iter()
                .any(|offense| offense.contains(file) && offense.contains("--brand-new-flag")),
            "the scan does not report {file} as omitting a newly declared flag: {offenses:?}"
        );
    }
}

#[test]
fn every_packaged_completion_file_is_measured() {
    // `package-release.sh` names the files the archive carries. One added there
    // and not here would ship unmeasured.
    let packaging = read("scripts/ci/package-release.sh");
    for (file, _) in COMPLETION_FILES {
        assert!(
            packaging.contains(file),
            "{file} is measured here but scripts/ci/package-release.sh does not package it"
        );
    }
    let packaged = packaging
        .lines()
        .filter(|line| line.contains("completions/"))
        .flat_map(|line| line.split_whitespace())
        .filter(|token| token.starts_with("completions/"))
        .count();
    assert_eq!(
        packaged,
        COMPLETION_FILES.len(),
        "scripts/ci/package-release.sh packages {packaged} completion files, but {} are measured",
        COMPLETION_FILES.len()
    );
}
