//! The command vocabularies a shell family is analyzed against.
//!
//! Four lists decide what a command may do before it runs: what is read-only,
//! what is allowed outright, what is refused wherever it appears, and what is
//! refused only as a bare command. They are per interpreter rather than per
//! host, because a Windows machine driving Git Bash composes the POSIX lists.
//!
//! These are data, and the reason they are data in their own module is that an
//! operator's configuration replaces or extends them: a list read from a file
//! and a list shipped with the binary have to be the same shape, so neither can
//! be spelled inline at the place it is consumed.

/// Reference `_get_default_allowlist`: the seven commands both branches share.
const SHELL_ALLOWLIST_COMMON: [&str; 7] = [
    "cd",
    "echo",
    "git diff",
    "git log",
    "git status",
    "tree",
    "whoami",
];

/// Reference `_READ_ONLY_COMMANDS_POSIX`, which `default_read_only_commands`
/// publishes on a POSIX host.
///
/// It is the read-only half of the allowlist, and the shell policy's
/// path-inspecting set is built on top of it: reference `_PATH_COMMANDS` is
/// documented as a superset of exactly this list, so a command that can be
/// auto-allowed has its operands checked before the grant.
pub const SHELL_READ_ONLY_POSIX: [&str; 37] = [
    "basename",
    "cat",
    "comm",
    "cut",
    "date",
    "diff",
    "dirname",
    "du",
    "file",
    "find",
    "fmt",
    "fold",
    "grep",
    "head",
    "join",
    "less",
    "ls",
    "md5sum",
    "more",
    "nl",
    "od",
    "paste",
    "pwd",
    "readlink",
    "sha1sum",
    "sha256sum",
    "shasum",
    "sort",
    "stat",
    "sum",
    "tac",
    "tail",
    "tr",
    "uname",
    "uniq",
    "wc",
    "which",
];

/// Reference `_READ_ONLY_COMMANDS_WINDOWS`.
pub const SHELL_READ_ONLY_WINDOWS: [&str; 6] = ["dir", "findstr", "more", "type", "ver", "where"];

/// Reference `_get_default_denylist`.
const SHELL_DENYLIST_POSIX: [&str; 14] = [
    "gdb", "pdb", "passwd", "nano", "vim", "vi", "emacs", "bash -i", "sh -i", "zsh -i", "fish -i",
    "dash -i", "screen", "tmux",
];

const SHELL_DENYLIST_WINDOWS: [&str; 7] = [
    "gdb",
    "pdb",
    "passwd",
    "cmd /k",
    "powershell -NoExit",
    "pwsh -NoExit",
    "notepad",
];

/// Reference `_get_default_denylist_standalone`.
const SHELL_DENYLIST_STANDALONE_POSIX: [&str; 11] = [
    "python", "python3", "ipython", "bash", "sh", "nohup", "vi", "vim", "emacs", "nano", "su",
];

const SHELL_DENYLIST_STANDALONE_WINDOWS: [&str; 7] = [
    "python",
    "python3",
    "ipython",
    "cmd",
    "powershell",
    "pwsh",
    "notepad",
];

/// Reference `default_read_only_commands`, the branch `posix_shell` selects.
#[must_use]
pub fn shell_read_only_commands(posix_shell: bool) -> &'static [&'static str] {
    if posix_shell {
        &SHELL_READ_ONLY_POSIX
    } else {
        &SHELL_READ_ONLY_WINDOWS
    }
}

/// Reference `_get_default_allowlist`: the shared seven, then the read-only
/// commands of the branch.
#[must_use]
pub fn shell_allowlist(posix_shell: bool) -> Vec<&'static str> {
    SHELL_ALLOWLIST_COMMON
        .iter()
        .copied()
        .chain(shell_read_only_commands(posix_shell).iter().copied())
        .collect()
}

#[must_use]
pub fn shell_denylist(posix_shell: bool) -> &'static [&'static str] {
    if posix_shell {
        &SHELL_DENYLIST_POSIX
    } else {
        &SHELL_DENYLIST_WINDOWS
    }
}

#[must_use]
pub fn shell_denylist_standalone(posix_shell: bool) -> &'static [&'static str] {
    if posix_shell {
        &SHELL_DENYLIST_STANDALONE_POSIX
    } else {
        &SHELL_DENYLIST_STANDALONE_WINDOWS
    }
}
