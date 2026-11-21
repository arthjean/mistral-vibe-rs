//! The import rules of the parity scripts (AGENTS.md, "The behavioral
//! oracle"), checked by `scripts/parity/import_rules.py` through Python's
//! `ast` module: a driver never imports another driver, a `*_cases.py` or
//! `*_scenarios.py` table imports the standard library only, and only its own
//! driver imports a table. The script's docstring defines a driver, a table
//! and a shared module. It reads syntax only, so it runs without a reference
//! environment, on every host that has `python3`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use super::replay::repository;

fn check(root: &Path) -> Output {
    Command::new("python3")
        .arg(repository().join("scripts/parity/import_rules.py"))
        .arg(root)
        .env_remove("PYTHONPATH")
        .output()
        .expect("python3 runs the import rules")
}

#[test]
fn no_driver_imports_a_driver_and_every_table_imports_the_standard_library_only() {
    let output = check(&repository());
    assert!(
        output.status.success(),
        "the parity scripts break the import rules of AGENTS.md:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = String::from_utf8_lossy(&output.stdout);
    // The manifest alone names more than thirty drivers; a count far below
    // that means the classification stopped reading it.
    let drivers: usize = summary
        .split_whitespace()
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("an unexpected summary: {summary}"));
    assert!(drivers >= 30, "only {drivers} drivers found: {summary}");
}

/// A tree that breaks each rule once, beside modules that keep them.
#[test]
fn the_check_names_every_import_that_breaks_a_rule() {
    let directory = tempfile::tempdir().expect("a temporary tree");
    let root = directory.path();
    let scripts = root.join("scripts/parity");
    fs::create_dir_all(&scripts).expect("the scripts directory");
    let write = |name: &str, text: &str| {
        fs::write(scripts.join(name), text).expect("a fixture module");
    };
    let command_line =
        "import argparse\n\nif __name__ == \"__main__\":\n    argparse.ArgumentParser()\n";
    write(
        "manifest.json",
        r#"{"entries": [{"driver": ["scripts/parity/alpha.py", "--flag"]}]}"#,
    );
    write(
        "repin_plan.json",
        r#"{"stages": [{"drivers": ["gamma.py"]}]}"#,
    );
    // A driver by the manifest, importing a driver by its command line, inside
    // a function, and its own table, which is allowed.
    write(
        "alpha.py",
        "import shared\nimport alpha_cases\n\n\ndef run():\n    from beta import thing\n",
    );
    write("beta.py", &format!("import shared\n{command_line}"));
    // A driver by the re-pin plan, importing another driver's table.
    write("gamma.py", "import alpha_cases\n");
    // `debt.py` has a `__main__` guard but no command line: shared, so a
    // driver may import it.
    write(
        "debt.py",
        "import sys\n\nif __name__ == \"__main__\":\n    sys.exit(1)\n",
    );
    write("delta.py", &format!("import debt\n{command_line}"));
    write("shared.py", "import json\n");
    write(
        "alpha_cases.py",
        "from __future__ import annotations\nimport json\nfrom typing import Any\nimport shared\nimport pydantic.fields\n",
    );
    write("orphan_scenarios.py", "import json\n");

    let output = check(root);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(
        lines,
        [
            "scripts/parity/orphan_scenarios.py: table without a driver `orphan.py` to own it",
            "scripts/parity/alpha.py:6: driver imports driver `beta`",
            "scripts/parity/alpha_cases.py:4: table imports shared module `shared`",
            "scripts/parity/alpha_cases.py:5: table imports module outside the standard library `pydantic.fields`",
            "scripts/parity/gamma.py:1: imports `alpha_cases`, the table of `alpha.py`",
        ],
        "stderr: {stderr}"
    );
}
