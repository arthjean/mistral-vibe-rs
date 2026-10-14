//! The cases below were answered by the reference's own `make_json_patch`
//! (`vibe/app_server/_patch.py` over `jsonpatch` 1.33), with each patch's
//! operations sorted by path: the package orders an object's keys as a
//! Python set does, which changes between processes.

use serde_json::{Value, json};

use super::*;

fn sorted(operations: Vec<PatchOperation>) -> Value {
    let mut operations = operations;
    operations.sort_by(|left, right| left.path.cmp(&right.path));
    patch_json(&operations)
}

#[test]
fn the_patches_are_the_reference_patches() {
    let cases = json!([
        {"source": {"a": 1, "b": {"c": 2}}, "target": {"a": 1, "b": {"c": 3}, "d": 4},
         "ops": [{"op": "replace", "path": "/b/c", "value": 3}, {"op": "add", "path": "/d", "value": 4}]},
        {"source": {"a": [1, 2, 3]}, "target": {"a": [1, 2]},
         "ops": [{"op": "remove", "path": "/a/2", "value": null}]},
        {"source": {"a": [1, 2]}, "target": {"a": [1, 2, 3]},
         "ops": [{"op": "add", "path": "/a/2", "value": 3}]},
        {"source": {"a": [1, 2, 3]}, "target": {"a": [2, 3]},
         "ops": [{"op": "remove", "path": "/a/0", "value": null}]},
        {"source": {"a": [{"x": 1}, {"x": 2}]}, "target": {"a": [{"x": 2}]},
         "ops": [{"op": "replace", "path": "/a/0/x", "value": 2}, {"op": "remove", "path": "/a/1", "value": null}]},
        {"source": {"a": "x", "b": null}, "target": {"a": null, "b": "x"},
         "ops": [{"op": "replace", "path": "/a", "value": null}, {"op": "replace", "path": "/b", "value": "x"}]},
        {"source": {"t": {"text": "hel"}}, "target": {"t": {"text": "hello"}},
         "ops": [{"op": "append", "path": "/t/text", "value": "lo"}]},
        {"source": {"s": {"activeTurnId": "t", "type": "running"}}, "target": {"s": {"type": "idle"}},
         "ops": [{"op": "remove", "path": "/s/activeTurnId", "value": null}, {"op": "replace", "path": "/s/type", "value": "idle"}]},
        {"source": {"a": [1, 2, 3, 4]}, "target": {"a": [4, 1, 2]},
         "ops": [{"op": "add", "path": "/a/0", "value": 3}, {"op": "remove", "path": "/a/2", "value": null}, {"op": "remove", "path": "/a/2", "value": null}]},
        {"source": {"x": 1}, "target": {"x": 1.0},
         "ops": [{"op": "replace", "path": "/x", "value": 1.0}]}
    ]);
    for case in cases.as_array().unwrap() {
        let patch = make_json_patch(&case["source"], &case["target"], &["/t/text"]);
        assert_eq!(sorted(patch), case["ops"], "{case}");
    }
}

#[test]
fn a_patch_applies_back_onto_its_source() {
    let source = json!({"a": {"t": {"text": "Hel"}}, "items": [1, 2, 3], "gone": true});
    let target = json!({"a": {"t": {"text": "Hello"}}, "items": [1, 3, 4], "new": "x"});
    let patch = make_json_patch(&source, &target, &["/a/t/text"]);
    let operations: Vec<Value> = patch.iter().map(PatchOperation::to_json).collect();
    let mut document = source;
    apply_json_patch(&mut document, &operations).unwrap();
    assert_eq!(document, target);
}
