//! The composite action's inputs against the reference's.
//!
//! A workflow written for the reference's `action.yml` names its inputs,
//! relies on their defaults and omits the optional ones, so those three facts
//! are the contract. This port may declare more inputs as long as every one it
//! adds is optional, which keeps a reference-shaped `with:` block valid and its
//! behavior unchanged. The reference inputs are recorded here as names,
//! requirements and default values; the live probe re-reads the pinned
//! checkout and fails if they drift.

use std::fs;
use std::path::Path;

use vibe_core::parity::{off_pin_reason, reference_root};

/// One declared input: its name, whether it is required, and its default.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Input {
    name: String,
    required: bool,
    default: Option<String>,
}

/// The reference's inputs at the pin, in declaration order.
fn reference_inputs() -> Vec<Input> {
    let input = |name: &str, required, default: Option<&str>| Input {
        name: name.to_owned(),
        required,
        default: default.map(str::to_owned),
    };
    vec![
        input("prompt", true, Some("You are a helpful assistant\n")),
        input("MISTRAL_API_KEY", true, None),
        input("install_python", true, Some("true")),
        input("python_version", false, None),
    ]
}

/// The `inputs:` block of an action file, read line by line: an input opens at
/// two spaces of indentation, its keys sit at four, and a `|` default is the
/// block of deeper lines below it, each ending in a newline.
fn declared_inputs(text: &str) -> Vec<Input> {
    let mut inputs: Vec<Input> = Vec::new();
    let mut in_inputs = false;
    let mut block: Option<String> = None;
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        if let Some(value) = block.as_mut() {
            if indent >= 6 || trimmed.is_empty() {
                if !trimmed.is_empty() {
                    value.push_str(trimmed);
                    value.push('\n');
                }
                continue;
            }
            let value = block.take();
            if let Some(input) = inputs.last_mut() {
                input.default = value;
            }
        }
        if indent == 0 {
            in_inputs = trimmed == "inputs:";
            continue;
        }
        if !in_inputs || trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        match (indent, key) {
            (2, name) => inputs.push(Input {
                name: name.to_owned(),
                required: false,
                default: None,
            }),
            (4, "required") => {
                if let Some(input) = inputs.last_mut() {
                    input.required = value == "true";
                }
            }
            (4, "default") if value == "|" => block = Some(String::new()),
            (4, "default") => {
                if let Some(input) = inputs.last_mut() {
                    input.default = Some(value.to_owned());
                }
            }
            _ => {}
        }
    }
    inputs
}

fn port_inputs() -> Vec<Input> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../action.yml");
    declared_inputs(&fs::read_to_string(path).expect("the action ships beside the workspace"))
}

#[test]
fn the_action_declares_every_reference_input_unchanged_and_adds_only_optional_ones() {
    let port = port_inputs();
    let reference = reference_inputs();
    assert_eq!(
        port.get(..reference.len()),
        Some(reference.as_slice()),
        "the reference inputs open the block, in its order, with its requirements and defaults"
    );
    for added in &port[reference.len()..] {
        assert!(
            !added.required,
            "{} is an input the reference does not declare, so a reference workflow never \
             passes it and it cannot be required",
            added.name
        );
    }
}

#[test]
fn the_recorded_reference_inputs_match_the_pinned_checkout() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "action inputs") {
        eprintln!("{reason}");
        return;
    }
    let text = fs::read_to_string(root.join("action.yml")).expect("the reference action");
    assert_eq!(declared_inputs(&text), reference_inputs());
}
