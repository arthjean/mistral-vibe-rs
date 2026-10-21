use super::operands::inspects_paths;
use super::*;
use crate::platform::parse_policy_path;
use crate::policy::PermissionScope;

/// The lists a POSIX host resolves for `bash`, which is what every session
/// analyzes with.
fn posix_lists() -> ShellCommandLists {
    ShellCommandLists::from_config(
        &crate::tools::config::ToolConfigResolver::new()
            .with_posix_shell(true)
            .view("bash"),
    )
}

fn posix_context() -> ShellPolicyContext {
    ShellPolicyContext::new(
        Platform::Posix,
        parse_policy_path(Platform::Posix, "/work/project").expect("working directory"),
    )
}

fn analyze(command: &str) -> ShellAnalysis {
    analyze_shell(
        ShellFlavor::Posix,
        command,
        &posix_context(),
        &posix_lists(),
    )
}

/// The session patterns an analysis asks about, in order.
fn session_patterns(analysis: &ShellAnalysis) -> Vec<String> {
    analysis
        .requirements
        .iter()
        .map(|requirement| requirement.session_pattern.clone())
        .collect()
}

// ----------------------------------------------------------------------
// US-109: the grammar decides what runs
// ----------------------------------------------------------------------

/// US-109: a heredoc is a redirect, so the command under it is not the bare
/// standalone interpreter the denylist refuses. Its body is what runs, so
/// only the text as written can be approved.
#[test]
fn a_heredoc_is_not_a_bare_standalone_interpreter() {
    let command = "python3 <<'EOF'\nprint(1)\nEOF";
    let analysis = analyze(command);
    assert_ne!(
        analysis.mode,
        PermissionMode::Never,
        "a heredoc body is not a standalone `python3`: {:?}",
        analysis.rationale
    );
    assert_eq!(session_patterns(&analysis), vec![command.to_owned()]);
    assert!(analysis.requirements[0].literal);
}

/// US-109: each segment of a chain is analyzed on its own, so approving the
/// reader never approves what follows it.
#[test]
fn a_chain_is_analyzed_one_segment_at_a_time() {
    let analysis = analyze("cat README.md && rm -rf build");
    assert_eq!(analysis.commands.len(), 2);
    // `cat` is allowlisted and drops out; only `rm` is asked about.
    assert_eq!(session_patterns(&analysis), vec!["rm *".to_owned()]);
}

/// US-109: text the grammar finds no command in defers to the configured
/// permission rather than being granted.
#[test]
fn text_without_a_command_falls_back_to_the_configured_permission() {
    let analysis = analyze("");
    assert_eq!(analysis.mode, PermissionMode::Ask);
    assert!(analysis.requirements.is_empty());
    assert!(analysis.commands.is_empty());
}

// ----------------------------------------------------------------------
// US-110: the four lists, and nothing else, decide
// ----------------------------------------------------------------------

/// US-110: the denylist refuses, naming both the segment and the pattern.
#[test]
fn the_denylist_refuses_and_names_what_matched() {
    for (command, pattern) in [
        ("vim notes.txt", "vim"),
        ("nano", "nano"),
        ("tmux", "tmux"),
        ("gdb ./binary", "gdb"),
        ("passwd", "passwd"),
        ("bash -i", "bash -i"),
    ] {
        let analysis = analyze(command);
        assert_eq!(
            analysis.mode,
            PermissionMode::Never,
            "`{command}` is on the reference denylist: {:?}",
            analysis.rationale
        );
        let reason = analysis.rationale.join("; ");
        assert!(
            reason.contains(pattern),
            "`{command}` refused without naming `{pattern}`: {reason}"
        );
    }
}

/// US-110: the standalone denylist refuses the bare interpreter by its whole
/// text or its basename, and refuses nothing that carries an argument.
#[test]
fn the_standalone_denylist_refuses_only_a_single_word() {
    for command in ["python3", "/usr/bin/python3", "su", "sh"] {
        assert_eq!(
            analyze(command).mode,
            PermissionMode::Never,
            "`{command}` is a bare standalone interpreter"
        );
    }
    for command in ["python3 script.py", "sh build.sh"] {
        assert_ne!(
            analyze(command).mode,
            PermissionMode::Never,
            "`{command}` carries an argument, so the standalone rule misses it"
        );
    }
}

