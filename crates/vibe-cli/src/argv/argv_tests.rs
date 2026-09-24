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

/// `--smart-approve` asks for the Unified Harness and names its agent only
/// when `--agent` did not, after the parse, so the exclusive pair it joins is
/// never checked against it (`vibe/cli/entrypoint.py:210-217`).
#[test]
fn smart_approve_is_rewritten_after_the_parse() {
    let alone = drive(&["--smart-approve"]).unwrap_or_else(|_| panic!("refused"));
    assert!(alone.experimental_harness && alone.smart_approve);
    assert_eq!(alone.agent.as_deref(), Some(super::SMART_APPROVE_AGENT));
    let named =
        drive(&["--smart-approve", "--agent", "plan"]).unwrap_or_else(|_| panic!("refused"));
    assert_eq!(named.agent.as_deref(), Some("plan"));
    let legacy =
        drive(&["--smart-approve", "--legacy-harness"]).unwrap_or_else(|_| panic!("refused"));
    assert!(legacy.experimental_harness && legacy.legacy_harness);
    let failure = refuse(&["--legacy-harness", "--experimental-harness"]);
    assert_eq!(
        last_line(&failure.rendered),
        "vibe: error: argument --experimental-harness: not allowed with argument --legacy-harness"
    );
}

/// The Unified Harness spawns the reference's entry point with this first
/// argument to run its PTY helper (`vibe/cli/entrypoint.py:5-10`). This port
/// ships no such harness, so nothing spawns it, and the argument is refused
/// like any other it does not declare.
#[test]
fn the_unified_harness_pty_helper_is_not_an_entry_point_here() {
    let failure = refuse(&["--internal-posix-pty-helper"]);
    assert_eq!(failure.exit, 2);
    assert_eq!(
        last_line(&failure.rendered),
        "vibe: error: unrecognized arguments: --internal-posix-pty-helper"
    );
}

/// `int()` and `float()` accept more than digits, and refuse what Python
/// refuses; an integer past `i64` saturates rather than failing.
#[test]
fn budgets_convert_as_python_converts_them() {
    for (raw, expected) in [
        ("3", 3),
        (" 3 ", 3),
        ("+3", 3),
        ("-5", -5),
        ("1_000", 1000),
        ("007", 7),
        ("99999999999999999999999", i64::MAX),
        ("-99999999999999999999999", i64::MIN),
    ] {
        assert_eq!(super::python_int(raw), Ok(expected), "{raw:?}");
    }
    for raw in ["", "x", "1e3", "1__0", "_1", "1_", "1.0", "+", "- 1"] {
        assert!(super::python_int(raw).is_err(), "{raw:?} was accepted");
    }
    for (raw, expected) in [
        ("1.5", 1.5),
        ("1e2", 100.0),
        ("1_000.5", 1000.5),
        (" -2.5 ", -2.5),
        (".5", 0.5),
        ("1.", 1.0),
    ] {
        assert_eq!(super::python_float(raw), Ok(expected), "{raw:?}");
    }
    assert!(super::python_float("inf").is_ok_and(f64::is_infinite));
    assert!(super::python_float("-Infinity").is_ok_and(f64::is_infinite));
    assert!(super::python_float("NaN").is_ok_and(f64::is_nan));
    for raw in ["", "cheap", "1__0", "1_.5", "_1", "1e", "0x10"] {
        assert!(super::python_float(raw).is_err(), "{raw:?} was accepted");
    }
}
