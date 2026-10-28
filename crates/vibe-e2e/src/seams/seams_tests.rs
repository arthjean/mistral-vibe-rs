use std::path::Path;

use super::{scan, violations};

#[test]
fn a_port_only_seam_is_reported_with_its_line() {
    let source = "let a = 1;\ncommand.args([\"-p\", \"hi\", \"--api-base\", base]);\n.env(\"VIBE_TEST_FAKE\", \"1\")\n";
    assert_eq!(violations(source), ["2: --api-base", "3: VIBE_TEST_"]);
}

#[test]
fn a_longer_name_that_starts_like_a_seam_is_not_one() {
    assert!(
        violations("\"--models\" \"VIBE_MODELS\" \"X_VIBE_API_BASE\" \"--model-list\"").is_empty()
    );
    assert_eq!(violations("[\"--model\", \"x\"]"), ["1: --model"]);
}

#[test]
fn a_declared_seam_is_allowed() {
    let source = "Scenario::new(\"seam\", |t| run(\"--api-base\")).tests_seam(\"--api-base\")";
    assert!(violations(source).is_empty());
    let source = "Scenario::new(\"seam\", |t| run(\"--api-base\", \"VIBE_MODEL\")).tests_seam(\"--api-base\")";
    assert_eq!(violations(source), ["1: VIBE_MODEL"]);
}

#[test]
fn no_scenario_source_passes_an_undeclared_seam() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    assert!(
        workspace
            .join("crates/vibe-cli/tests/e2e/main.rs")
            .is_file()
    );
    assert_eq!(scan(&workspace), Vec::<String>::new());
}