/// US-110: the five commands this port used to refuse outright now reach an
/// approval prompt, because no denylist entry matches any of them.
///
/// One named case per loosening, which is what makes the change visible in
/// the suite rather than buried in a list diff.
#[test]
fn rm_now_reaches_an_approval_prompt() {
    let analysis = analyze("rm -rf build/");
    assert_eq!(
        analysis.mode,
        PermissionMode::Ask,
        "{:?}",
        analysis.rationale
    );
    assert_eq!(session_patterns(&analysis), vec!["rm *".to_owned()]);
}

#[test]
fn dd_now_reaches_an_approval_prompt() {
    let analysis = analyze("dd if=/dev/zero of=disk.img");
    assert_eq!(analysis.mode, PermissionMode::Ask);
    assert!(session_patterns(&analysis).contains(&"dd *".to_owned()));
}

#[test]
fn mkfs_now_reaches_an_approval_prompt() {
    let analysis = analyze("mkfs.ext4 /dev/sdb1");
    assert_eq!(analysis.mode, PermissionMode::Ask);
    assert!(session_patterns(&analysis).contains(&"mkfs.ext4 *".to_owned()));
}

#[test]
fn shutdown_now_reaches_an_approval_prompt() {
    let analysis = analyze("shutdown -h now");
    assert_eq!(analysis.mode, PermissionMode::Ask);
    assert_eq!(session_patterns(&analysis), vec!["shutdown *".to_owned()]);
}

#[test]
fn eval_now_reaches_an_approval_prompt() {
    let analysis = analyze("eval echo hi");
    assert_eq!(analysis.mode, PermissionMode::Ask);
    assert!(session_patterns(&analysis).contains(&"eval *".to_owned()));
}

/// The sixth loosening this epic carries: `git reset --hard` is on no
/// denylist, so it is asked about rather than refused.
#[test]
fn a_destructive_git_reset_now_reaches_an_approval_prompt() {
    for command in ["git reset --hard", "git reset --hard -- src"] {
        let analysis = analyze(command);
        assert_eq!(
            analysis.mode,
            PermissionMode::Ask,
            "`{command}`: {:?}",
            analysis.rationale
        );
        assert!(
            !analysis.requirements.is_empty(),
            "`{command}` must leave something to approve"
        );
    }
}

/// US-110: a sensitive first word always produces a requirement, is never
/// covered by an allowlist match, and never widens past itself.
#[test]
fn a_sensitive_first_word_is_always_asked_about() {
    let analysis = analyze("sudo ls");
    assert_eq!(analysis.mode, PermissionMode::Ask);
    assert_eq!(session_patterns(&analysis), vec!["sudo ls".to_owned()]);
    assert!(
        analysis
            .rationale
            .iter()
            .any(|line| line.contains("sensitive pattern `sudo`")),
        "{:?}",
        analysis.rationale
    );

    // Even at permission `always`, which grants everything else outright.
    let mut lists = posix_lists();
    lists.permission = PermissionMode::Always;
    let granted = analyze_shell(ShellFlavor::Posix, "ls -la", &posix_context(), &lists);
    assert_eq!(granted.mode, PermissionMode::Always);
    let sensitive = analyze_shell(ShellFlavor::Posix, "sudo ls", &posix_context(), &lists);
    assert_eq!(sensitive.mode, PermissionMode::Ask);
}

/// US-110: `find` running a program is asked about under the whole segment,
/// even though `find` is allowlisted. `{}` is syntax that needs approval,
/// so the part is asked about beside the guardrail, and neither list
/// deduplicates against the other, as upstream.
#[test]
fn find_running_a_program_is_asked_about_once_per_segment() {
    for predicate in ["-exec", "-execdir", "-ok", "-okdir"] {
        let analysis = analyze(&format!("find . {predicate} rm {{}} ;"));
        assert_eq!(
            analysis.mode,
            PermissionMode::Ask,
            "`find {predicate}` must ask: {:?}",
            analysis.rationale
        );
        let requirement = analysis
            .requirements
            .first()
            .expect("a find execution raises a requirement");
        assert_eq!(requirement.scope, PermissionScope::CommandPattern);
        assert!(
            requirement.invocation_pattern.contains(predicate),
            "the requirement carries the whole segment: {requirement:?}"
        );
        // The grant may not widen: the segment is its own session pattern.
        assert_eq!(requirement.invocation_pattern, requirement.session_pattern);
    }
    // Deleting or writing a file is gated the same way.
    for command in ["find . -delete", "find . -fprint out.txt"] {
        let analysis = analyze(command);
        assert_eq!(analysis.mode, PermissionMode::Ask, "`{command}` must ask");
        assert_eq!(
            analysis.requirements,
            vec![PermissionRequirement::exact_command(command)]
        );
    }
    // A plain walk stays allowlisted.
    assert_eq!(analyze("find . -name '*.rs'").mode, PermissionMode::Always);
    // The same segment twice is one approval per list.
    let repeated = analyze("find . -exec rm {} \\; && find . -exec rm {} \\;");
    assert_eq!(
        repeated.requirements.len(),
        2,
        "{:?}",
        repeated.requirements
    );
}

