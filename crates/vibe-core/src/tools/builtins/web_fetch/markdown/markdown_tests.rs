//! Replays the corpus `scripts/parity/web_fetch_markdown.py` captured from the
//! reference's `_html_to_markdown` and requires the port to agree on every
//! page, the failures included.

use std::path::PathBuf;

use serde_json::Value;

use super::{ConversionError, html_to_markdown};

const CORPUS_RELATIVE: &str = "tests/web-fetch-markdown/corpus.json";

fn corpus() -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CORPUS_RELATIVE);
    let text = std::fs::read_to_string(&path).expect("the committed corpus is readable");
    let corpus: Value = serde_json::from_str(&text).expect("the corpus is JSON");
    corpus["cases"]
        .as_array()
        .expect("the corpus lists its cases")
        .clone()
}

#[test]
fn every_captured_page_converts_as_the_reference_converted_it() {
    let cases = corpus();
    assert!(cases.len() > 400, "the corpus lost its pages");
    let mut mismatches = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap_or_default();
        let html = case["html"].as_str().expect("every case carries its page");
        let expected = match (case["markdown"].as_str(), case["error"].as_str()) {
            (Some(markdown), None) => Ok(markdown.to_owned()),
            (None, Some(error)) => Err(ConversionError(error.to_owned())),
            _ => panic!("case `{name}` records neither an output nor an error"),
        };
        let actual = html_to_markdown(html);
        if actual != expected {
            mismatches.push(format!(
                "{name}\n  html:     {html:?}\n  expected: {expected:?}\n  actual:   {actual:?}"
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} pages diverge:\n{}",
        mismatches.len(),
        cases.len(),
        mismatches.join("\n")
    );
}

#[test]
fn the_corpus_exercises_the_failures_the_reference_raises() {
    let failures = corpus()
        .iter()
        .filter_map(|case| case["error"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    for expected in [
        "maximum recursion depth exceeded",
        "'dict' object is not callable",
        "invalid literal for int() with base 10",
        "convert_soup()",
    ] {
        assert!(
            failures.iter().any(|failure| failure.contains(expected)),
            "no captured page raises `{expected}`"
        );
    }
}
