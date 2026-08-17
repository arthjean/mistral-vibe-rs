//! The names this build knows, their defaults, and the URL a host and a key
//! resolve to.

use std::collections::BTreeSet;

use super::{EVAL_PATH_TEMPLATE, ExperimentName, build_eval_url};

#[test]
fn every_name_carries_a_key_and_a_default() {
    // The reference pairs its names with its defaults in a dictionary and holds
    // the two together with a module-level assertion. Here the pairing is an
    // exhaustive match, so this test states the values rather than the tie.
    assert_eq!(
        ExperimentName::ALL.map(ExperimentName::key),
        [
            "vibe_cli_system_prompt",
            "vibe_cli_managed_shell_tools",
            "vibe_cli_default_routing_model"
        ]
    );
    assert_eq!(
        ExperimentName::ALL.map(ExperimentName::default_variant),
        ["cli", "legacy", "{}"]
    );
    let keys = ExperimentName::ALL
        .into_iter()
        .map(ExperimentName::key)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        keys.len(),
        ExperimentName::ALL.len(),
        "the keys are distinct"
    );
}

#[test]
fn a_key_resolves_to_its_name_and_an_unknown_one_to_nothing() {
    for name in ExperimentName::ALL {
        assert_eq!(ExperimentName::from_key(name.key()), Some(name));
    }
    assert_eq!(ExperimentName::from_key("vibe_cli_unknown_rollout"), None);
    assert_eq!(ExperimentName::from_key(""), None);
    assert_eq!(
        ExperimentName::from_key("VIBE_CLI_SYSTEM_PROMPT"),
        None,
        "a key is matched exactly"
    );
}

#[test]
fn the_url_trims_the_host_and_strips_every_trailing_slash() {
    let cases = [
        ("https://experiments.example.test", "sdk-key"),
        ("https://experiments.example.test/", "sdk-key"),
        ("https://experiments.example.test///", "sdk-key"),
        ("  https://experiments.example.test  ", " sdk-key "),
    ];
    for (host, key) in cases {
        assert_eq!(
            build_eval_url(host, key).as_deref(),
            Some("https://experiments.example.test/api/eval/sdk-key"),
            "{host:?}"
        );
    }
    assert_eq!(
        build_eval_url("https://gateway.example.test/gb/", "sdk-key").as_deref(),
        Some("https://gateway.example.test/gb/api/eval/sdk-key"),
        "a host carrying a path keeps it"
    );
}

#[test]
fn a_blank_host_or_key_resolves_no_url() {
    for (host, key) in [
        ("", "sdk-key"),
        ("   ", "sdk-key"),
        ("///", "sdk-key"),
        ("  //  ", "sdk-key"),
        ("https://experiments.example.test", ""),
        ("https://experiments.example.test", "  "),
        ("", ""),
    ] {
        assert_eq!(build_eval_url(host, key), None, "{host:?} with {key:?}");
    }
}

#[test]
fn the_path_template_names_the_key_it_substitutes() {
    assert!(EVAL_PATH_TEMPLATE.contains("{client_key}"));
    assert!(EVAL_PATH_TEMPLATE.starts_with('/'));
}