/// Text the grammar cannot parse, or that a line continuation rewrites
/// before the shell reads it, is never granted: the extracted segments no
/// longer describe what runs, so the text as written is the only scope.
#[test]
fn syntax_the_segments_do_not_describe_is_asked_about_as_written() {
    for command in ["cat 'unterminated", "cat file.txt \\\nsecret"] {
        let analysis = analyze(command);
        assert_eq!(analysis.mode, PermissionMode::Ask, "`{command}` must ask");
        let [requirement] = analysis.requirements.as_slice() else {
            panic!("one whole-command requirement: {:?}", analysis.requirements);
        };
        assert_eq!(requirement.invocation_pattern, command);
        assert_eq!(requirement.session_pattern, command);
    }
}

/// US-110: an operator who empties the allowlist is asked per segment rather
/// than losing the tool.
#[test]
fn an_emptied_allowlist_asks_per_segment() {
    let lists = ShellCommandLists::default();
    let analysis = analyze_shell(
        ShellFlavor::Posix,
        "ls -la && cat README.md",
        &posix_context(),
        &lists,
    );
    assert_eq!(analysis.mode, PermissionMode::Ask);
    assert_eq!(
        session_patterns(&analysis),
        vec!["ls *".to_owned(), "cat *".to_owned()]
    );
    // An emptied denylist stops refusing what it used to refuse, which is
    // the operator's decision rather than a hardcoded one.
    assert_ne!(
        analyze_shell(
            ShellFlavor::Posix,
            "vim notes.txt",
            &posix_context(),
            &lists
        )
        .mode,
        PermissionMode::Never
    );
}

/// US-110: two segments reducing to the same session pattern are one
/// approval, not two.
#[test]
fn segments_sharing_a_session_pattern_are_asked_once() {
    let analysis = analyze("cargo build && cargo build --release");
    assert_eq!(
        session_patterns(&analysis),
        vec!["cargo build *".to_owned()]
    );
    let distinct = analyze("cargo build && cargo test");
    assert_eq!(
        session_patterns(&distinct),
        vec!["cargo build *".to_owned(), "cargo test *".to_owned()]
    );
}

// ----------------------------------------------------------------------
// US-111: the operands that leave the workspace
// ----------------------------------------------------------------------

/// US-111: an allowlisted reader pointed outside the roots asks, under an
/// `outside_directory` requirement naming the path itself and granting it
/// by its exact encoded form.
#[test]
fn an_allowlisted_reader_pointed_outside_the_roots_still_asks() {
    let inside = analyze("wc -l /work/project/notes.txt");
    assert_eq!(
        inside.mode,
        PermissionMode::Always,
        "`wc` is on the reference read-only allowlist: {:?}",
        inside.rationale
    );

    let outside = analyze("grep secret /etc/passwd");
    assert_eq!(outside.mode, PermissionMode::Ask);
    let requirement = outside
        .requirements
        .iter()
        .find(|requirement| requirement.scope == PermissionScope::OutsideDirectory)
        .expect("an escaping operand raises an outside-directory requirement");
    assert_eq!(requirement.invocation_pattern, "/etc/passwd");
    assert_eq!(requirement.session_pattern, "vibe-path:exact:/etc/passwd");
    assert_eq!(requirement.label, "outside workdir (/etc/passwd)");
    assert_eq!(
        requirement.path_scope_root, None,
        "a file offers no recursive root"
    );
}

/// An outside directory offers itself as the root a recursive grant would
/// reach, which a file never does.
#[cfg(unix)]
#[test]
fn an_outside_directory_offers_itself_as_the_recursive_root() {
    let analysis = analyze("ls /etc");
    let requirement = analysis
        .requirements
        .iter()
        .find(|requirement| requirement.scope == PermissionScope::OutsideDirectory)
        .expect("an outside directory raises a requirement");
    assert_eq!(requirement.invocation_pattern, "/etc");
    assert_eq!(requirement.path_scope_root.as_deref(), Some("/etc"));
}

