//! Answers recorded from the pinned reference's `packaging` 26.2, which the
//! update notifier parses every version with. The corpus replay in
//! `vibe-cli`'s terminal-services parity test measures the same parser against
//! the live reference; these cases pin the grammar where a failure is easiest
//! to read.

use super::{Version, artifact_version};

fn normalized(raw: &str) -> Option<String> {
    Version::parse(raw).map(|version| version.to_string())
}

#[test]
fn every_spelling_the_grammar_accepts_normalizes_as_the_reference_prints_it() {
    for (raw, expected) in [
        ("1.0dev1", Some("1.0.dev1")),
        ("1.0.dev", Some("1.0.dev0")),
        ("1.0rc", Some("1.0rc0")),
        ("1.0.rc1", Some("1.0rc1")),
        ("1.0alpha1", Some("1.0a1")),
        ("1.0c1", Some("1.0rc1")),
        ("1.0pre1", Some("1.0rc1")),
        ("1.0post1", Some("1.0.post1")),
        ("1.0.post", Some("1.0.post0")),
        ("1.0r1", Some("1.0.post1")),
        ("1.0rev1", Some("1.0.post1")),
        ("1!1.0", Some("1!1.0")),
        ("1.0_a1", Some("1.0a1")),
        ("1.0+abc_def", Some("1.0+abc.def")),
        ("2.25.07", Some("2.25.7")),
        ("V2.25.7", Some("2.25.7")),
        ("1.0+001.AbC-x_Y", Some("1.0+1.abc.x.y")),
        ("v1!02.03.0a", Some("1!2.3.0a0")),
        ("1.0-5", Some("1.0.post5")),
        ("1.0PREVIEW3", Some("1.0rc3")),
        ("\u{1c}1.0\u{3000}", Some("1.0")),
        ("1.0+", None),
        ("1.0--1", None),
        (
            "99999999999999999999999.1",
            Some("99999999999999999999999.1"),
        ),
        ("1.0a.post.dev", Some("1.0a0.post0.dev0")),
        ("\u{ff11}.0", None),
        ("1.0+K", Some("1.0+k")),
    ] {
        assert_eq!(normalized(raw).as_deref(), expected, "{raw:?}");
    }
}

#[test]
fn ordering_follows_the_reference_comparison_key() {
    let version = |raw| Version::parse(raw).expect("valid version");
    assert!(version("1.0a1.dev1") < version("1.0a1"));
    assert!(version("1.0.post1.dev1") < version("1.0.post1"));
    assert!(version("1.0rc1.post1") < version("1.0"));
    assert!(version("1.0rc1.post1") < version("1.0.post1"));
    assert!(version("1.0.dev0") < version("1.0a1.dev0"));
    assert!(version("1.0a1") < version("1.0b1") && version("1.0b1") < version("1.0rc1"));
    assert!(version("1!0.1") > version("9.9"));
    assert!(version("1.0+abc") < version("1.0+1"));
    assert!(version("1.0+a.b") > version("1.0+a"));
    assert!(version("0.0") < version("0.0.1"));
    assert_eq!(version("1.0"), version("1.0.0"));
    assert_eq!(version("1.0").to_string(), "1.0");
    assert_eq!(version("1.0.0").to_string(), "1.0.0");
}

#[test]
fn the_notifier_reads_every_dash_as_a_local_separator() {
    assert!(Version::parse_notifier("2.25.7-1-g1234abc").is_none());
    assert_eq!(
        Version::parse_notifier("2.23.1-dev").map(|version| version.to_string()),
        Some("2.23.1+dev".to_owned())
    );
}

#[test]
fn artifact_filenames_are_held_to_the_wheel_and_sdist_rules() {
    for (filename, expected) in [
        ("mistral_vibe-2.25.7-py3-none-any.whl", Some("2.25.7")),
        ("mistral_vibe-2.25.7-1-py3-none-any.whl", Some("2.25.7")),
        ("mistral_vibe-2.25.7-x1-py3-none-any.whl", None),
        ("mistral__vibe-2.25.7-py3-none-any.whl", None),
        ("mistral-vibe-2.25.7-py3-none-any.whl", None),
        ("mistral_vibe-2.25.7-py3-none.whl", None),
        ("mistral_vibe-2.25.07.tar.gz", Some("2.25.7")),
        ("mistral-vibe-2.25.7.zip", Some("2.25.7")),
        ("nodash.tar.gz", None),
    ] {
        assert_eq!(
            artifact_version(filename)
                .map(|version| version.to_string())
                .as_deref(),
            expected,
            "{filename}"
        );
    }
}
