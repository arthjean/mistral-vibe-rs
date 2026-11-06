//! `scripts/parity/repin.py capture`, run end to end in a throwaway copy of
//! the repository layout with a stand-in driver, so the tests need no
//! reference checkout and never touch the committed manifest.
//!
//! The stand-in records the commit `repin.py` hands it, as a real driver does
//! through `reference.require_pin`, and its behavior is chosen per test
//! through `STAND_IN_MODE`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

use super::REFERENCE_COMMIT;
use super::replay::repository;

const CORPUS: &str = "crates/sample/tests/sample/corpus.json";

/// The stand-in driver: writes a corpus of `STAND_IN_COUNT` scenarios to the
/// path after `--output`, recording the commit `repin.py` sanctioned.
const STAND_IN: &str = r#"
import json, os, sys
from pathlib import Path

mode = os.environ.get("STAND_IN_MODE", "write")
count = int(os.environ.get("STAND_IN_COUNT", "20"))
output = Path(sys.argv[sys.argv.index("--output") + 1])
committed = Path(__file__).resolve().parents[2] / "crates/sample/tests/sample/corpus.json"
if mode == "fail":
    sys.exit(3)
if mode == "race":
    committed.write_text(committed.read_text() + " ", encoding="utf-8")
commit = os.environ["VIBE_PARITY_REPIN_COMMIT"]
corpus = {
    "schemaVersion": 1,
    "reference": {"commit": commit},
    "scenarios": [{"name": f"s{index}", "observed": mode} for index in range(count)],
}
output.write_text(json.dumps(corpus) + "\n", encoding="utf-8", newline="")
"#;

/// A copy of the repository layout `repin.py` reads, holding one corpus of
/// 20 scenarios at the pin and its manifest entry.
struct World {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl World {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let root = directory.path().to_path_buf();
        let scripts = root.join("scripts/parity");
        std::fs::create_dir_all(&scripts).expect("the scripts directory");
        for module in [
            "repin.py",
            "provenance.py",
            "pin.py",
            "materialize.py",
            "host_env.py",
            "sentinel.py",
            "census.py",
        ] {
            std::fs::copy(
                repository().join("scripts/parity").join(module),
                scripts.join(module),
            )
            .expect("a parity module copies");
        }
        std::fs::write(scripts.join("stand_in.py"), STAND_IN).expect("the stand-in driver");
        std::fs::create_dir_all(root.join("docs")).expect("the docs directory");
        std::fs::write(root.join("docs/parity.md"), "| 1 | Sample | 100 |\n").expect("a scorecard");
        let corpus = json!({
            "schemaVersion": 1,
            "reference": {"commit": REFERENCE_COMMIT},
            "scenarios": (0..20).map(|index| json!({"name": format!("s{index}"), "observed": "old"})).collect::<Vec<_>>(),
        });
        let text = format!("{corpus}\n");
        let path = root.join(CORPUS);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the corpus directory");
        std::fs::write(&path, &text).expect("the corpus");
        let manifest = json!({
            "schemaVersion": 1,
            "transition": [],
            "entries": [{
                "corpus": CORPUS,
                "kind": "corpus",
                "driver": ["scripts/parity/stand_in.py"],
                "output": "--output",
                "test": "crates/sample/tests/sample.rs::replays",
                "row": "1",
                "commit": REFERENCE_COMMIT,
                "sha256": crate::text::sha256_hex(text.as_bytes()),
                "count": 20,
                "floors": {"scenarios": 19},
            }],
        });
        std::fs::write(scripts.join("manifest.json"), format!("{manifest:#}\n"))
            .expect("the manifest");
        Self {
            _directory: directory,
            root,
        }
    }

    fn capture(&self, arguments: &[&str], environment: &[(&str, &str)]) -> Output {
        let mut command = Command::new("python3");
        command
            .arg(self.root.join("scripts/parity/repin.py"))
            .arg("capture")
            .arg(CORPUS)
            .args(arguments)
            .current_dir(&self.root)
            .env_remove("VIBE_PARITY_REPIN_COMMIT");
        for (key, value) in environment {
            command.env(key, value);
        }
        command.output().expect("python3 runs repin.py")
    }