/// The shell reads back the path grants its allowlist holds, the encoded
/// ones a permanent approval writes and an absolute path glob, and never a
/// command entry.
#[test]
fn the_allowlist_grants_outside_paths_by_path_only() {
    let resolve = |entries: &[&str], command: &str| {
        let mut lists = posix_lists();
        lists.allowlist = entries.iter().map(|entry| (*entry).to_owned()).collect();
        analyze_shell(ShellFlavor::Posix, command, &posix_context(), &lists)
    };
    let exact = resolve(
        &["cat", "vibe-path:exact:/srv/data/a.txt"],
        "cat /srv/data/a.txt",
    );
    assert_eq!(
        exact.mode,
        PermissionMode::Always,
        "{:?}",
        exact.requirements
    );

    let sibling = resolve(
        &["cat", "vibe-path:exact:/srv/data/a.txt"],
        "cat /srv/data/b.txt",
    );
    assert_eq!(sibling.mode, PermissionMode::Ask);
    assert_eq!(
        session_patterns(&sibling),
        vec!["vibe-path:exact:/srv/data/b.txt".to_owned()]
    );

    let recursive = resolve(
        &["cat", "vibe-path:directory_recursive:/srv"],
        "cat /srv/data/a.txt",
    );
    assert_eq!(recursive.mode, PermissionMode::Always);

    let glob = resolve(&["cat", "/srv/data/*"], "cat /srv/data/a.txt");
    assert_eq!(glob.mode, PermissionMode::Always);
    let nested = resolve(&["cat", "/srv/data/*"], "cat /srv/data/sub/a.txt");
    assert_eq!(
        nested.mode,
        PermissionMode::Ask,
        "a `*` never crosses a separator"
    );

    let wildcard = resolve(&["cat", "*"], "cat /srv/data/a.txt");
    assert_eq!(
        wildcard.mode,
        PermissionMode::Ask,
        "a command wildcard is not a path grant"
    );
}

/// US-111: a flag and a `chmod` mode are not paths.
#[test]
fn flags_and_chmod_modes_are_not_resolved_as_paths() {
    let flagged = analyze("ls --color /work/project");
    assert_eq!(
        flagged.mode,
        PermissionMode::Always,
        "{:?}",
        flagged.rationale
    );
    let mode = analyze("chmod +x /work/project/script.sh");
    assert!(
        mode.requirements
            .iter()
            .all(|requirement| requirement.scope != PermissionScope::OutsideDirectory),
        "`+x` is a mode, not a path: {:?}",
        mode.requirements
    );
}

/// US-111: every outside path is its own approval, in sorted order, and
/// one path reached twice is asked about once.
#[test]
fn every_outside_path_is_its_own_approval() {
    let analysis = analyze("cat /srv/b.txt /srv/a.txt && cat /srv/b.txt");
    let outside = analysis
        .requirements
        .iter()
        .filter(|requirement| requirement.scope == PermissionScope::OutsideDirectory)
        .map(|requirement| requirement.invocation_pattern.as_str())
        .collect::<Vec<_>>();
    assert_eq!(outside, ["/srv/a.txt", "/srv/b.txt"]);
}

/// US-111: a command that inspects no path never resolves its arguments,
/// which is why `echo` naming a file outside is not an approval.
#[test]
fn a_command_outside_the_path_set_has_no_operands_resolved() {
    let analysis = analyze("echo /etc/passwd");
    assert_eq!(
        analysis.mode,
        PermissionMode::Always,
        "{:?}",
        analysis.rationale
    );
    assert!(analysis.path_operands.is_empty());
}

/// US-111: the inspected set stays a superset of the read-only allowlist, so
/// no auto-allowed reader escapes the operand walk.
#[test]
fn every_read_only_allowlist_command_has_its_operands_inspected() {
    let uninspected = crate::tools::config::shell_read_only_commands(true)
        .iter()
        .filter(|program| !inspects_paths(program))
        .collect::<Vec<_>>();
    assert!(
        uninspected.is_empty(),
        "read-only commands whose operands are never inspected: {uninspected:?}"
    );
    // The reference set is the union with the eight mutating commands, and
    // nothing else: a command only the shared allowlist names is not
    // inspected, which is what keeps `echo /etc/passwd` an allowed echo.
    assert!(!inspects_paths("echo"));
    assert!(inspects_paths("rm"));
}

