//! How a tool configuration composes, and what it does with a value it cannot
//! carry.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use super::*;

fn settings(document: &str) -> Table {
    document.parse::<Table>().expect("the settings parse")
}

#[test]
fn a_tool_with_no_settings_resolves_to_its_declared_defaults() {
    let resolver = ToolConfigResolver::new();
    let grep: GrepConfig = resolver.view("grep");
    assert_eq!(grep.default_max_matches, 100);
    assert_eq!(grep.max_output_bytes, 64_000);
    assert_eq!(grep.default_timeout, 60);
    assert_eq!(grep.codeignore_file, ".vibeignore");
    assert_eq!(grep.exclude_patterns.len(), 23);
    assert_eq!(grep.shared.permission, PermissionMode::Always);
    assert_eq!(grep.shared.sensitive_patterns, ["**/.env", "**/.env.*"]);
    // The reference returns the class defaults untouched when neither an
    // override nor a session permission exists.
    assert_eq!(
        resolver.resolve("grep").into_table(),
        ToolConfigResolver::new().resolve("grep").into_table()
    );
}

#[test]
fn an_operator_setting_reaches_the_tool_that_declares_it() {
    let resolver = ToolConfigResolver::new();
    resolver.update(settings("[grep]\ndefault_max_matches = 500\n"));
    let grep: GrepConfig = resolver.view("grep");
    assert_eq!(grep.default_max_matches, 500);
    // The keys the operator left alone still answer with their defaults.
    assert_eq!(grep.max_output_bytes, 64_000);
}

#[test]
fn the_session_permission_override_wins_over_the_configured_one() {
    let resolver = ToolConfigResolver::new();
    resolver.update(settings("[bash]\npermission = \"always\"\n"));
    assert_eq!(
        resolver.resolve("bash").permission(),
        PermissionMode::Always,
        "the operator's table wins over the declared default"
    );
    let overridden = resolver.with_session_permissions(Arc::new(|tool| {
        (tool == "bash").then_some(PermissionMode::Never)
    }));
    assert_eq!(
        overridden.resolve("bash").permission(),
        PermissionMode::Never,
        "the session override wins over the operator's table"
    );
    assert_eq!(
        overridden.resolve("grep").permission(),
        PermissionMode::Always,
        "a tool the session does not override keeps its configured permission"
    );
}

#[test]
fn a_session_scoped_resolver_still_observes_a_configuration_change() {
    let resolver = ToolConfigResolver::new();
    let session = resolver
        .clone()
        .with_session_permissions(Arc::new(|_tool| None));
    resolver.update(settings("[todo]\nmax_todos = 7\n"));
    let todo: TodoConfig = session.view("todo");
    assert_eq!(
        todo.max_todos, 7,
        "the settings cache is shared with the resolver the session was built from"
    );
}

#[test]
fn a_key_no_tool_declares_is_carried_rather_than_dropped() {
    let resolver = ToolConfigResolver::new();
    resolver.update(settings("[grep]\nfuture_option = \"kept\"\n"));
    let resolved = resolver.resolve("grep");
    assert_eq!(
        resolved.value("future_option").and_then(Value::as_str),
        Some("kept")
    );
    assert_eq!(resolved.undeclared_keys(), ["future_option"]);
    assert!(resolver.diagnostics().is_empty());
}

#[test]
fn a_tool_no_declaration_covers_resolves_through_the_base_keys() {
    let resolver = ToolConfigResolver::new();
    resolver.update(settings("[mistral_mcp_search]\npermission = \"always\"\n"));
    let resolved = resolver.resolve("mistral_mcp_search");
    assert_eq!(resolved.permission(), PermissionMode::Always);
    assert!(resolved.strings("denylist").is_empty());
    assert_eq!(
        ToolConfigResolver::new()
            .resolve("mistral_mcp_search")
            .permission(),
        PermissionMode::Ask,
        "reference `BaseToolConfig.permission` is the fallback for an unknown tool"
    );
}

#[test]
fn a_value_of_the_wrong_type_falls_back_to_the_declared_default() {
    let resolver = ToolConfigResolver::new();
    resolver.update(settings("[grep]\ndefault_max_matches = \"many\"\n"));
    let grep: GrepConfig = resolver.view("grep");
    assert_eq!(grep.default_max_matches, 100);
    let diagnostics = resolver.diagnostics();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0],
        "tools.grep.default_max_matches expects an integer, found a string; using 100"
    );
    assert!(
        diagnostics[0].len() < 200,
        "a failure message stays inside the single line the accessibility requirement bounds it to"
    );
}

#[test]
fn a_limit_of_zero_or_less_is_refused_and_named() {
    let resolver = ToolConfigResolver::new();
    resolver.update(settings(
        "[todo]\nmax_todos = 0\n\n[read_file]\nmax_read_bytes = -1\n",
    ));
    let todo: TodoConfig = resolver.view("todo");
    let read: ReadFileConfig = resolver.view("read_file");
    assert_eq!(todo.max_todos, 100);
    assert_eq!(read.max_read_bytes, 51_200);
    let diagnostics = resolver.diagnostics();
    assert!(
        diagnostics.contains(&"tools.todo.max_todos must be positive; using 100".to_owned()),
        "{diagnostics:?}"
    );
    assert!(
        diagnostics
            .contains(&"tools.read_file.max_read_bytes must be positive; using 51200".to_owned()),
        "{diagnostics:?}"
    );
}

