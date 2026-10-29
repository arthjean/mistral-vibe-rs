//! US-014: every published tool called with valid arguments, for a tool error,
//! and with invalid arguments, on both harnesses, and the tool surface the
//! first turn request offers.
//!
//! A tool message carries reference-authored text, which never enters this
//! repository, so each case commits a fingerprint instead: the SHA-256 of the
//! normalized tool message and the disk state the call left, measured against
//! the pinned reference. A trial that disagrees prints both normalized parts.

use std::path::Path;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use vibe_e2e::{Ctx, Entry, Outcome, Reply, Scenario, World, ensure};

use crate::cli::{chat_count, content_text, exits, vibe, with_role};

/// One tool call and the fingerprint the reference leaves.
pub struct Case {
    tool: &'static str,
    name: &'static str,
    /// The call's arguments as JSON, where `{work}` stands for the work tree
    /// and `{base}` for the platform.
    arguments: &'static str,
    fingerprint: &'static str,
}

const fn case(
    tool: &'static str,
    name: &'static str,
    arguments: &'static str,
    fingerprint: &'static str,
) -> Case {
    Case {
        tool,
        name,
        arguments,
        fingerprint,
    }
}

const UNIFIED: &[Case] = &[
    case(
        "read_file",
        "valid",
        r#"{"path":"seed.txt"}"#,
        "add42e7f0aff7d20",
    ),
    case(
        "read_file",
        "tool_error",
        r#"{"path":"missing.txt"}"#,
        "e19f80332f6e3e18",
    ),
    case(
        "read_file",
        "invalid_arguments",
        r#"{"path":5}"#,
        "9088082d2e00ab1d",
    ),
    case(
        "write_file",
        "valid",
        r#"{"path":"new.txt","content":"hi"}"#,
        "15773ddd0ebae13f",
    ),
    case(
        "write_file",
        "tool_error",
        r#"{"path":"adir","content":"x"}"#,
        "6fcac581f7e082d6",
    ),
    case(
        "write_file",
        "invalid_arguments",
        r#"{"path":"x.txt"}"#,
        "b98c6cb7d20f2848",
    ),
    case(
        "edit",
        "valid",
        r#"{"file_path":"seed.txt","old_string":"alpha","new_string":"beta"}"#,
        "0260db352fa8f0eb",
    ),
    case(
        "edit",
        "tool_error",
        r#"{"file_path":"seed.txt","old_string":"zulu","new_string":"beta"}"#,
        "01c925ef120fbb1c",
    ),
    case(
        "edit",
        "invalid_arguments",
        r#"{"file_path":"seed.txt","old_string":"","new_string":"beta"}"#,
        "029b6fb46c6757e5",
    ),
    case(
        "bash",
        "valid",
        r#"{"command":"echo hi"}"#,
        "bc7214f26d09f52b",
    ),
    case(
        "bash",
        "tool_error",
        r#"{"command":"echo oops >&2; exit 3"}"#,
        "a725006427c40e56",
    ),
    case("bash", "invalid_arguments", r#"{}"#, "6003c27040000ab6"),
    case(
        "todo",
        "valid",
        r#"{"action":"write","todos":[{"id":"1","content":"first"}]}"#,
        "120d0d26ca6dbd65",
    ),
    case(
        "todo",
        "tool_error",
        r#"{"action":"write","todos":[{"id":"1","content":"a"},{"id":"1","content":"b"}]}"#,
        "4cf7ce9905acfe6b",
    ),
    case(
        "todo",
        "invalid_arguments",
        r#"{"action":"bogus"}"#,
        "7f3cf36ad74a374b",
    ),
    case(
        "unified_harness_scratchpad",
        "valid",
        r#"{"action":"write","path":"n.md","content":"note"}"#,
        "592172cba084fe69",
    ),
    case(
        "unified_harness_scratchpad",
        "tool_error",
        r#"{"action":"read","path":"missing.md"}"#,
        "dc77259e0de9c73d",
    ),
    case(
        "unified_harness_scratchpad",
        "invalid_arguments",
        r#"{"action":"bogus"}"#,
        "6fdf90c47b85fd47",
    ),
    // `vibe -p` hosts no live session, so even a valid cron call fails.
    case("cron", "valid", r#"{"action":"list"}"#, "f9d14b5261699fcc"),
    case(
        "cron",
        "tool_error",
        r#"{"action":"cancel","id":"nope"}"#,
        "f9d14b5261699fcc",
    ),
    case(
        "cron",
        "invalid_arguments",
        r#"{"action":"schedule","interval_seconds":5,"prompt":"x"}"#,
        "838f40465a67b26e",
    ),
    case("skill", "valid", r#"{"name":"demo"}"#, "9d480d94ab62d7f8"),
    case(
        "skill",
        "tool_error",
        r#"{"name":"nosuch"}"#,
        "2e1b136157cbf564",
    ),
    case("skill", "invalid_arguments", r#"{}"#, "d55bb78493d46eba"),
    case(
        "search_tool_functions",
        "valid",
        r#"{"query":"read"}"#,
        "6f1c5e1c119a6135",
    ),
    case(
        "search_tool_functions",
        "tool_error",
        r#"{"mode":"details","functionNames":["nosuch"]}"#,
        "55a563c4f51e0de8",
    ),
    case(
        "search_tool_functions",
        "invalid_arguments",
        r#"{"mode":"bogus"}"#,
        "6736229b7b60b696",
    ),
    case(
        "run_typescript",
        "valid",
        r#"{"code":"return 1 + 1;"}"#,
        "9ce55e8536a5ac4a",
    ),
    case(
        "run_typescript",
        "tool_error",
        r#"{"code":"throw new Error('boom');"}"#,
        "edfdd3861c41afc2",
    ),
    case(
        "run_typescript",
        "invalid_arguments",
        r#"{}"#,
        "78659bca1da64e72",
    ),
    case(
        "no_such_tool",
        "unknown_tool",
        r#"{"x":1}"#,
        "ea58407fc7d04269",
    ),
];

const LEGACY: &[Case] = &[
    case(
        "read_file",
        "valid",
        r#"{"file_path":"{work}/seed.txt"}"#,
        "a03f7d20012eaef8",
    ),
    case(
        "read_file",
        "tool_error",
        r#"{"file_path":"{work}/missing.txt"}"#,
        "96b1fbe4b4ea1b29",
    ),
    case(
        "read_file",
        "invalid_arguments",
        r#"{"file_path":5}"#,
        "d462e00809705db9",
    ),
    case(
        "write_file",
        "valid",
        r#"{"file_path":"{work}/new.txt","content":"hi"}"#,
        "3627d739cdcd8292",
    ),
    case(
        "write_file",
        "tool_error",
        r#"{"file_path":"{work}/seed.txt","content":"over"}"#,
        "77090f78609115a1",
    ),
    case(
        "write_file",
        "invalid_arguments",
        r#"{"file_path":"{work}/x.txt"}"#,
        "48e669fe7c6f878b",
    ),
    case(
        "edit",
        "valid",
        r#"{"file_path":"{work}/seed.txt","old_string":"alpha","new_string":"beta"}"#,
        "456d00c867a7e29d",
    ),
    case(
        "edit",
        "tool_error",
        r#"{"file_path":"{work}/seed.txt","old_string":"zulu","new_string":"beta"}"#,
        "feded5ee65f555b4",
    ),
    case(
        "edit",
        "invalid_arguments",
        r#"{"file_path":"{work}/seed.txt"}"#,
        "b8b1c96bccf77f34",
    ),
    case(
        "grep",
        "valid",
        r#"{"pattern":"bravo"}"#,
        "a13ae506298b6319",
    ),
    case(
        "grep",
        "tool_error",
        r#"{"pattern":"bravo","path":"nope"}"#,
        "2bb34eac32b3c87c",
    ),
    case("grep", "invalid_arguments", r#"{}"#, "c25c3ca4ea7034a0"),
    case(
        "web_fetch",
        "valid",
        r#"{"url":"{base}/pages/hello"}"#,
        "28d1d8df6f3564db",
    ),
    // Nothing listens on the discard port inside the jail.
    case(
        "web_fetch",
        "tool_error",
        r#"{"url":"http://127.0.0.1:9/"}"#,
        "87eea64b67c00cce",
    ),
    case(
        "web_fetch",
        "invalid_arguments",
        r#"{}"#,
        "92ecc23ddb4378ac",
    ),
    case(
        "task",
        "valid",
        r#"{"task":"E2E-SUBAGENT-TASK look around"}"#,
        "717fa124c3319b93",
    ),
    case(
        "task",
        "tool_error",
        r#"{"task":"x","agent":"nosuch"}"#,
        "1f6bc600008db031",
    ),
    case("task", "invalid_arguments", r#"{}"#, "74db212209d28abe"),
    case(
        "no_such_tool",
        "unknown_tool",
        r#"{"x":1}"#,
        "bbce9e8f3c68a376",
    ),
];

/// The names the first turn request offers on each harness, with the hash of
/// each parameter schema once descriptions are removed.
const UNIFIED_SURFACE: &[(&str, &str)] = &[
    ("bash", "2bf4d1ef0d6931ba"),
    ("cron", "a4ef2f86bc6cc5db"),
    ("edit", "b3579d8a474f1856"),
    ("read_file", "38ca276832cd2efe"),
    ("run_typescript", "e5de9e79f2da6956"),
    ("search_tool_functions", "3716f302a7309cd3"),
    ("skill", "9d5f56c1393c2dc1"),
    ("todo", "560d987b7d098d79"),
    ("unified_harness_scratchpad", "c306f7bd3396774b"),
    ("write_file", "0863ca49d0670920"),
];
const LEGACY_SURFACE: &[(&str, &str)] = &[
    ("bash", "6c797b809dc35c38"),
    ("edit", "2616107c497c240b"),
    ("grep", "bc958eb82dbede2e"),
    ("read_file", "5dffa2464bcccea4"),
    ("skill", "098974972159a1c5"),
    ("task", "8b9eaa8be8a65729"),
    ("todo", "fa924280a4df836b"),
    ("web_fetch", "1d693eb21a920fbf"),
    ("write_file", "0c531496444a31d0"),
];

const SUBAGENT_MARKER: &str = "E2E-SUBAGENT-TASK";
const PAGE: &str = "<html><head><title>E2E page</title></head><body><h1>Hello page</h1>\
                    <p>PAGE-MARK body</p></body></html>";

pub fn scenarios() -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    for (harness, legacy, cases, surface) in [
        ("unified", false, UNIFIED, UNIFIED_SURFACE),
        ("legacy", true, LEGACY, LEGACY_SURFACE),
    ] {
        scenarios.push(Scenario::new(
            format!("tools::{harness}::offered_names_and_schemas"),
            move |ctx| offered(ctx, legacy, surface),
        ));
        for case in cases {
            let scenario = Scenario::new(
                format!("tools::{harness}::{}::{}", case.tool, case.name),
                move |ctx| call(ctx, legacy, case),
            );
            scenarios.push(
                if !legacy && case.tool == "run_typescript" && case.name == "invalid_arguments" {
                    scenario.known_defect(
                        "E2E-D38",
                        "`run_typescript` without `code` reports a schema violation, not the \
                     reference's refusal of a missing code string",
                    )
                } else {
                    scenario
                },
            );
        }
    }
    scenarios
}

/// A world holding the files every case reads, edits, or searches, and a
/// user skill.
fn seeded(ctx: &mut Ctx) -> Outcome<World> {
    let world = ctx.world()?;
    world.write("work/seed.txt", "alpha\nbravo\n")?;
    world.write("work/adir/keep", "")?;
    world.write(
        "vibe-home/skills/demo/SKILL.md",
        "---\nname: demo\ndescription: A demo skill for e2e\n---\n\nDEMO-SKILL-BODY-MARK\n",
    )?;
    Ok(world)
}

fn call(ctx: &mut Ctx, legacy: bool, case: &Case) -> Outcome {
    let world = seeded(ctx)?;
    world.platform().page("hello", PAGE);
    if case.tool == "task" {
        world
            .platform()
            .when(SUBAGENT_MARKER, Reply::text("subagent report"));
    }
    let arguments = resolve(&world, case.arguments)?;
    world
        .platform()
        .replies([Reply::tool_call(case.tool, arguments), Reply::text("done")]);
    world.platform().exact();
    let mut flags = vec!["-p", "go", "--output", "text", "--auto-approve"];
    if legacy {
        flags.push("--legacy-harness");
    }
    let output = vibe(&world, &flags)?;
    exits(&output, 0)?;
    ensure(output.stdout == "done\n", || {
        format!(
            "the turn did not continue to its answer: {:?}",
            output.stdout
        )
    })?;
    let last = turn_requests(&world)?
        .pop()
        .ok_or("the platform received no turn request")?;
    let message = tool_message(&last, "e2e000001")?;
    let normalized = normalize(&world, case.tool, legacy, &message);
    let disk = disk_state(&world);
    let found = fingerprint(&format!("{normalized}\n--\n{disk}"));
    ensure(found == case.fingerprint, || {
        format!(
            "fingerprint {found}, expected {:?}\ntool message:\n{normalized}\ndisk:\n{disk}",
            case.fingerprint
        )
    })
}

/// `arguments` with the world's paths in place of its placeholders.
fn resolve(world: &World, arguments: &str) -> Outcome<Value> {
    let escape = |text: &str| {
        let quoted = Value::String(text.to_owned()).to_string();
        quoted[1..quoted.len() - 1].to_owned()
    };
    let text = arguments
        .replace("{work}", &escape(&world.work().to_string_lossy()))
        .replace("{base}", &escape(world.platform().base()));
    Ok(serde_json::from_str(&text)?)
}

/// The turn requests, leaving out what a subagent sent through its marker.
pub fn turn_requests(world: &World) -> Outcome<Vec<Entry>> {
    Ok(world
        .platform()
        .chat_requests()
        .into_iter()
        .filter(|entry| entry.queue == "turn")
        .collect())
}

/// The content of the tool message answering `id`.
pub fn tool_message(request: &Entry, id: &str) -> Outcome<String> {
    with_role(request, "tool")
        .into_iter()
        .find(|m| m.get("tool_call_id").and_then(Value::as_str) == Some(id))
        .map(content_text)
        .ok_or_else(|| format!("no tool message answers {id}").into())
}

/// The first 16 hex digits of the SHA-256 of `text`.
pub fn fingerprint(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))[..16].to_owned()
}

/// What stays of a tool message once everything that legitimately differs
/// between two runs, or between the implementations, is removed.
pub fn normalize(world: &World, tool: &str, legacy: bool, message: &str) -> String {
    let root = world.root().to_string_lossy().into_owned();
    let text = message
        .replace(&root.replace('\\', "\\\\"), "<world>")
        .replace(&root, "<world>")
        .replace(world.platform().base(), "<platform>");
    let text = if cfg!(windows) {
        forward_slashes(&text)
    } else {
        text
    };
    match tool {
        "run_typescript" => without_stack_frames(&text),
        // `docs/parity.md` ledgers the two guidance lines that follow the base
        // directory as this port's own wording.
        "skill" => {
            let mut lines = Vec::new();
            let mut skip = 0;
            for line in text.lines() {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                if line.contains("<world>/vibe-home/skills/") {
                    skip = 2;
                }
                lines.push(line);
            }
            lines.join("\n")
        }
        // Best-match scores are computed over description prose, which the
        // licensing boundary keeps different; the candidates found compare.
        "search_tool_functions" if text.starts_with("---\nmode: best_match") => best_match(&text),
        _ if legacy => legacy_error(tool, &text),
        _ => text,
    }
}

/// A `run_typescript` failure keeps the first line of its stack: the frames
/// below it are the sandbox's own.
fn without_stack_frames(text: &str) -> String {
    let Some((head, error)) = text.split_once(": ") else {
        return text.to_owned();
    };
    let Ok(mut error) = serde_json::from_str::<Map<String, Value>>(error) else {
        return text.to_owned();
    };
    if let Some(Value::String(stack)) = error.get_mut("stack") {
        *stack = stack.lines().next().unwrap_or_default().to_owned();
    }
    format!("{head}: {}", Value::Object(error))
}

/// The front matter, the sorted candidate names, and the sorted connectors.
fn best_match(text: &str) -> String {
    let mut sections = text.splitn(3, "---\n");
    let front = sections.nth(1).unwrap_or_default();
    let body = sections.next().unwrap_or_default();
    let mut candidates: Vec<&str> = body
        .lines()
        .filter_map(|line| {
            let (rank, rest) = line.split_once(". ")?;
            rank.parse::<u32>().ok()?;
            rest.rsplit_once(" - ").map(|(name, _)| name)
        })
        .collect();
    candidates.sort_unstable();
    let mut connectors: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("<name>"))
        .collect();
    connectors.sort_unstable();
    format!(
        "{front}candidates: {}\nconnectors: {}",
        candidates.join(", "),
        connectors.join(", ")
    )
}

/// A legacy tool error keeps its framing, not its sentence: the port writes
/// its own (`docs/parity.md`, authored result and error text). The legacy
/// `edit` success sentence is ledgered the same way.
fn legacy_error(tool: &str, text: &str) -> String {
    if let Some(inner) = text
        .strip_prefix("<tool_error>")
        .and_then(|rest| rest.strip_suffix("</tool_error>"))
    {
        let mut parts = inner.splitn(3, ": ");
        let head = parts.next().unwrap_or_default();
        let kept = match (head == tool, parts.next(), parts.next()) {
            (true, Some(kind), Some(_)) => format!("{head}: {kind}"),
            (true, Some(rest), None) => format!("{head}: {rest}"),
            _ => head.to_owned(),
        };
        return format!("<tool_error>{kept}</tool_error>");
    }
    if tool == "edit" {
        return text
            .lines()
            .map(|line| {
                if line.starts_with("message: ") {
                    "message: <authored>"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    text.to_owned()
}

/// `text` with the separators of every path under `<world>` written as `/`,
/// as the expectations measured on Linux spell them.
fn forward_slashes(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<world>") {
        let (head, tail) = rest.split_at(start + "<world>".len());
        out.push_str(head);
        let end = tail
            .find(|c: char| c == '"' || c == '\'' || c == ')' || c == ',' || c.is_whitespace())
            .unwrap_or(tail.len());
        out.push_str(&tail[..end].replace("\\\\", "/").replace('\\', "/"));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Every file of the work tree outside `.git`, and of each session
/// scratchpad, with the hash of its content.
pub fn disk_state(world: &World) -> String {
    let mut lines = Vec::new();
    walk(&world.work(), &world.work(), "work", &mut lines);
    let sessions = world.vibe_home().join("logs/session/unified");
    for session in std::fs::read_dir(sessions).into_iter().flatten().flatten() {
        let scratchpad = session.path().join("scratchpad");
        walk(&scratchpad, &scratchpad, "scratchpad", &mut lines);
    }
    lines.sort();
    lines.join("\n")
}

fn walk(root: &Path, directory: &Path, label: &str, into: &mut Vec<String>) {
    for entry in std::fs::read_dir(directory).into_iter().flatten().flatten() {
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path);
        if relative.starts_with(".git") {
            continue;
        }
        let parts: Vec<_> = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect();
        let shown = format!("{label}/{}", parts.join("/"));
        if path.is_dir() {
            walk(root, &path, label, into);
        } else {
            let bytes = std::fs::read(&path).unwrap_or_default();
            into.push(format!("{shown} {}", fingerprint_bytes(&bytes)));
        }
    }
}

fn fingerprint_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))[..16].to_owned()
}

fn offered(ctx: &mut Ctx, legacy: bool, expected: &[(&str, &str)]) -> Outcome {
    let world = seeded(ctx)?;
    world.platform().reply(Reply::text("done"));
    world.platform().exact();
    let mut flags = vec!["-p", "go", "--output", "text"];
    if legacy {
        flags.push("--legacy-harness");
    }
    exits(&vibe(&world, &flags)?, 0)?;
    let requests = chat_count(&world, 1)?;
    let mut found: Vec<(String, String)> = requests[0]
        .body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|tool| {
            let function = &tool["function"];
            (
                function["name"].as_str().unwrap_or_default().to_owned(),
                fingerprint(&canonical(&without_descriptions(&function["parameters"]))),
            )
        })
        .collect();
    found.sort();
    let expected: Vec<(String, String)> = expected
        .iter()
        .map(|(name, hash)| ((*name).to_owned(), (*hash).to_owned()))
        .collect();
    ensure(found == expected, || {
        format!("the first turn offered {found:?}, expected {expected:?}")
    })
}

/// `schema` without its `description` keys, which carry authored prose.
fn without_descriptions(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| *key != "description")
                .map(|(key, value)| (key.clone(), without_descriptions(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_descriptions).collect()),
        other => other.clone(),
    }
}

/// `value` serialized with every object's keys sorted, whatever order the
/// workspace's `preserve_order` keeps.
pub fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|key| format!("{}:{}", Value::String(key.clone()), canonical(&map[key])))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        other => other.to_string(),
    }
}
