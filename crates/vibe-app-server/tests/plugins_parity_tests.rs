//! Replays the committed plugins server corpus against this port's app server.
//!
//! `scripts/parity/plugins.py --family server` captured the corpus from the
//! pinned reference's own `vibe-app-server`: every scenario serves it over
//! stdio in a fresh home, selecting the unified harness with
//! `--experimental-harness` or with no flag over the rollout in the eval cache,
//! or the legacy one with `--legacy-harness`, and records what `plugin_catalog/read`, `plugins/read`, `plugin/info`,
//! `plugin/reload`, the plugin parts of `runtime/read` and `mcp_catalog/read`,
//! and the MCP mutations a plugin server refuses answer around a session start
//! and a reload. One scenario's plugin declares MCP servers: the
//! `vibe-mcp-stdio-fixture` binary found on `PATH`, a script the plugin ships,
//! and a command that does not exist. Driving the
//! `vibe-app-server-stdio-fixture` binary through the same script with
//! `--server` yields observations normalized the same way, and the replay
//! compares the two scenario by scenario. It needs no reference checkout; only
//! the live probe at the end, which recaptures the reference, does.
//!
//! Every difference the replay finds has to fall under a `LEDGER` entry, and
//! every entry has to still reproduce, so row 35 of `docs/parity.md` is a
//! reading of the summary this file prints.

#![cfg(feature = "test-fixtures")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use vibe_core::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("plugins-parity/corpus.json");

const CAPTURE_SCRIPT: &str = "scripts/parity/plugins.py";

/// The scenarios the corpus may not fall below.
const SCENARIO_FLOOR: usize = 11;

/// A difference this port keeps in one scenario.
struct Divergence {
    scenario: &'static str,
    /// Every difference whose JSON pointer starts with this prefix is covered.
    pointer: &'static str,
    reason: &'static str,
}

/// Row 35: the unified session's MCP view.
const CONNECTOR_ERROR: &str = "since v2.26.0 a unified session's MCP view reports why its \
     connectors failed to load (`vibe/app_server/_unified_harness_backend_adapter.py:3444`, set \
     at `:3975-3980`, at `376f6a3`), here the bootstrap's HTTP 401, where it reported null; this \
     port's unified view still reports null";

/// Row 35: the unified session's plugin packages.
const BUNDLED_SERVER: &str = "since v2.26.0 the unified package store checks out a file the \
     plugin's own `mcp.json` launches with a `./` command as executable \
     (`harness/runtimes/python/python/mistralai_vibe_local_harness/vibe/plugins/_store.py:285-289,353-391` \
     at `376f6a3`), so the plugin's `script` server is kept with the tool it serves where it was \
     dropped, and the reload warns about one dropped server fewer; this port still drops it";

/// Row 35: the plugin servers `/mcp` lists.
const SESSION_CONNECTION: &str = "since v2.26.0 `/mcp` lists a plugin MCP server through the \
     session's own connection to it, with that connection's status and tools, and keeps a \
     discovery error only for a server the session holds no connection to \
     (`vibe/app_server/mcp_catalog.py:1020-1022,1034-1048` at `376f6a3`); this port lists the \
     discovery pass's status, tools and error for every plugin server, and the kept `script` \
     server answers there too";

const LEDGER: &[Divergence] = &[
    Divergence {
        scenario: "unified/dropped",
        pointer: "/1/answer/mcp/connectorError",
        reason: CONNECTOR_ERROR,
    },
    Divergence {
        scenario: "unified/mcp-servers",
        pointer: "/3/answer/mcp/connectorError",
        reason: CONNECTOR_ERROR,
    },
    Divergence {
        scenario: "unified/mcp-servers",
        pointer: "/4/answer/mcp/connectorError",
        reason: CONNECTOR_ERROR,
    },
    Divergence {
        scenario: "unified/only-builtin",
        pointer: "/3/answer/mcp/connectorError",
        reason: CONNECTOR_ERROR,
    },
    Divergence {
        scenario: "unified/only-builtin",
        pointer: "/4/answer/mcp/connectorError",
        reason: CONNECTOR_ERROR,
    },
    Divergence {
        scenario: "unified/reload",
        pointer: "/4/answer/mcp/connectorError",
        reason: CONNECTOR_ERROR,
    },
    Divergence {
        scenario: "unified/user-and-project",
        pointer: "/3/answer/mcp/connectorError",
        reason: CONNECTOR_ERROR,
    },
    Divergence {
        scenario: "unified/user-and-project",
        pointer: "/4/answer/mcp/connectorError",
        reason: CONNECTOR_ERROR,
    },
    Divergence {
        scenario: "unified/mcp-servers",
        pointer: "/0/answer/plugins/dropped",
        reason: BUNDLED_SERVER,
    },
    Divergence {
        scenario: "unified/mcp-servers",
        pointer: "/0/answer/plugins/plugins/0/components",
        reason: BUNDLED_SERVER,
    },
    Divergence {
        scenario: "unified/mcp-servers",
        pointer: "/1/answer/plugins/dropped",
        reason: BUNDLED_SERVER,
    },
    Divergence {
        scenario: "unified/mcp-servers",
        pointer: "/1/answer/plugins/plugins/0/components",
        reason: BUNDLED_SERVER,
    },
    Divergence {
        scenario: "unified/mcp-servers",
        pointer: "/2/answer/info/components",
        reason: BUNDLED_SERVER,
    },
    Divergence {
        scenario: "unified/mcp-servers",
        pointer: "/7/warnings",
        reason: BUNDLED_SERVER,
    },
    Divergence {
        scenario: "unified/mcp-servers",
        pointer: "/3/answer/mcp/discoveryErrors",
        reason: SESSION_CONNECTION,
    },
    Divergence {
        scenario: "unified/mcp-servers",
        pointer: "/3/answer/mcp/sources",
        reason: SESSION_CONNECTION,
    },
    Divergence {
        scenario: "unified/mcp-servers",
        pointer: "/4/answer/mcp/discoveryErrors",
        reason: SESSION_CONNECTION,
    },
    Divergence {
        scenario: "unified/mcp-servers",
        pointer: "/4/answer/mcp/sources",
        reason: SESSION_CONNECTION,
    },
];

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root resolves")
}

