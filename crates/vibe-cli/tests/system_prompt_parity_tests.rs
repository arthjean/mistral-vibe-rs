//! Replays the committed system prompt corpus against this port.
//!
//! `scripts/parity/system_prompt.py` captured the corpus from the pinned
//! reference in three families. `compose` scenarios lay a tree out (a home
//! whose `.vibe` is the vibe home, a workspace, Git repositories, skills,
//! agents, prompt files and `AGENTS.md` documents) and record the sections of
//! the prompt a session opened there gets, each as the data it carries and its
//! prose as a length and a SHA-256. The replay lays the same tree out through
//! the script's `--materialize`, asks
//! [`WorkspaceService::system_prompt_inputs`] and
//! [`vibe_core::system_prompt::compose`] for the port's prompt, and records it
//! the same way. `os` scenarios fix a Windows host's published tools and
//! resolved shell. `live` scenarios run this crate's `vibe -p` through the
//! script's `--live-binary` and record what the system messages of every
//! request carry.
//!
//! The port writes its own prose (`NOTICE`), so two digests count as equal
//! wherever both sides hold one, and everything else is compared exactly.
//! A difference has to fall under a `LEDGER` entry, and every entry has to
//! still reproduce, so row 19 of `docs/parity.md` is a reading of the summary
//! this file prints. Only the live probe at the end needs the reference
//! checkout.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use vibe_app_server::workspace::{
    PromptHost, SessionPromptScope, SystemPromptError, WorkspacePaths, WorkspaceService,
};
use vibe_core::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};
use vibe_core::prompt::library::PromptFileError;
use vibe_core::system_prompt::{
    ComposedSystemPrompt, GitContext, ProjectContextSettings, ProjectInputs, SectionKind,
    ShellEnvironment, SystemPromptInputs, compose,
};
use vibe_core::tools::shell::WindowsShell;

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("system-prompt-parity/corpus.json");

const CAPTURE_SCRIPT: &str = "scripts/parity/system_prompt.py";

/// The scenarios the corpus may not fall below, so a recapture that lost
/// coverage fails here and not only on the machine that made it.
const SCENARIO_FLOOR: usize = 55;

const ROOT: &str = "<root>";
const DATE: &str = "<DATE>";
const CO_AUTHOR: &str = "Co-Authored-By: Mistral Vibe <vibe@mistral.ai>";

/// A difference the replay accepts, and why.
struct Divergence {
    /// The scenario it applies to, or `*` for every one.
    scenario: &'static str,
    /// A JSON pointer inside the scenario's observation, where a `*` segment
    /// matches any one segment.
    pointer: &'static str,
    reason: &'static str,
}

const LEDGER: &[Divergence] = &[];

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

fn is_prose(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == 2 && object.contains_key("prose") && object.contains_key("sha256")
    })
}