/// US-111: a path inside the scratchpad raises nothing, because it is the
/// runtime's own capability rather than the operator's workspace.
#[test]
fn a_scratchpad_operand_raises_no_requirement() {
    let workspace = tempfile::tempdir().expect("workspace");
    let scratchpad = tempfile::tempdir().expect("scratchpad");
    std::fs::write(scratchpad.path().join("note.txt"), "note").expect("note");
    let root = workspace.path().to_string_lossy().into_owned();
    let context = ShellPolicyContext::new(
        Platform::Posix,
        parse_policy_path(Platform::Posix, &root).expect("root"),
    )
    .with_scratchpad(Some(scratchpad.path().to_path_buf()));

    let note = scratchpad.path().join("note.txt");
    let analysis = analyze_shell(
        ShellFlavor::Posix,
        &format!("cat {}", note.display()),
        &context,
        &posix_lists(),
    );
    assert_eq!(
        analysis.mode,
        PermissionMode::Always,
        "the scratchpad is granted before any list: {:?}",
        analysis.rationale
    );
}

/// The analysis a call gets on this host's shell: Git Bash over a Windows
/// working directory on Windows, which is what the published family runs
/// there, and [`analyze`] elsewhere.
fn analyze_on_host(command: &str) -> ShellAnalysis {
    if !cfg!(windows) {
        return analyze(command);
    }
    let context = ShellPolicyContext::new(
        Platform::Windows,
        parse_policy_path(Platform::Windows, r"C:\work\project").expect("working directory"),
    );
    analyze_shell(ShellFlavor::GitBash, command, &context, &posix_lists())
}

/// US-111: a `~` operand is expanded before it is positioned, so a home-relative
/// read is measured where it actually reads.
///
/// On Windows the home is the one every entry point resolves. Git Bash
/// reads `~\` as an escape, so the reference extracts no operand from
/// `cat ~\.ssh\id_rsa` and asks about the unmodeled word, while PowerShell
/// expands `~\` as it expands `~/`.
#[test]
fn a_home_relative_operand_is_expanded_before_it_is_positioned() {
    let Some(home) = crate::config::user_home_directory() else {
        eprintln!("skipping: the environment names no home directory");
        return;
    };
    let expected = crate::platform::strip_verbatim_prefix(
        &crate::platform::resolve_lenient(&home.join(".ssh").join("id_rsa")).to_string_lossy(),
    );
    let names_the_home = |analysis: &ShellAnalysis| {
        analysis
            .requirements
            .iter()
            .any(|requirement| requirement.invocation_pattern == expected)
    };
    let analysis = analyze_on_host("cat ~/.ssh/id_rsa");
    assert_eq!(
        analysis.mode,
        PermissionMode::Ask,
        "{:?}",
        analysis.rationale
    );
    assert!(
        names_the_home(&analysis),
        "expected `{expected}` among {:?}",
        analysis.requirements
    );
    if !cfg!(windows) {
        return;
    }
    let escaped = analyze_on_host(r"cat ~\.ssh\id_rsa");
    assert_eq!(
        escaped.mode,
        PermissionMode::Ask,
        "an escaped home operand is asked about, never granted: {:?}",
        escaped.rationale
    );
    let context = ShellPolicyContext::new(
        Platform::Windows,
        parse_policy_path(Platform::Windows, r"C:\work\project").expect("working directory"),
    );
    let powershell = analyze_shell(
        ShellFlavor::PowerShell,
        r"Get-Content ~\.ssh\id_rsa",
        &context,
        &posix_lists(),
    );
    assert_eq!(powershell.mode, PermissionMode::Ask, "{powershell:?}");
    assert!(
        names_the_home(&powershell),
        "expected `{expected}` among {:?}",
        powershell.requirements
    );
}

