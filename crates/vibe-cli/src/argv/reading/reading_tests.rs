//! The rewrite argparse's reading makes of a vector, token by token.

use std::ffi::OsString;

use clap::CommandFactory;

use super::{python_repr, read};
use crate::Arguments;
use crate::argv::PORT_ONLY_ARGUMENTS;

fn rewrite(argv: &[&str]) -> (Vec<String>, Option<String>) {
    let full: Vec<OsString> = std::iter::once("vibe")
        .chain(argv.iter().copied())
        .map(OsString::from)
        .collect();
    let reading = read(&Arguments::command(), full, PORT_ONLY_ARGUMENTS);
    (
        reading
            .argv
            .iter()
            .skip(1)
            .map(|token| token.to_string_lossy().into_owned())
            .collect(),
        reading.refusal,
    )
}

#[test]
fn an_unambiguous_prefix_is_spelled_out() {
    assert_eq!(rewrite(&["--p", "hi"]).0, ["--prompt", "hi"]);
    assert_eq!(rewrite(&["--se"]).0, ["--setup"]);
    assert_eq!(rewrite(&["--o", "json"]).0, ["--output", "json"]);
    assert_eq!(rewrite(&["--max-tu=3"]).0, ["--max-turns=3"]);
    assert_eq!(rewrite(&["--te"]).0, ["--teleport"]);
    assert_eq!(rewrite(&["--yo"]).0, ["--auto-approve"]);
}

/// An argument only this port declares answers to its exact spelling, so it
/// never makes a reference abbreviation ambiguous and is never reached by one.
#[test]
fn a_port_only_argument_takes_no_abbreviation() {
    assert_eq!(rewrite(&["--mod", "x"]).0, ["--mod", "--", "x"]);
    assert_eq!(rewrite(&["--model", "x"]).0, ["--model", "x"]);
    let (_, refusal) = rewrite(&["--m", "3"]);
    assert_eq!(
        refusal.as_deref(),
        Some("ambiguous option: --m could match --max-turns, --max-price, --max-tokens")
    );
}

#[test]
fn an_ambiguous_prefix_cuts_the_vector_where_argparse_stops() {
    let (argv, refusal) = rewrite(&["--trust", "--a", "--bogus"]);
    assert_eq!(argv, ["--trust"]);
    assert_eq!(
        refusal.as_deref(),
        Some("ambiguous option: --a could match --agent, --auto-approve, --add-dir")
    );
}

#[test]
fn a_value_shaped_like_a_negative_number_stays_a_value() {
    assert_eq!(rewrite(&["--max-turns", "-5"]).0, ["--max-turns=-5"]);
    assert_eq!(rewrite(&["--agent", "-5"]).0, ["--agent=-5"]);
    assert_eq!(rewrite(&["-p", "-1"]).0, ["--prompt=-1"]);
    assert_eq!(rewrite(&["-5", "--trust"]).0, ["--trust", "--", "-5"]);
    assert_eq!(rewrite(&["-.5"]).0, ["--", "-.5"]);
    assert_eq!(rewrite(&["-x y"]).0, ["--", "-x y"]);
}

#[test]
fn a_flag_shaped_token_is_never_taken_as_a_value() {
    assert_eq!(rewrite(&["--agent", "--trust"]).0, ["--agent", "--trust"]);
    assert_eq!(rewrite(&["--agent", "--", "x"]).0, ["--agent", "--", "x"]);
    assert_eq!(rewrite(&["--resume", "-c"]).0, ["--resume", "--continue"]);
}

#[test]
fn every_positional_moves_behind_one_separator_in_order() {
    assert_eq!(
        rewrite(&["first", "--trust", "second"]).0,
        ["--trust", "--", "first", "second"]
    );
    assert_eq!(rewrite(&["--", "--not-a-flag"]).0, ["--", "--not-a-flag"]);
    assert!(rewrite(&["--"]).0.is_empty());
}

#[test]
fn a_flag_given_a_value_is_refused_as_argparse_refuses_it() {
    let (argv, refusal) = rewrite(&["--trust=yes"]);
    assert!(argv.is_empty());
    assert_eq!(
        refusal.as_deref(),
        Some("argument --trust: ignored explicit argument 'yes'")
    );
    assert_eq!(
        rewrite(&["--trust="]).1.as_deref(),
        Some("argument --trust: ignored explicit argument ''")
    );
    assert_eq!(
        rewrite(&["-c=x"]).1.as_deref(),
        Some("argument -c/--continue: ignored explicit argument 'x'")
    );
}

#[test]
fn a_short_cluster_is_left_for_clap_to_split() {
    assert_eq!(rewrite(&["-phello"]).0, ["-phello"]);
    assert_eq!(rewrite(&["-x"]).0, ["-x"]);
}

#[test]
fn values_are_quoted_as_python_quotes_them() {
    assert_eq!(python_repr("x"), "'x'");
    assert_eq!(python_repr(""), "''");
    assert_eq!(python_repr("it's"), "\"it's\"");
    assert_eq!(python_repr("a\tb"), "'a\\tb'");
}