#[test]
fn a_number_the_other_numeric_kind_can_hold_exactly_is_accepted() {
    let resolver = ToolConfigResolver::new();
    resolver.update(settings(
        "[bash]\nmax_timeout_seconds = 900\ndefault_timeout = 120.0\n",
    ));
    let bash: ShellCommandConfig = resolver.view("bash");
    assert_eq!(bash.max_timeout_seconds, 900.0);
    assert_eq!(bash.default_timeout, 120);
    assert!(resolver.diagnostics().is_empty());

    // A float carrying a fraction is not a whole number of seconds.
    resolver.update(settings("[bash]\ndefault_timeout = 120.5\n"));
    assert_eq!(
        resolver.view::<ShellCommandConfig>("bash").default_timeout,
        300
    );
    assert_eq!(resolver.diagnostics().len(), 1);
}

#[test]
fn a_tools_entry_that_is_not_a_table_is_named_rather_than_obeyed() {
    let resolver = ToolConfigResolver::new();
    resolver.update(settings("grep = 3\n"));
    assert_eq!(resolver.view::<GrepConfig>("grep").default_max_matches, 100);
    assert_eq!(
        resolver.diagnostics(),
        ["tools.grep expects a table, found an integer; ignoring it"]
    );
}

#[test]
fn a_permission_is_read_case_insensitively_and_a_bad_one_is_refused() {
    let resolver = ToolConfigResolver::new();
    resolver.update(settings("[bash]\npermission = \"ALWAYS\"\n"));
    assert_eq!(
        resolver.resolve("bash").permission(),
        PermissionMode::Always
    );
    resolver.update(settings("[bash]\npermission = \"maybe\"\n"));
    assert_eq!(resolver.resolve("bash").permission(), PermissionMode::Ask);
    assert_eq!(
        resolver.diagnostics(),
        [
            "tools.bash.permission expects one of `ask`, `always` or `never`, found a string; using ask"
        ]
    );
}

#[test]
fn the_shell_lists_follow_the_branch_the_host_drives() {
    let posix = ToolConfigResolver::new().with_posix_shell(true);
    let windows = ToolConfigResolver::new().with_posix_shell(false);
    assert_eq!(
        posix
            .view::<ShellCommandConfig>("bash")
            .shared
            .allowlist
            .len(),
        44
    );
    assert_eq!(
        windows
            .view::<ShellCommandConfig>("powershell")
            .shared
            .allowlist
            .len(),
        13
    );
    // A Windows host driving Git Bash reads the POSIX lists, as
    // `uses_posix_shell` does. `ShellTools::register` is what narrows the
    // resolver to that branch, and it keeps the settings the operator wrote.
    windows.update(settings("[git_bash]\ndefault_timeout = 42\n"));
    let git_bash = windows.clone().with_posix_shell(true);
    assert_eq!(
        git_bash
            .view::<ShellCommandConfig>("git_bash")
            .denylist_standalone
            .len(),
        11
    );
    assert_eq!(
        git_bash
            .view::<ShellCommandConfig>("git_bash")
            .default_timeout,
        42,
        "narrowing the branch shares the settings cache rather than replacing it"
    );
}

#[test]
fn resolution_does_not_wait_behind_a_settings_write() {
    let resolver = ToolConfigResolver::new();
    resolver.update(settings("[todo]\nmax_todos = 7\n"));
    let held = resolver.settings.clone();
    let (blocked, release) = std::sync::mpsc::channel::<()>();
    let writer = thread::spawn(move || {
        let guard = write_through_poison(&held);
        blocked.send(()).expect("the reader is waiting");
        thread::sleep(Duration::from_millis(200));
        drop(guard);
    });
    release.recv().expect("the writer holds the lock");
    let started = Instant::now();
    let todo: TodoConfig = resolver.view("todo");
    let elapsed = started.elapsed().as_millis();
    assert!(
        elapsed < MAX_RESOLUTION_BLOCK_MILLIS,
        "resolution waited {elapsed} ms behind a settings write"
    );
    assert_eq!(
        todo.max_todos, 100,
        "a contended cache resolves to the declared defaults rather than blocking"
    );
    writer.join().expect("the writer finishes");
    assert_eq!(resolver.view::<TodoConfig>("todo").max_todos, 7);
}

#[test]
fn a_poisoned_settings_lock_still_resolves() {
    let resolver = ToolConfigResolver::new();
    resolver.update(settings("[todo]\nmax_todos = 9\n"));
    let poisoned = resolver.settings.clone();
    thread::spawn(move || {
        let _guard = write_through_poison(&poisoned);
        panic!("poisoning the settings lock");
    })
    .join()
    .expect_err("the thread panicked");
    assert!(resolver.settings.is_poisoned());
    assert_eq!(
        resolver.view::<TodoConfig>("todo").max_todos,
        9,
        "a poisoned lock is read through rather than panicked on"
    );
    resolver.update(settings("[todo]\nmax_todos = 11\n"));
    assert_eq!(resolver.view::<TodoConfig>("todo").max_todos, 11);
}

#[test]
fn the_published_defaults_carry_every_declared_tool() {
    let published = ToolConfigResolver::new().published_defaults();
    assert_eq!(published.len(), DECLARATIONS.len());
    let skill = published
        .get("skill")
        .and_then(Value::as_table)
        .expect("a tool declaring no limit still publishes its shared keys");
    assert_eq!(skill.len(), 4);
    assert_eq!(
        published
            .get("powershell")
            .and_then(Value::as_table)
            .map(Table::len),
        Some(9),
        "a Windows-only family is published on a POSIX host too, because the reference \
         enumerates declarations rather than the surface"
    );
}