/// Every JSON pointer at which `port` departs from `reference`.
fn differences(reference: &Value, port: &Value, pointer: &str, found: &mut Vec<String>) {
    if is_prose(reference) && is_prose(port) {
        return;
    }
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

fn pointer_matches(pattern: &str, pointer: &str) -> bool {
    let pattern = pattern.split('/').collect::<Vec<_>>();
    let pointer = pointer.split('/').collect::<Vec<_>>();
    pattern.len() == pointer.len()
        && pattern
            .iter()
            .zip(&pointer)
            .all(|(pattern, segment)| *pattern == "*" || pattern == segment)
}

fn capture(arguments: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new("python3")
        .arg(repository().join(CAPTURE_SCRIPT))
        .args(arguments)
        .current_dir(repository())
        .env_remove("FORCE_COLOR")
        .output()
        .expect("python3 runs the system prompt capture script")
}

/// Replaces the scenario root, however it is spelled, with the placeholder the
/// corpus records.
struct Normalizer {
    roots: Vec<String>,
}

impl Normalizer {
    fn new(root: &Path) -> Self {
        let mut roots = vec![root.display().to_string()];
        if let Ok(resolved) = root.canonicalize() {
            roots.push(resolved.display().to_string());
        }
        roots.sort_by_key(|root| std::cmp::Reverse(root.len()));
        roots.dedup();
        Self { roots }
    }

    fn text(&self, value: &str) -> String {
        let mut value = value.to_owned();
        for root in &self.roots {
            value = value.replace(root.as_str(), ROOT);
        }
        value
    }

    fn path(&self, path: &Path) -> String {
        self.text(&path.display().to_string())
    }

    fn prose(&self, value: &str) -> Value {
        let text = self.text(value);
        json!({
            "prose": text.chars().count(),
            "sha256": Sha256::digest(text.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        })
    }

    /// A `git log --oneline` line with its abbreviated hash replaced.
    fn commit(&self, line: &str) -> String {
        let text = self.text(line);
        let hash = text
            .split(' ')
            .next()
            .filter(|hash| hash.len() >= 7 && hash.chars().all(|c| c.is_ascii_hexdigit()));
        match hash {
            Some(hash) => format!("<hash>{}", &text[hash.len()..]),
            None => text,
        }
    }
}

/// `Path(prompt_id).with_suffix(".md").name` for the identifiers the corpus
/// names.
fn prompt_file_name(prompt_id: &str) -> String {
    let stem = match prompt_id.rfind('.') {
        Some(index) if index > 0 => &prompt_id[..index],
        _ => prompt_id,
    };
    format!("{stem}.md")
}

fn base_data(
    inputs: &SystemPromptInputs,
    text: &str,
    directories: &[PathBuf],
    normalizer: &Normalizer,
) -> Value {
    let file_name = prompt_file_name(&inputs.prompt_id);
    for directory in directories {
        if directory.join(&file_name).is_file() {
            return json!({
                "promptId": inputs.prompt_id,
                "source": "file",
                "file": normalizer.path(&directory.join(&file_name)),
                "content": normalizer.text(text),
            });
        }
    }
    json!({
        "promptId": inputs.prompt_id,
        "source": "builtin",
        "dated": text.contains(DATE),
    })
}

fn os_data(platform: &str, shell: &ShellEnvironment, text: &str) -> Value {
    let (label, rules) = match shell {
        ShellEnvironment::Posix { shell } => (shell.clone(), Value::Null),
        ShellEnvironment::GitBashTool => ("Git Bash".to_owned(), json!("git_bash")),
        ShellEnvironment::PowerShellTool => ("PowerShell".to_owned(), json!("powershell")),
        ShellEnvironment::Bash { executable } => {
            (format!("bash ({executable})"), json!("git_bash"))
        }
        ShellEnvironment::Cmd { executable } => (executable.clone(), json!("cmd")),
    };
    assert!(
        text.contains(platform) && text.contains(&format!("`{label}`")),
        "the operating system section names neither {platform} nor `{label}`: {text}"
    );
    json!({"platform": platform, "shell": label, "rules": rules})
}

fn skills_data(inputs: &SystemPromptInputs, text: &str, normalizer: &Normalizer) -> Value {
    let mut model = inputs
        .skills
        .iter()
        .filter(|skill| skill.model_invocable)
        .collect::<Vec<_>>();
    model.sort_by(|left, right| left.name.cmp(&right.name));
    let entries = model
        .iter()
        .map(|skill| {
            let description = vibe_core::system_prompt::html_escape(&skill.description);
            assert!(
                text.contains(&format!("<name>{}</name>", skill.name))
                    && text.contains(&description),
                "the skills section does not list {}",
                skill.name
            );
            json!({
                "name": skill.name,
                "description": match skill.path {
                    Some(_) => json!(description),
                    None => normalizer.prose(&description),
                },
                "path": skill.path.as_deref().map(|path| {
                    let escaped = vibe_core::system_prompt::html_escape(&path.display().to_string());
                    assert!(text.contains(&escaped), "the skills section omits {escaped}");
                    json!(normalizer.path(path))
                }),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "modelIntro": inputs.skills.iter().any(|skill| skill.model_invocable),
        "userIntro": inputs.skills.iter().any(|skill| skill.user_invocable),
        "entries": entries,
    })
}

fn git_data(git: &GitContext, normalizer: &Normalizer) -> Value {
    match git {
        GitContext::Repository {
            current_branch,
            main_branch,
            recent_commits,
        } => json!({
            "kind": "repository",
            "currentBranch": current_branch,
            "mainBranch": main_branch,
            "commits": recent_commits
                .iter()
                .map(|line| normalizer.commit(line))
                .collect::<Vec<_>>(),
        }),
        GitContext::TimedOut => json!({"kind": "timed_out"}),
        GitContext::Unavailable => json!({"kind": "unavailable"}),
        GitContext::Failed(_) => json!({"kind": "failed"}),
    }
}

fn user_data(inputs: &ProjectInputs, normalizer: &Normalizer) -> Value {
    let (path, content) = &inputs.user_instructions;
    let content = content.trim();
    if content.is_empty() {
        Value::Null
    } else {
        json!({"path": normalizer.path(path), "content": content})
    }
}

/// The sections of `composed` as the corpus records them.
fn observed_sections(
    inputs: &SystemPromptInputs,
    composed: &ComposedSystemPrompt,
    prompt_directories: &[PathBuf],
    normalizer: &Normalizer,
) -> Value {
    let project = &inputs.project;
    let cwd = project
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| project.cwd.clone());
    let sections = composed
        .sections
        .iter()
        .map(|section| {
            let text = section.text.as_str();
            let data = match section.kind {
                SectionKind::Base => base_data(inputs, text, prompt_directories, normalizer),
                SectionKind::Headless => json!({}),
                SectionKind::CommitSignature => json!({"coAuthorTrailer": text.contains(CO_AUTHOR)}),
                SectionKind::ModelInfo => {
                    assert!(text.contains(&format!("`{}`", inputs.model_alias)));
                    json!({"alias": inputs.model_alias})
                }
                SectionKind::OperatingSystem => os_data(&inputs.platform, &inputs.shell, text),
                SectionKind::Skills => skills_data(inputs, text, normalizer),
                SectionKind::Subagents => json!({
                    "lines": text
                        .split('\n')
                        .filter(|line| line.starts_with("- "))
                        .collect::<Vec<_>>(),
                }),
                SectionKind::Scratchpad => {
                    let path = inputs.scratchpad.as_deref().expect("a scratchpad");
                    assert!(text.contains(&path.display().to_string()));
                    json!({"path": normalizer.path(path)})
                }
                SectionKind::ProjectContext => {
                    assert!(text.contains(&cwd.display().to_string()));
                    json!({
                        "absPath": normalizer.path(&cwd),
                        "git": git_data(composed.git.as_ref().expect("a snapshot"), normalizer),
                    })
                }
                SectionKind::DangerousDirectory => json!({
                    "absPath": normalizer.path(&cwd),
                    "description": composed.dangerous.expect("a description"),
                }),
                SectionKind::AdditionalDirectories => json!({
                    "directories": composed
                        .additional_directories
                        .iter()
                        .map(|directory| {
                            assert!(text.contains(&directory.display().to_string()));
                            normalizer.path(directory)
                        })
                        .collect::<Vec<_>>(),
                }),
                SectionKind::Instructions => json!({
                    "user": user_data(project, normalizer),
                    "project": project
                        .project_instructions
                        .iter()
                        .map(|(directory, content)| {
                            assert!(text.contains(content.trim()));
                            json!({"directory": normalizer.path(directory), "content": content.trim()})
                        })
                        .collect::<Vec<_>>(),
                }),
            };
            let mut entry = json!({"kind": section.kind.name(), "text": normalizer.prose(text)});
            if let (Some(entry), Value::Object(data)) = (entry.as_object_mut(), data) {
                entry.extend(data);
            }
            entry
        })
        .collect::<Vec<_>>();
    json!({"sections": sections})
}

fn error_data(error: SystemPromptError, normalizer: &Normalizer) -> Value {
    match error {
        SystemPromptError::Prompt(PromptFileError::Missing {
            prompt_id,
            builtins,
            directories,
            available,
            ..
        }) => json!({"error": {
            "kind": "missing",
            "promptId": prompt_id,
            "builtins": builtins,
            "directories": directories.iter().map(|d| normalizer.path(d)).collect::<Vec<_>>(),
            "available": available,
        }}),
        SystemPromptError::Prompt(PromptFileError::InvalidId { prompt_id, .. }) => {
            json!({"error": {"kind": "invalid", "promptId": prompt_id}})
        }
        SystemPromptError::Config(message) => {
            json!({"error": {"kind": "config", "message": message}})
        }
    }
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect()
}

/// The port's prompt for one `compose` scenario.
fn replay_compose(scenario: &Value) -> Value {
    let name = scenario["name"].as_str().expect("a name");
    let temporary = tempfile::tempdir().expect("a scenario root");
    let root = temporary.path().canonicalize().expect("the root resolves");
    let output = capture(&[
        "--materialize".as_ref(),
        name.as_ref(),
        "--root".as_ref(),
        root.as_os_str(),
    ]);
    assert!(
        output.status.success(),
        "{name}: materializing failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let normalizer = Normalizer::new(&root);
    let vibe_home = root.join("home/.vibe");
    let cwd = root.join(scenario["cwd"].as_str().expect("a working directory"));
    let trusted =
        vibe_core::trust::TrustStore::for_vibe_home(&vibe_home).is_trusted(&cwd) == Some(true);
    let service = WorkspaceService::new(
        WorkspacePaths {
            vibe_home: vibe_home.clone(),
            working_directory: cwd.clone(),
            session_root: root.join("sessions"),
        },
        trusted,
    )
    .expect("the workspace service opens");
    let agent = scenario["agent"].as_str().map(ToOwned::to_owned);
    let scope = SessionPromptScope {
        working_directory: cwd,
        trusted,
        add_directories: string_list(&scenario["addDirs"])
            .into_iter()
            .map(|directory| root.join(directory).display().to_string())
            .collect(),
        project_file_trust: None,
        scratchpad: (scenario["scratchpad"] == json!(true) && agent.is_none())
            .then(|| root.join("scratch")),
        agent,
        model: None,
        headless: scenario["headless"] == json!(true),
        tool_names: Vec::new(),
        skill_seed: None,
    };
    let host = PromptHost {
        home: Some(root.join("home")),
        current_date: DATE.to_owned(),
        platform: vibe_core::system_prompt::platform_display_name().to_owned(),
        shell: ShellEnvironment::Posix {
            shell: scenario["shell"].as_str().unwrap_or("sh").to_owned(),
        },
    };
    match service.system_prompt_inputs(&scope, &host) {
        Ok(inputs) => {
            let composed = compose(&inputs);
            let directories = service.layered_config().harness_files().prompts_dirs();
            observed_sections(&inputs, &composed, &directories, &normalizer)
        }
        Err(error) => error_data(error, &normalizer),
    }
}

/// The port's operating system section for one `os` scenario.
fn replay_os(scenario: &Value) -> Value {
    let resolved = &scenario["resolved"];
    let executable = PathBuf::from(resolved["executable"].as_str().expect("an executable"));
    let resolved = match resolved["kind"].as_str() {
        Some("bash") => WindowsShell::Bash(executable),
        _ => WindowsShell::Cmd(executable),
    };
    let shell = ShellEnvironment::for_windows(&string_list(&scenario["tools"]), resolved);
    let inputs = SystemPromptInputs {
        prompt_id: String::new(),
        base: String::new(),
        current_date: String::new(),
        headless: false,
        include_commit_signature: false,
        include_model_info: false,
        include_prompt_detail: true,
        include_project_context: false,
        model_alias: String::new(),
        platform: "Windows".to_owned(),
        shell: shell.clone(),
        skills: Vec::new(),
        subagents: Vec::new(),
        scratchpad: None,
        project: ProjectInputs {
            cwd: PathBuf::new(),
            home: None,
            settings: ProjectContextSettings::default(),
            project_roots: Vec::new(),
            user_instructions: (PathBuf::new(), String::new()),
            project_instructions: Vec::new(),
        },
    };
    let composed = compose(&inputs);
    let text = &composed
        .sections
        .iter()
        .find(|section| section.kind == SectionKind::OperatingSystem)
        .expect("an operating system section")
        .text;
    let normalizer = Normalizer { roots: Vec::new() };
    let mut section = json!({"kind": "operating_system", "text": normalizer.prose(text)});
    if let (Some(section), Value::Object(data)) =
        (section.as_object_mut(), os_data("Windows", &shell, text))
    {
        section.extend(data);
    }
    json!({"section": section})
}

/// The port's live requests, captured by the script driving this crate's
/// `vibe`.
fn replay_live() -> BTreeMap<String, Value> {
    let output_path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("system-prompt-live-port.json");
    let output = capture(&[
        "--live-binary".as_ref(),
        env!("CARGO_BIN_EXE_vibe").as_ref(),
        "--output".as_ref(),
        output_path.as_os_str(),
    ]);
    assert!(
        output.status.success(),
        "the port's live capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured: Value = serde_json::from_str(
        &std::fs::read_to_string(&output_path).expect("the live capture is readable"),
    )
    .expect("the live capture parses");
    scenarios(&captured)
        .into_iter()
        .map(|(name, entry)| (name, entry["observed"].clone()))
        .collect()
}

#[test]
fn the_port_composes_every_system_prompt_scenario_as_the_corpus_records_or_as_the_ledger_names() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let recorded = scenarios(&corpus);
    assert!(
        recorded.len() >= SCENARIO_FLOOR,
        "the corpus holds {} scenarios, below the floor of {SCENARIO_FLOOR}",
        recorded.len()
    );
    let live = replay_live();

    let mut unexplained = Vec::new();
    let mut reproduced = vec![0_usize; LEDGER.len()];
    let mut conformant = 0;
    for (name, reference) in &recorded {
        let scenario = &reference["scenario"];
        let family = reference["family"].as_str();
        let port = match family {
            Some("compose") => replay_compose(scenario),
            Some("os") => replay_os(scenario),
            _ => {
                assert_eq!(family, Some("live"), "{name}: unknown family");
                live.get(name)
                    .cloned()
                    .expect("the live capture holds every live scenario")
            }
        };
        let mut found = Vec::new();
        differences(&reference["observed"], &port, "", &mut found);
        if found.is_empty() {
            conformant += 1;
        }
        for pointer in found {
            match LEDGER.iter().position(|entry| {
                (entry.scenario == "*" || entry.scenario == name)
                    && pointer_matches(entry.pointer, &pointer)
            }) {
                Some(index) => reproduced[index] += 1,
                None => unexplained.push(format!(
                    "{name} {pointer}\n  reference: {}\n  port:      {}",
                    pointer_value(&reference["observed"], &pointer),
                    pointer_value(&port, &pointer),
                )),
            }
        }
    }
    let stale: Vec<&str> = LEDGER
        .iter()
        .zip(&reproduced)
        .filter(|(_, count)| **count == 0)
        .map(|(entry, _)| entry.pointer)
        .collect();
    println!(
        "system prompt parity: {conformant}/{} scenarios conformant, {} ledgered differences \
         across {} entries",
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

fn pointer_value(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .map_or_else(|| "(absent)".to_owned(), ToString::to_string)
}

#[test]
fn every_ledger_entry_states_a_pointer_and_a_reason() {
    for entry in LEDGER {
        assert!(
            entry.pointer.starts_with('/')
                && !entry.reason.is_empty()
                && !entry.scenario.is_empty(),
            "the ledger entry for {} needs a scenario, a pointer and a reason",
            entry.pointer
        );
    }
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "system prompt") {
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
