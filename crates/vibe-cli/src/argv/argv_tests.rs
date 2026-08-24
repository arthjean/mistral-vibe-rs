//! What each refusal reads like once it has been shaped the reference's way.

use clap::error::ErrorKind;

use super::{ParseFailure, parse_arguments, render};

fn drive(argv: &[&str]) -> Result<crate::Arguments, ParseFailure> {
    let full: Vec<&str> = std::iter::once("vibe")
        .chain(argv.iter().copied())
        .collect();
    parse_arguments(full)
}

fn refuse(argv: &[&str]) -> ParseFailure {
    match drive(argv) {
        Err(failure) => failure,
        Ok(_) => panic!("{argv:?} was accepted"),
    }
}

fn last_line(rendered: &str) -> &str {
    rendered
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .unwrap_or_default()
}

/// Every refusal this port reshapes carries the usage block first and one line
/// naming what went wrong, in the sentence CPython's own `argparse` renders.
#[test]
fn each_reshaped_refusal_reads_the_way_argparse_reads() {
    let cases: &[(&[&str], &str)] = &[
        (&["--bogus"], "vibe: error: unrecognized arguments: --bogus"),
        (&["-x"], "vibe: error: unrecognized arguments: -x"),
        (
            &["first", "second"],
            "vibe: error: unrecognized arguments: second",
        ),
        (
            &["--output", "bogus"],
            "vibe: error: argument --output: invalid choice: 'bogus' (choose from text, json, \
             streaming)",
        ),
        (
            &["--agent"],
            "vibe: error: argument --agent: expected one argument",
        ),
        (
            &["--agent", "--trust"],
            "vibe: error: argument --agent: expected one argument",
        ),
        (
            &["--max-turns", "x"],
            "vibe: error: argument --max-turns: invalid int value: 'x'",
        ),
        (
            &["--max-turns", ""],
            "vibe: error: argument --max-turns: invalid int value: ''",
        ),
        (
            &["--max-price", "cheap"],
            "vibe: error: argument --max-price: invalid float value: 'cheap'",
        ),
        (
            &["-c", "--resume"],
            "vibe: error: argument --resume: not allowed with argument -c/--continue",
        ),
    ];
    for (argv, expected) in cases {
        let failure = refuse(argv);
        assert_eq!(failure.exit, 2, "{argv:?} exited {}", failure.exit);
        assert!(failure.use_stderr, "{argv:?} did not go to standard error");
        assert!(
            failure.rendered.starts_with("usage: vibe "),
            "{argv:?} opened with {:?}",
            failure.rendered.lines().next()
        );
        assert_eq!(last_line(&failure.rendered), *expected, "{argv:?}");
    }
}

/// A kind with no argparse sentence keeps clap's render rather than losing the
/// diagnosis to a shape it cannot fill.
#[test]
fn a_kind_the_reshape_does_not_cover_keeps_claps_own_render() {
    let error = clap::Error::raw(
        ErrorKind::Io,
        "the terminal could not be read: a kind no argv reaches\n",
    );
    let rendered = render(&error);
    assert!(
        rendered.contains("a kind no argv reaches"),
        "the fallback lost the diagnosis: {rendered}"
    );
    assert!(
        !rendered.contains("vibe: error:"),
        "a kind with no argparse sentence must not be dressed as one: {rendered}"
    );
}

/// The help and the version are documents rather than refusals: they leave on
/// standard output and exit 0, as they do upstream.
#[test]
fn the_help_and_the_version_leave_on_standard_output_with_nothing_to_report() {
    for argv in [["-h"], ["--help"], ["-v"], ["--version"]] {
        let failure = refuse(&argv);
        assert_eq!(failure.exit, 0, "{argv:?} exited {}", failure.exit);
        assert!(
            !failure.use_stderr,
            "{argv:?} wrote its document to standard error"
        );
    }
}

/// An argv the reference accepts is still accepted, so the reshape never
/// stands between a working invocation and the session.
#[test]
fn an_argv_that_parses_still_parses() {
    let arguments = drive(&["--max-turns", "3", "--output", "json", "hello"])
        .unwrap_or_else(|_| panic!("a valid argv was refused"));
    assert_eq!(arguments.max_turns, Some(3));
    assert_eq!(arguments.initial_prompt.as_deref(), Some("hello"));
}
