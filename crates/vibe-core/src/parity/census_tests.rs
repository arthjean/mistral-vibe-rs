//! `scripts/parity/census.py` read through its command line, on synthetic
//! trees whose modules would abort if anything imported them: the census
//! reads public boundaries by syntax only, which is what lets the sentinel
//! run it on upstream trees with a read-only token.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

use super::replay::repository;

/// Every module starts by aborting, so a census that imported one fails.
const TRAP: &str = "raise SystemExit('the census imported reference code')\n";

fn tree(methods: &[&str], fields: &[&str], tools: &[&str], flags: &[&str]) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("a temporary tree");
    let root = directory.path();
    let write = |path: &str, text: String| {
        let path = root.join(path);
        fs::create_dir_all(path.parent().expect("a parent")).expect("directories");
        fs::write(path, text).expect("a fixture file");
    };
    let methods: Vec<String> = methods
        .iter()
        .map(|method| format!("    {method:?},\n"))
        .collect();
    write(
        "vibe/app_server/protocol.py",
        format!(
            "{TRAP}SERVER_METHODS: tuple[str, ...] = (\n{})\n",
            methods.concat()
        ),
    );
    let fields: Vec<String> = fields
        .iter()
        .map(|field| format!("    {field}: int = 0\n"))
        .collect();
    write(
        "vibe/core/config/vibe_schema.py",
        format!(
            "{TRAP}class VibeConfigSchema(ConfigSchema):\n    _private: int = 0\n    kind: ClassVar[str] = ''\n{}",
            fields.concat()
        ),
    );
    for tool in tools {
        write(
            &format!("vibe/core/tools/builtins/{}.py", tool.to_lowercase()),
            format!("{TRAP}class {tool}(BaseTool[Args, Result]):\n    pass\n"),
        );
    }
    write(
        "vibe/core/tools/builtins/_helper.py",
        format!("{TRAP}class Hidden(BaseTool[Args, Result]):\n    pass\n"),
    );
    let flags: Vec<String> = flags
        .iter()
        .map(|flag| format!("    #[arg(\n        long = {flag:?},\n    )]\n    field: bool,\n"))
        .collect();
    write(
        "vibe/cli-rust/src/cli.rs",
        format!(
            "struct Cli {{\n    #[arg(short = 'v', long = \"version\")]\n    version: bool,\n{}}}\n",
            flags.concat()
        ),
    );
    directory
}

fn census(arguments: &[&Path]) -> Value {
    let output = Command::new("python3")
        .arg(repository().join("scripts/parity/census.py"))
        .args(arguments)
        .output()
        .expect("python3 runs the census");
    assert!(
        output.status.success(),
        "the census failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("the census prints JSON")
}

#[test]
fn the_census_reads_every_boundary_without_importing_the_tree() {
    let pinned = tree(
        &["session/new", "tools/list"],
        &["active_model", "tool_paths"],
        &["ReadFile", "ExitPlanMode"],
        &["resume"],
    );
    let found = census(&[pinned.path()]);
    assert_eq!(found["methods"], json!(["session/new", "tools/list"]));
    assert_eq!(
        found["config"],
        json!([
            "VibeConfigSchema.active_model",
            "VibeConfigSchema.tool_paths"
        ])
    );
    assert_eq!(found["tools"], json!(["exit_plan_mode", "read_file"]));
    assert_eq!(found["cliFlags"], json!(["--resume", "--version", "-v"]));
    assert!(
        found["scorecardPaths"]["cited"]
            .as_u64()
            .is_some_and(|cited| cited > 0)
    );
}

#[test]
fn the_census_diff_counts_what_a_release_adds_and_removes_per_boundary() {
    let pinned = tree(
        &["session/new", "tools/list"],
        &["tool_paths"],
        &["ExitPlanMode"],
        &["resume"],
    );
    let newest = tree(
        &["session/new", "skills/local/list", "projects/list"],
        &["strict_config"],
        &[],
        &["resume", "model"],
    );
    let difference = census(&[newest.path(), Path::new("--against"), pinned.path()]);
    assert_eq!(
        difference["methods"],
        json!({"added": ["projects/list", "skills/local/list"], "removed": ["tools/list"]})
    );
    assert_eq!(
        difference["config"],
        json!({"added": ["VibeConfigSchema.strict_config"], "removed": ["VibeConfigSchema.tool_paths"]})
    );
    assert_eq!(
        difference["tools"],
        json!({"added": [], "removed": ["exit_plan_mode"]})
    );
    assert_eq!(
        difference["cliFlags"],
        json!({"added": ["--model"], "removed": []})
    );
}
