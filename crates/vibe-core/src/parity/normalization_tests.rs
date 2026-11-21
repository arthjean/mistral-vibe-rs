//! What the black-box drivers' normalization keeps, tested on the shared
//! modules under `scripts/parity/` without starting any subject.
//!
//! A normalization maps what both sides vary on (identities, clocks,
//! counters) to placeholders. Mapped too broadly, it erases the regression it
//! should expose: an integer where the reference sends a timestamp string, a
//! counter sequence that resets, a list that comes back empty or reordered.
//! These tests drive `blackbox.time_token`, the event counter ranks of
//! `app_server_client.Normalizer`, the list shape of
//! `app_server_protocol.shape` and the `--renormalize` pass of
//! `scripts/parity/sensitivity.py` through `python3`, which every host that
//! runs the replays already has.

use std::process::Command;

use super::replay::repository;

/// Runs `body` under `python3` with `scripts/parity` importable and returns
/// its standard output, failing with its standard error when it fails.
fn python(body: &str) -> String {
    let root = repository();
    let program = format!(
        "import sys\nsys.path.insert(0, {:?})\n{body}",
        root.join("scripts/parity").to_string_lossy()
    );
    let output = Command::new("python3")
        .arg("-c")
        .arg(&program)
        .current_dir(&root)
        .env_remove("FORCE_COLOR")
        .output()
        .expect("python3 runs");
    assert!(
        output.status.success(),
        "the normalization check failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

const PATHS: &str = "paths = {k: f'/nowhere/{k}' for k in \
                     ('workspace', 'vibeHome', 'home', 'root', 'bin', 'backend')}";

#[test]
fn a_time_value_keeps_its_type_class() {
    let answer = python(&format!(
        "import json, blackbox, app_server_client\n{PATHS}\n\
         values = ['2026-11-17T10:00:00Z', '2026-11-17 10:00:00.5+01:00', 'yesterday', \
         1763373600, 1763373600.5, None, True]\n\
         print(json.dumps([blackbox.time_token(v) for v in values]))\n\
         n = app_server_client.Normalizer({{}}, paths)\n\
         print(json.dumps([n.value({{'createdAt': v}}) for v in (1763373600, '2026-11-17T10:00:00Z')]))"
    ));
    let mut lines = answer.lines();
    assert_eq!(
        lines.next(),
        Some(r#"["<time>", "<time>", "<time:text>", "<time:int>", "<time:float>", null, true]"#)
    );
    assert_eq!(
        lines.next(),
        Some(r#"[{"createdAt": "<time:int>"}, {"createdAt": "<time>"}]"#),
        "an integer where the reference sends a timestamp string has to differ"
    );
}

#[test]
fn event_counters_become_their_rank_so_a_reset_differs() {
    let answer = python(&format!(
        "import json, app_server_client\n{PATHS}\n\
         def ranks(sequence):\n\
         \x20   n = app_server_client.Normalizer({{}}, paths)\n\
         \x20   steps = [{{'eventId': value}} for value in sequence]\n\
         \x20   n.collect_ids(steps)\n\
         \x20   return n.value(steps)\n\
         print(json.dumps([ranks([40, 41, 42]), ranks([7, 8, 9]), ranks([40, 41, 3]), ranks([0, 5])]))"
    ));
    assert_eq!(
        answer,
        r#"[[{"eventId": 1}, {"eventId": 2}, {"eventId": 3}], [{"eventId": 1}, {"eventId": 2}, {"eventId": 3}], [{"eventId": 2}, {"eventId": 3}, {"eventId": 1}], [{"eventId": 0}, {"eventId": 1}]]"#,
        "two monotonic runs rank alike whatever their clock, a reset does not, and zero stays zero"
    );
}

#[test]
fn a_list_shape_records_emptiness_and_the_order_of_its_kinds() {
    let answer = python(
        "import json, app_server_protocol as p\n\
         a = p.shape({'entries': [{'type': 'user', 'text': 'x'}, {'type': 'assistant', 'text': 'y'}]})\n\
         b = p.shape({'entries': [{'type': 'assistant', 'text': 'y'}, {'type': 'user', 'text': 'x'}]})\n\
         c = p.shape({'entries': [{'type': 'user', 'text': 'q'}, {'type': 'assistant', 'text': 'r'}]})\n\
         empty = p.shape({'entries': []})\n\
         print(json.dumps([a == b, a == c, a == empty, empty]))",
    );
    assert_eq!(
        answer, r#"[false, true, false, {"entries": {"<list>": null, "<empty>": true}}]"#,
        "reordered kinds and an empty list differ, other values of the same kinds do not"
    );
}

#[test]
fn the_renormalization_pass_reports_what_a_normalization_erases() {
    let answer = python(
        "import json, sensitivity\n\
         raw = {'scenarios': [{'name': 's', 'scenario': {}, 'raw': {'id': 'abc', 'size': 2}}]}\n\
         def erase_ids(scenario, run):\n\
         \x20   return {k: ('<id>' if k == 'id' else v) for k, v in run.items()}\n\
         report = sensitivity.renormalize(raw, erase_ids, ['change-value', 'delete-key'], None, 't')\n\
         print(json.dumps({op: [r['applied'], r['erased'], [e['pointer'] for e in r['examples']]] \
         for op, r in report['operators'].items()}, sort_keys=True))",
    );
    assert_eq!(
        answer, r#"{"change-value": [2, 1, ["/id"]], "delete-key": [2, 0, []]}"#,
        "a value the normalization maps to a placeholder is erased, a deleted key is not"
    );
}

#[test]
fn the_python_ranking_is_the_rust_one() {
    // Both halves sample the same sites of the same document.
    let answer =
        python("import sensitivity\nprint(sensitivity.rank(sensitivity.SEED, ['corpus', 'op']))");
    assert_eq!(
        answer,
        super::sensitivity::rank(super::sensitivity::SEED, &["corpus", "op"]).to_string()
    );
    let seed = python("import sensitivity\nprint(sensitivity.SEED)");
    assert_eq!(seed, super::sensitivity::SEED.to_string());
}