fn scenarios(corpus: &Value) -> BTreeMap<String, &Value> {
    corpus["scenarios"]
        .as_array()
        .expect("the corpus holds a scenario list")
        .iter()
        .map(|entry| {
            (
                entry["name"]
                    .as_str()
                    .expect("every scenario is named")
                    .to_owned(),
                entry,
            )
        })
        .collect()
}

fn differences(reference: &Value, port: &Value, pointer: &str, found: &mut Vec<String>) {
    match (reference, port) {
        (Value::Object(left), Value::Object(right)) => {
            let keys: BTreeSet<&String> = left.keys().chain(right.keys()).collect();
            for key in keys {
                let child = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => differences(left, right, &child, found),
                    _ => found.push(child),
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            for index in 0..left.len().max(right.len()) {
                let child = format!("{pointer}/{index}");
                match (left.get(index), right.get(index)) {
                    (Some(left), Some(right)) => differences(left, right, &child, found),
                    _ => found.push(child),
                }
            }
        }
        _ if reference != port => found.push(pointer.to_owned()),
        _ => {}
    }
}

fn capture(arguments: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new("python3")
        .arg(repository().join(CAPTURE_SCRIPT))
        .args(["--family", "server", "--mcp-fixture"])
        .arg(env!("CARGO_BIN_EXE_vibe-mcp-stdio-fixture"))
        .args(arguments)
        .current_dir(repository())
        .env_remove("FORCE_COLOR")
        .output()
        .expect("python3 runs the plugins capture script")
}

#[test]
fn the_port_serves_every_plugin_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    assert!(
        recorded.len() >= SCENARIO_FLOOR,
        "the corpus holds {} scenarios, below the floor of {SCENARIO_FLOOR}",
        recorded.len()
    );

    let output_path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("plugins-parity-port.json");
    let output = capture(&[
        "--server".as_ref(),
        env!("CARGO_BIN_EXE_vibe-app-server-stdio-fixture").as_ref(),
        "--output".as_ref(),
        output_path.as_os_str(),
    ]);
    assert!(
        output.status.success(),
        "the port capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replayed: Value = serde_json::from_str(
        &std::fs::read_to_string(&output_path).expect("the port capture is readable"),
    )
    .expect("the port capture parses");
    let replayed = scenarios(&replayed);
    assert_eq!(
        recorded.keys().collect::<Vec<_>>(),
        replayed.keys().collect::<Vec<_>>(),
        "the capture script declares other scenarios than the corpus records; recapture the \
         reference with `{CAPTURE_SCRIPT}`"
    );

    let mut unexplained = Vec::new();
    let mut reproduced = vec![0_usize; LEDGER.len()];
    let mut conformant = 0;
    for (name, reference) in &recorded {
        let port = replayed[name];
        assert_eq!(
            reference["scenario"], port["scenario"],
            "the capture script changed scenario {name} since the corpus was recorded; \
             recapture the reference with `{CAPTURE_SCRIPT}`"
        );
        let mut found = Vec::new();
        differences(&reference["observed"], &port["observed"], "", &mut found);
        if found.is_empty() {
            conformant += 1;
        }
        for pointer in found {
            match LEDGER
                .iter()
                .position(|entry| entry.scenario == name && pointer.starts_with(entry.pointer))
            {
                Some(index) => reproduced[index] += 1,
                None => unexplained.push(format!(
                    "{name} {pointer}\n  reference: {}\n  port:      {}",
                    reference["observed"]
                        .pointer(&pointer)
                        .map_or_else(|| "<absent>".to_owned(), Value::to_string),
                    port["observed"]
                        .pointer(&pointer)
                        .map_or_else(|| "<absent>".to_owned(), Value::to_string),
                )),
            }
        }
    }
    let stale: Vec<String> = LEDGER
        .iter()
        .zip(&reproduced)
        .filter(|(_, count)| **count == 0)
        .map(|(entry, _)| format!("{} {} ({})", entry.scenario, entry.pointer, entry.reason))
        .collect();
    println!(
        "plugins parity: {conformant}/{} scenarios conformant, {} ledgered differences across \
         {} entries",
        recorded.len(),
        reproduced.iter().sum::<usize>(),
        LEDGER.len()
    );
    assert!(
        unexplained.is_empty(),
        "the port departs from the corpus where no ledger entry says it may:\n{}",
        unexplained.join("\n")
    );
    assert!(
        stale.is_empty(),
        "these ledger entries no longer reproduce and should be removed: {stale:?}"
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "plugins") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let output = capture(&["--check".as_ref(), "--reference".as_ref(), root.as_os_str()]);
    assert!(
        output.status.success(),
        "the pinned reference no longer answers what the corpus records; regenerate it with \
         `{CAPTURE_SCRIPT}`: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