    fn corpus(&self) -> String {
        std::fs::read_to_string(self.root.join(CORPUS)).expect("the corpus is readable")
    }

    fn entry(&self) -> Value {
        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(self.root.join("scripts/parity/manifest.json"))
                .expect("the manifest is readable"),
        )
        .expect("the manifest parses");
        manifest["entries"][0].clone()
    }

    fn leftovers(&self) -> Vec<PathBuf> {
        let directory = self
            .root
            .join(Path::new(CORPUS).parent().expect("a parent"));
        std::fs::read_dir(directory)
            .expect("the corpus directory is readable")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| !path.ends_with("corpus.json"))
            .collect()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_capture_replaces_the_corpus_and_records_its_digest_and_commit_in_one_invocation() {
    let world = World::new();
    let output = world.capture(&[], &[("STAND_IN_COUNT", "40")]);
    assert!(output.status.success(), "{}", stderr(&output));
    let corpus = world.corpus();
    assert!(
        corpus.contains("\"observed\": \"write\""),
        "the corpus was replaced"
    );
    let entry = world.entry();
    assert_eq!(entry["sha256"], crate::text::sha256_hex(corpus.as_bytes()));
    assert_eq!(entry["commit"], REFERENCE_COMMIT);
    assert_eq!(entry["count"], 40);
    assert_eq!(
        entry["floors"]["scenarios"], 38,
        "a floor rises with the corpus"
    );
    assert!(entry.get("offTarget").is_none());
    assert!(
        world.leftovers().is_empty(),
        "no staged file is left behind: {:?}",
        world.leftovers()
    );
}

#[test]
fn a_failed_capture_replaces_nothing() {
    let world = World::new();
    let corpus = world.corpus();
    let entry = world.entry();
    let output = world.capture(&[], &[("STAND_IN_MODE", "fail")]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("nothing was replaced"),
        "{}",
        stderr(&output)
    );
    assert_eq!(world.corpus(), corpus);
    assert_eq!(world.entry(), entry);
    assert!(world.leftovers().is_empty(), "{:?}", world.leftovers());
}

#[test]
fn a_corpus_that_changes_during_the_capture_is_not_replaced() {
    let world = World::new();
    let entry = world.entry();
    let output = world.capture(&[], &[("STAND_IN_MODE", "race")]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains(&format!("{CORPUS} changed during capture; rerun")),
        "{}",
        stderr(&output)
    );
    assert!(
        world.corpus().contains("\"old\""),
        "the racing edit is kept, not overwritten"
    );
    assert_eq!(world.entry(), entry);
}

#[test]
fn a_capture_that_lost_scenarios_keeps_the_old_floor_unless_told_otherwise() {
    let world = World::new();
    let output = world.capture(&[], &[("STAND_IN_COUNT", "10")]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        world.entry()["floors"]["scenarios"],
        19,
        "the floor does not follow a shrink"
    );

    let world = World::new();
    let output = world.capture(&["--accept-shrink"], &[("STAND_IN_COUNT", "10")]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(world.entry()["floors"]["scenarios"], 10);
}

#[test]
fn a_capture_at_another_commit_is_recorded_off_target() {
    let world = World::new();
    let target = "0123456789abcdef0123456789abcdef01234567";
    let output = world.capture(&["--commit", target], &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let entry = world.entry();
    assert_eq!(entry["commit"], target);
    assert_eq!(entry["offTarget"], true);
    assert!(world.corpus().contains(target));
}

#[test]
fn a_corpus_edited_outside_the_tool_is_refused_before_the_driver_runs() {
    let world = World::new();
    let path = world.root.join(CORPUS);
    std::fs::write(&path, world.corpus().replace("s19", "s99")).expect("an out-of-band edit");
    let output = world.capture(&[], &[]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("changed outside the capture tool"),
        "{}",
        stderr(&output)
    );
    assert!(world.corpus().contains("s99"), "the driver never ran");
}