/// US-111: an operand that ascends is folded before it is positioned, so it
/// is measured and named where it actually reads.
#[test]
fn an_ascending_operand_is_folded_before_it_is_positioned() {
    let outside = analyze("cat ../elsewhere/secret.txt");
    assert_eq!(outside.mode, PermissionMode::Ask, "{:?}", outside.rationale);
    let requirement = outside
        .requirements
        .iter()
        .find(|requirement| requirement.scope == PermissionScope::OutsideDirectory)
        .expect("an ascending operand leaves the workspace");
    assert_eq!(requirement.invocation_pattern, "/work/elsewhere/secret.txt");
    assert_eq!(
        requirement.session_pattern,
        "vibe-path:exact:/work/elsewhere/secret.txt"
    );

    // An ascent that lands back inside raises nothing, which is what keeps a
    // relative read of a sibling directory from asking.
    let inside = analyze("cat sub/../notes.txt");
    assert_eq!(
        inside.mode,
        PermissionMode::Always,
        "`sub/../notes.txt` never left the workspace: {:?}",
        inside.rationale
    );

    // An ascent past the root stops at it rather than failing to position.
    let root = analyze("cat ../../../../etc/passwd");
    assert!(
        root.requirements
            .iter()
            .any(|requirement| requirement.invocation_pattern == "/etc/passwd"),
        "{:?}",
        root.requirements
    );
}

#[cfg(unix)]
#[test]
fn symlinked_reader_operand_outside_root_requires_approval() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("secret"), "secret").expect("outside file");
    symlink(outside.path(), root.path().join("outside")).expect("symlink");
    let root_text = root.path().to_string_lossy().into_owned();
    let context = ShellPolicyContext::new(
        Platform::Posix,
        parse_policy_path(Platform::Posix, &root_text).expect("root"),
    );

    let analysis = analyze_shell(
        ShellFlavor::Posix,
        "cat outside/secret",
        &context,
        &posix_lists(),
    );

    assert_eq!(analysis.mode, PermissionMode::Ask);
    assert!(
        analysis
            .requirements
            .iter()
            .any(|requirement| requirement.scope == PermissionScope::OutsideDirectory),
        "a symlink out of the workspace is still out of it: {:?}",
        analysis.requirements
    );
}

// ----------------------------------------------------------------------
// The guards this port keeps beyond the reference set
// ----------------------------------------------------------------------

/// Each of them withholds the automatic grant and nothing more: the segment
/// is asked about under its own pattern, never refused.
#[test]
fn the_extra_guards_ask_rather_than_refuse() {
    for command in [
        "cat $(credential-helper)",
        "cat README.md > /etc/motd",
        "git diff --no-index /etc/passwd /dev/null",
        "git -c core.pager=sh log",
        "rg --pre malicious-helper needle .",
    ] {
        let analysis = analyze(command);
        assert_eq!(
            analysis.mode,
            PermissionMode::Ask,
            "`{command}`: {:?}",
            analysis.rationale
        );
        assert!(
            !analysis.requirements.is_empty(),
            "`{command}` must leave something to approve"
        );
    }
}

#[test]
fn windows_shells_handle_aliases_drives_unc_and_ambiguity() {
    let windows_lists = ShellCommandLists::from_config(
        &crate::tools::config::ToolConfigResolver::new()
            .with_posix_shell(false)
            .view("powershell"),
    );
    let context = ShellPolicyContext::new(
        Platform::Windows,
        parse_policy_path(Platform::Windows, r"C:\work\project").expect("cwd"),
    );
    let safe = analyze_shell(
        ShellFlavor::PowerShell,
        r"type C:\work\project\README.md",
        &context,
        &windows_lists,
    );
    assert_eq!(safe.mode, PermissionMode::Always, "{:?}", safe.rationale);
    // A `cmd.exe` host is not a POSIX shell, where the reference `bash`
    // tool resolves nothing and the configured permission applies.
    let deferred = analyze_shell(
        ShellFlavor::Cmd,
        r"type C:\work\project\README.md",
        &context,
        &windows_lists,
    );
    assert_eq!(deferred.mode, windows_lists.permission);
    assert!(deferred.requirements.is_empty());
    let unc = analyze_shell(
        ShellFlavor::PowerShell,
        r"type \\server\share\secret.txt",
        &context,
        &windows_lists,
    );
    assert_eq!(unc.mode, PermissionMode::Ask);
    let provider = analyze_shell(
        ShellFlavor::PowerShell,
        r"type Env:\SECRET",
        &context,
        &windows_lists,
    );
    assert_eq!(provider.mode, PermissionMode::Ask);
}

#[test]
fn platform_default_preserves_executable_arguments_and_flavor() {
    assert_eq!(
        ShellConfig::default_for(Platform::Posix),
        ShellConfig {
            flavor: ShellFlavor::Posix,
            executable: PathBuf::from("/bin/sh"),
            arguments: vec!["-lc".to_owned()],
        }
    );
    assert_eq!(
        ShellConfig::default_for(Platform::Windows).flavor,
        ShellFlavor::PowerShell
    );
}
