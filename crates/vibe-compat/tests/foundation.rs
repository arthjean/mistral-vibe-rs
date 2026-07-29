#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;

use serde_json::Value;
use vibe_compat::model::ScenarioKind;
use vibe_compat::oracle::load_scenarios;
use vibe_protocol::{Envelope, InitializeParams};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture(scenario_id: &str) -> Value {
    let path = root()
        .join("compat/corpus/upstream-2.23.1")
        .join(format!("{scenario_id}.json"));
    serde_json::from_slice(&fs::read(path).expect("checked-in fixture"))
        .expect("valid fixture JSON")
}

#[test]
fn rust_envelopes_match_all_recorded_protocol_oracle_cases() {
    let scenarios = load_scenarios().expect("scenario inventory");
    for scenario in scenarios
        .scenarios
        .iter()
        .filter(|scenario| scenario.kind == ScenarioKind::Protocol)
    {
        let payload = scenario.payload.as_deref().expect("protocol payload");
        let upstream = fixture(&scenario.id);
        let upstream_result = &upstream["outcome"]["jsonFrames"][0];
        let rust = serde_json::from_str::<Envelope>(payload);
        if upstream_result["accepted"] == true {
            let rust = rust.expect("upstream accepted the protocol fixture");
            assert_eq!(
                serde_json::to_value(rust).expect("Rust envelope serializes"),
                upstream_result["value"],
                "wire mismatch for {}",
                scenario.id
            );
        } else {
            assert!(
                rust.is_err(),
                "Rust accepted invalid fixture {}",
                scenario.id
            );
        }
    }
}

#[test]
fn initialize_defaults_and_camel_case_match_the_oracle() {
    let scenario = load_scenarios()
        .expect("scenario inventory")
        .scenarios
        .into_iter()
        .find(|scenario| scenario.id == "protocol-initialize-camel-case")
        .expect("initialize scenario");
    let rust: InitializeParams =
        serde_json::from_str(scenario.payload.as_deref().expect("initialize payload"))
            .expect("valid initialize fixture");
    let upstream = fixture(&scenario.id);
    assert_eq!(
        serde_json::to_value(rust).expect("initialize serializes"),
        upstream["outcome"]["jsonFrames"][0]["value"]
    );
}

#[test]
fn schema_digest_is_an_explicit_baseline_decision() {
    let expected =
        fs::read_to_string(root().join("compat/protocol-schema.sha256")).expect("schema baseline");
    assert_eq!(vibe_protocol::protocol_schema_digest(), expected.trim());
}
