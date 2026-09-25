//! What the review requests accept and refuse, pinned against answers the
//! reference's `validate_wire` gave for the same payloads: the issue paths and
//! their count are the contract, the sentences are this port's own.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use vibe_core::checkpoints::{Owner, RegionId, ReviewTarget};
use vibe_protocol::PathSegment;

use super::params::{ReviewCall, parse};

fn params(value: Value) -> BTreeMap<String, Value> {
    value
        .as_object()
        .expect("an object")
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The paths of every issue `method` reports for `payload`, rendered the way
/// the wire renders them.
fn refusal(method: &str, payload: Value) -> Vec<Value> {
    match parse(method, &params(payload)) {
        Ok(_) => unreachable!("{method} accepted a payload the reference refuses"),
        Err(issues) => issues
            .into_iter()
            .map(|issue| {
                Value::Array(
                    issue
                        .path
                        .into_iter()
                        .map(|segment| match segment {
                            PathSegment::Field(name) => Value::String(name),
                            PathSegment::Index(index) => json!(index),
                        })
                        .collect(),
                )
            })
            .collect(),
    }
}

fn accepted(method: &str, payload: Value) -> ReviewCall {
    match parse(method, &params(payload)) {
        Ok(request) => request.call,
        Err(issues) => unreachable!("{method} refused a payload the reference accepts: {issues:?}"),
    }
}

#[test]
fn declared_fields_are_reported_before_unknown_ones_and_only_camel_case_counts() {
    assert_eq!(
        refusal("review/state", json!({})),
        vec![json!(["sessionId"])]
    );
    assert_eq!(
        refusal("review/state", json!({"session_id": "a"})),
        vec![json!(["sessionId"]), json!(["session_id"])]
    );
    assert_eq!(
        refusal("review/state", json!({"sessionId": 1, "x": 1})),
        vec![json!(["sessionId"]), json!(["x"])]
    );
    assert_eq!(
        refusal("review/baseline", json!({"sessionId": "s", "path": null})),
        vec![json!(["path"])]
    );
    assert_eq!(
        refusal("review/revert", json!({"target": {"kind": "all"}, "zz": 1})),
        vec![json!(["sessionId"]), json!(["zz"])]
    );
}

#[test]
fn a_union_is_refused_at_its_own_path_until_its_tag_resolves() {
    for owner in [
        json!("agent"),
        json!(null),
        json!([1]),
        json!({}),
        json!({"kind": 7}),
    ] {
        assert_eq!(
            refusal(
                "review/turnDiff",
                json!({"sessionId": "s", "path": "p", "owner": owner})
            ),
            vec![json!(["owner"])],
            "{owner}"
        );
    }
    assert_eq!(
        refusal(
            "review/turnDiff",
            json!({"sessionId": "s", "path": "p", "owner": {"kind": "agent", "turn_id": 1}})
        ),
        vec![
            json!(["owner", "agent", "turnId"]),
            json!(["owner", "agent", "turn_id"])
        ]
    );
    assert_eq!(
        refusal(
            "review/approve",
            json!({"sessionId": "s", "target": {"kind": "scope", "owner": {"kind": "agent", "turnId": 1, "z": 0}}})
        ),
        vec![json!(["target", "scope", "owner", "agent", "z"])]
    );
    assert_eq!(
        refusal(
            "review/approve",
            json!({"sessionId": "s", "target": {"kind": "region", "path": 1}})
        ),
        vec![
            json!(["target", "region", "path"]),
            json!(["target", "region", "versionIndex"]),
            json!(["target", "region", "ordinal"]),
        ]
    );
    assert_eq!(
        refusal(
            "review/approve",
            json!({"sessionId": "s", "target": {"kind": "scopeFile"}})
        ),
        vec![
            json!(["target", "scopeFile", "owner"]),
            json!(["target", "scopeFile", "path"])
        ]
    );
    assert_eq!(
        refusal("review/approve", json!({"sessionId": "s", "target": null})),
        vec![json!(["target"])]
    );
}

#[test]
fn every_region_reference_is_checked_and_must_not_be_negative() {
    assert_eq!(
        refusal(
            "review/revert",
            json!({"sessionId": "s", "target": {"kind": "regions", "path": "p", "regions": [
                {"versionIndex": -1, "ordinal": -2}, "x", {"versionIndex": 1}
            ]}})
        ),
        vec![
            json!(["target", "regions", "regions", 0, "versionIndex"]),
            json!(["target", "regions", "regions", 0, "ordinal"]),
            json!(["target", "regions", "regions", 1]),
            json!(["target", "regions", "regions", 2, "ordinal"]),
        ]
    );
    assert_eq!(
        refusal(
            "review/revert",
            json!({"sessionId": "s", "target": {"kind": "regions", "path": "p", "regions": "x"}})
        ),
        vec![json!(["target", "regions", "regions"])]
    );
}

#[test]
fn integers_are_read_as_pydantic_reads_them_in_lax_mode() {
    let owner = |turn_id: Value| match accepted(
        "review/turnDiff",
        json!({"sessionId": "s", "path": "p", "owner": {"kind": "agent", "turnId": turn_id}}),
    ) {
        ReviewCall::TurnDiff { owner, .. } => owner,
        _ => unreachable!("a turn diff"),
    };
    for (value, turn_id) in [
        (json!(true), 1),
        (json!(2.0), 2),
        (json!(" 3 "), 3),
        (json!("+3"), 3),
        (json!("1_0"), 10),
        (json!("3.00"), 3),
        (json!("007"), 7),
    ] {
        assert_eq!(owner(value.clone()), Owner::Agent { turn_id }, "{value}");
    }
    // A negative identifier and one past every identifier name nothing, as
    // they name nothing upstream, rather than being refused.
    assert_eq!(owner(json!(-1)), Owner::Agent { turn_id: u64::MAX });
    assert_eq!(owner(json!(1e30)), Owner::Agent { turn_id: u64::MAX });
    for value in [
        json!(2.5),
        json!("1e3"),
        json!("3."),
        json!(".0"),
        json!("1__0"),
        json!("_1"),
        json!("1_"),
        json!("0x10"),
        json!(""),
        json!(null),
    ] {
        assert_eq!(
            refusal(
                "review/turnDiff",
                json!({"sessionId": "s", "path": "p", "owner": {"kind": "agent", "turnId": value}})
            ),
            vec![json!(["owner", "agent", "turnId"])],
            "{value}"
        );
    }
}

#[test]
fn a_well_formed_request_names_what_it_asks() {
    assert!(matches!(
        accepted(
            "review/hunks",
            json!({"sessionId": "s", "path": "p", "owner": null})
        ),
        ReviewCall::Hunks { owner: None, .. }
    ));
    assert!(matches!(
        accepted(
            "review/hunks",
            json!({"sessionId": "s", "path": "p", "owner": {"kind": "manual", "index": 0}})
        ),
        ReviewCall::Hunks {
            owner: Some(Owner::Manual { index: 0 }),
            ..
        }
    ));
    match accepted(
        "review/revert",
        json!({"sessionId": "s", "target": {"kind": "regions", "path": "p", "regions": [
            {"versionIndex": 3, "ordinal": 1}
        ]}}),
    ) {
        ReviewCall::Revert(ReviewTarget::Regions { path, regions }) => {
            assert_eq!(path, "p");
            assert_eq!(regions, vec![RegionId::new(3, 1)]);
        }
        _ => unreachable!("a regions revert"),
    }
    assert!(matches!(
        accepted(
            "review/approve",
            json!({"sessionId": "s", "target": {"kind": "lastTurns", "count": "2"}})
        ),
        ReviewCall::Approve(ReviewTarget::LastTurns { count: 2 })
    ));
    assert!(matches!(
        accepted(
            "review/approve",
            json!({"sessionId": "s", "target": {"kind": "lastTurns", "count": -1e30}})
        ),
        ReviewCall::Approve(ReviewTarget::LastTurns { count: i64::MIN })
    ));
}
