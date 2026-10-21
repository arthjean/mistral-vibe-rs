"""The inputs ``shell_policy.py`` asks the pinned reference's shell policy about.

Every command, path, grant and token here is authored for the corpus; none is
read from the reference. ``shell_policy.py`` records what the reference answers
for each.
"""

from __future__ import annotations

#: Command strings whose grammar extraction is recorded. They cover the shapes
#: a tokenizer decides differently from a grammar: a heredoc whose command node
#: sits under a redirect, the four chain operators, quoting of every kind, a
#: command substitution, an assignment prefix, a compound statement, and text
#: the grammar cannot parse.
EXTRACTION_CASES: tuple[str, ...] = (
    "",
    "ls",
    "ls -la",
    "python3 << 'EOF'",
    "python3 <<'EOF'\nprint(1)\nEOF",
    "cat README.md && rm -rf build",
    "grep needle src | wc -l",
    "ls; pwd",
    "cd /tmp || true",
    "cat file.txt > out.txt",
    "wc -l < input.txt",
    "echo 'hello world'",
    'echo "hello world"',
    "echo hello\\ world",
    "ls -la 'my dir'",
    "git config user.name",
    "npm run build -- --watch",
    "find . -name '*.rs' -exec rm {} ;",
    "cat $(which ls)",
    "VAR=1 ./script.sh",
    "if [ -f x ]; then cat x; fi",
    "for f in *.txt; do cat $f; done",
    "/usr/bin/vim notes.txt",
    "sudo apt update",
    "cat 'unterminated",
    "cat file.txt 2>&1",
    "ls & pwd",
    "( cd src && ls )",
    # Words the bash grammar reads literally and zsh expands, which the
    # reference drops from a command's words.
    "=ls -la",
    "ls ~root",
    "echo a==b",
    "ls ***/x",
    # ANSI-C quoting, which the reference keeps in the words for the guardrails.
    "find . $'-exec' rm {} ;",
    "echo $'a\\tb'",
    "git $SUB",
    "echo $HOME ${HOME}",
    "ls {a,b}",
    "echo $((1+2))",
    "diff <(ls) <(ls)",
    "f() { ls; }",
    "ls >/dev/null 2>&1",
    "ls >&2",
    "cat <<< text",
    "[[ -n $FOO ]]",
    "x=1",
    "ls \\\n -la",
    "eval 'sort -o x y'",
)

#: Command strings whose resolved permission is recorded. They cover each of the
#: four lists, the five commands this port used to deny outright, the find
#: execution predicates, the sensitive prefix, and the chains that must not let
#: an approval for one segment cover the next.
RESOLUTION_CASES: tuple[str, ...] = (
    "ls -la",
    "cat README.md",
    "wc -l src/main.rs",
    "git status",
    "git diff --stat",
    "rm -rf build",
    "rm file.txt",
    "dd if=/dev/zero of=disk.img",
    "mkfs.ext4 /dev/sdb1",
    "shutdown -h now",
    "eval echo hi",
    "vim notes.txt",
    "vi",
    "nano",
    "emacs",
    "tmux",
    "screen",
    "gdb ./binary",
    "pdb script.py",
    "passwd",
    "bash -i",
    "sh -i",
    "zsh -i",
    "python3",
    "python",
    "ipython",
    "bash",
    "sh",
    "su",
    "nohup",
    "python3 script.py",
    "/usr/bin/python3",
    "/usr/bin/vim notes.txt",
    "python3 << 'EOF'",
    "python3 <<'EOF'\nprint(1)\nEOF",
    "",
    "cat 'unterminated",
    "sudo apt update",
    "sudo ls",
    "find . -name '*.rs'",
    "find . -exec rm {} ;",
    "find . -execdir rm {} ;",
    "find . -ok rm {} ;",
    "find . -okdir rm {} ;",
    "find . -exec rm {} ; && find . -exec rm {} ;",
    "cargo build",
    "npm run build",
    "npm run test",
    "cat README.md && rm -rf build",
    "cat README.md && cat CHANGELOG.md",
    "echo one && echo two",
    "cargo build && cargo test",
    "unknown-binary --flag",
    "cd src",
    "tree",
    "whoami",
    # The cases where this port withholds a grant the reference gives, each
    # recorded so the divergence is measured rather than asserted.
    "cat $(which ls)",
    "cat file.txt > out.txt",
    "git -c core.pager=sh log",
    "git diff --no-index /etc/passwd /dev/null",
    "rg --pre helper needle .",
    "git reset --hard",
    "git reset --hard -- src",
    # The per-program guardrails: an option that writes, runs or sets
    # something withholds the grant the allowlist gives, and each negative
    # control stays granted.
    "sort -o out.txt in.txt",
    "sort in.txt",
    "sort --random-source=/etc/hosts in.txt",
    "date -s 10:00",
    "date 0101120025",
    "date +%s",
    "less -k keys notes.txt",
    "less notes.txt",
    "more +:e notes.txt",
    "uniq a.txt b.txt",
    "uniq a.txt",
    "tree -o out.txt",
    "tree -L 2",
    "md5sum -c sums.txt",
    "md5sum notes.txt",
    "wc --files0-from=list.txt",
    "du -sh .",
    "du --files0-from=list.txt",
    "file -z archive.gz",
    "file notes.txt",
    "grep -f /etc/hosts notes.txt",
    "diff -X /etc/hosts a.txt b.txt",
    "find . -delete",
    "find . -fprint out.txt",
    "git log --ext-diff",
    "git log --output=out.txt",
    "git log",
    "git diff --diff-merges=remerge",
    "git status && sort -o x y && sort -o x y",
    "sudo ls && sudo ls",
    # What a wrapper runs is checked as if it ran directly.
    "eval git log --ext-diff",
    "eval 'sort -o x y'",
    "exec sort -o x y",
    "exec -a name sort -o x y",
    "exec",
    "eval vim notes.txt",
    "exec -- vim notes.txt",
    "eval python3",
    # The directory a git reader runs in, followed through `cd`, `pushd` and
    # `popd`, and given up on when it cannot be known.
    "cd <outside> && git status",
    "cd $X && git status",
    "pushd <outside> && popd && git status",
    "popd +1 && git status",
    "cd -- <outside> && git status",
    # Syntax that needs approval: where the words still name the command, the
    # command pattern carries the approval; where they do not, only the text
    # as written can be approved.
    "git $SUB",
    "sudo $CMD",
    "git log $REF",
    "echo $HOME",
    "ls {a,b}",
    "HOME=/x git status",
    "ls &",
    "( ls )",
    "ls >/dev/null 2>&1",
    "ls >&2",
    "ls >&file",
    "cat <<< text",
    "echo $'a'",
    "=ls",
    "ls ~root",
    "f() { ls; }",
    "ls \\\n -la",
    "[[ -n $FOO ]]",
    "env FOO=1 ls",
    ". ./script.sh",
    "npm run $TASK",
    "npm run build $ARGS",
    "cargo $CMD",
    "docker compose up $SVC",
    'echo "$(date)"',
    "cat <outside>/secret.txt $X",
)

#: Repository configurations a git reader is resolved in, each a `.git/config`
#: written into the workdir. The reference reads them without invoking git, and
#: an executable helper withholds the grant `git status` would otherwise get.
REPOSITORY_FIXTURES: tuple[tuple[str, str], ...] = (
    ("plain", "[core]\n\tbare = false\n"),
    ("pager", "[core]\n\tpager = less\n"),
    ("pager-off", "[core]\n\tpager = off\n"),
    ("include", "[include]\n\tpath = other.config\n"),
    ("log-pager", "[pager]\n\tlog = cat\n"),
    ("fsmonitor", "[core]\n\tfsmonitor\n"),
    ("diff-driver", '[diff "x"]\n\ttextconv = cat\n'),
    ("gpg", "[gpg]\n\tprogram = gpg2\n"),
)

#: The git readers resolved in every repository fixture.
REPOSITORY_COMMANDS: tuple[str, ...] = (
    "git status",
    "git diff",
    "git log",
    "cd nested && git log",
    "cd <outside> && git status",
)

#: Calls the managed resolver answers, as `(command, cwd, shell, env names)`:
#: a `cwd` is where operands resolve and is itself an outside directory when it
#: leaves the workspace, the shell and environment overrides carry their own
#: requirements, and the denylist also matches a program's basename.
MANAGED_CASES: tuple[tuple[str, str | None, str | None, tuple[str, ...]], ...] = (
    ("pwd", None, None, ()),
    ("pwd", "<outside>", None, ()),
    ("pwd", "nested", None, ()),
    ("pwd", "<scratchpad>", None, ()),
    ("cat secret.txt", "<outside>", None, ()),
    ("cat nested/secret.txt", "<outside>", None, ()),
    ("cat ../elsewhere/secret.txt", "<workdir>", None, ()),
    ("pwd", None, "/bin/zsh", ()),
    ("pwd", None, None, ("B", "A")),
    ("rm -rf build", None, "/bin/zsh", ("X",)),
    ("# only a comment", None, "/bin/zsh", ()),
    ("/usr/bin/vim notes.txt", None, None, ()),
    ("/usr/bin/python3", None, None, ()),
    ("git status", "<outside>", None, ()),
    ("ls $X", None, "/bin/zsh", ()),
    ("sudo ls", "<outside>", None, ("A",)),
)

#: Commands whose path operands are resolved against a workdir this script
#: creates. ``<workdir>``, ``<outside>``, ``<scratchpad>`` and ``<home>`` are
#: substituted back into both the case and its answer, so the corpus replays on
#: another machine.
OUTSIDE_DIR_CASES: tuple[str, ...] = (
    "cat <workdir>/inside.txt",
    "cat ./inside.txt",
    "cat inside.txt",
    "cat <outside>/secret.txt",
    "cat <outside>/nested/secret.txt",
    "grep needle <outside>/secret.txt",
    "wc -l <outside>/secret.txt",
    "ls <outside>",
    "cat <scratchpad>/note.txt",
    "rm <outside>/secret.txt",
    "cp <outside>/secret.txt <workdir>/copy.txt",
    "chmod +x <outside>/secret.txt",
    "chmod 755 <outside>/secret.txt",
    "cat -n <outside>/secret.txt",
    "ls --color <outside>",
    "cat <outside>/a.txt && cat <outside>/b.txt",
    "echo <outside>/secret.txt",
    "cat ~/.ssh/id_rsa",
    "cat /etc/passwd",
    # An ascending operand, which the reference folds before it positions it:
    # one that lands outside, one that lands back inside, and one that ascends
    # past the root.
    "cat ../elsewhere/secret.txt",
    "cat <workdir>/../elsewhere/nested/secret.txt",
    "cat sub/../inside.txt",
    "cat ../../../../etc/passwd",
    # Every operand is its own path: two files of one directory, a directory,
    # a file that does not exist yet, and two links that are named by where
    # they lead rather than by how they were written.
    "cat <outside>/secret.txt <outside>/nested/secret.txt",
    "ls <outside>/nested",
    "touch <outside>/missing/new.txt",
    "cat <workdir>/escape/secret.txt",
    "cat <outside>/approved/link/secret.txt",
)

#: The encoded grants the allowlist cases name, as their placeholder forms.
EXACT_SECRET = "vibe-path:exact:<outside>/secret.txt"
RECURSIVE_OUTSIDE = "vibe-path:directory_recursive:<outside>"

#: Calls resolved with path grants in the shell's allowlist, as
#: `(resolver, allowlist, command, cwd)`. Since v2.25.8 a permanent approval
#: writes the encoded grant into the tool's allowlist and every shell reads it
#: back before asking; an absolute path glob is read the same way, while a
#: command prefix or wildcard never clears a path.
ALLOWLIST_GRANT_CASES: tuple[tuple[str, tuple[str, ...], str, str | None], ...] = (
    ("legacy", ("cat", EXACT_SECRET), "cat <outside>/secret.txt", None),
    ("legacy", ("cat", EXACT_SECRET), "cat <outside>/nested/secret.txt", None),
    (
        "legacy",
        ("cat", EXACT_SECRET),
        "cat <outside>/secret.txt <outside>/nested/secret.txt",
        None,
    ),
    ("legacy", ("cat", RECURSIVE_OUTSIDE), "cat <outside>/nested/secret.txt", None),
    (
        "legacy",
        ("cat", "vibe-path:directory_recursive:<outside>/nested"),
        "cat <outside>/secret.txt",
        None,
    ),
    ("legacy", ("cat", "<outside>/*"), "cat <outside>/secret.txt", None),
    ("legacy", ("cat", "<outside>/*"), "cat <outside>/nested/secret.txt", None),
    ("legacy", ("cat", "<outside>/secre?.txt"), "cat <outside>/secret.txt", None),
    ("legacy", ("cat", "<outside>/secret.txt"), "cat <outside>/secret.txt", None),
    ("legacy", ("cat", "elsewhere/*"), "cat <outside>/secret.txt", None),
    ("legacy", ("*",), "cat <outside>/secret.txt", None),
    ("legacy", ("cat", "/*"), "cat <outside>/secret.txt", None),
    (
        "legacy",
        ("cat", "vibe-path:recursive:<outside>"),
        "cat <outside>/secret.txt",
        None,
    ),
    (
        "legacy",
        ("cat", "vibe-path:exact:<outside>/./nested/../secret.txt"),
        "cat <outside>/secret.txt",
        None,
    ),
    ("legacy", (EXACT_SECRET,), "cat <outside>/secret.txt", None),
    ("legacy", ("sudo", EXACT_SECRET), "sudo cat <outside>/secret.txt", None),
    (
        "legacy",
        ("grep", EXACT_SECRET),
        "grep -f <outside>/secret.txt notes.txt",
        None,
    ),
    (
        "legacy",
        ("cat", "vibe-path:directory_recursive:<outside>/approved"),
        "cat <outside>/approved/link/secret.txt",
        None,
    ),
    ("legacy", ("ls", RECURSIVE_OUTSIDE), "ls <outside>", None),
    (
        "legacy",
        ("cd", "git status", RECURSIVE_OUTSIDE),
        "cd <outside> && git status",
        None,
    ),
    ("managed", ("pwd", RECURSIVE_OUTSIDE), "pwd", "<outside>/nested"),
    ("managed", ("pwd", "vibe-path:exact:<outside>"), "pwd", "<outside>"),
    ("managed", ("pwd", "vibe-path:exact:<outside>"), "pwd", "<outside>/nested"),
    ("managed", ("cat", "vibe-path:exact:<outside>"), "cat secret.txt", "<outside>"),
    ("managed", ("cat", "<outside>/*"), "cat nested/secret.txt", "<outside>"),
    ("managed", ("cat", EXACT_SECRET), "cat ../elsewhere/secret.txt", None),
    (
        "managed",
        ("cat", "vibe-path:directory_recursive:<outside>/approved"),
        "cat <outside>/approved/link/secret.txt",
        None,
    ),
    ("managed", ("*",), "pwd", "<outside>"),
)

#: `(path, pattern)` pairs offered to ``path_grant_pattern_matches``: encoded
#: grants of both scopes and an unknown one, absolute path globs in both
#: grammars, and the command entries a shell allowlist also holds.
PATH_GRANT_MATCH_CASES: tuple[tuple[str, str], ...] = (
    ("/srv/app/a.txt", "vibe-path:exact:/srv/app/a.txt"),
    ("/srv/app/a.txt", "vibe-path:exact:/srv/app/./a.txt"),
    ("/srv/app/a.txt", "vibe-path:exact:/srv/app"),
    ("/srv/app/a.txt", "vibe-path:directory_recursive:/srv/app"),
    ("/srv/app", "vibe-path:directory_recursive:/srv/app"),
    ("/srv/application", "vibe-path:directory_recursive:/srv/app"),
    ("/srv/app/a.txt", "vibe-path:directory_recursive:/srv/app/"),
    ("/srv/app/a.txt", "vibe-path:recursive:/srv/app"),
    ("/srv/app/a.txt", "vibe-path:/srv/app/a.txt"),
    ("/srv/app/a.txt", "vibe-path"),
    ("/srv/app/a.txt", "/srv/app/*"),
    ("/srv/app/sub/a.txt", "/srv/app/*"),
    ("/srv/app/a.txt", "/srv/*/a.txt"),
    ("/srv/app/a.txt", "/srv/app/?.txt"),
    ("/srv/app/a.txt", "/srv/app/[ab].txt"),
    ("/srv/app/a.txt", "/srv/app/a.txt"),
    ("/srv/app/a.txt", "/*"),
    ("/srv/app/a.txt", "/**"),
    ("/srv/app/a.txt", "srv/app/*"),
    ("/srv/app/a.txt", "*"),
    ("/srv/app/a.txt", "*/a.txt"),
    ("/srv/app/a.txt", "cat"),
    ("/srv/app/a.txt", "npm *"),
    ("/srv/app/a.txt", "cat /srv/app/*"),
    ("C:\\srv\\app\\a.txt", "C:\\srv\\app\\*"),
    ("C:\\srv\\app\\a.txt", "c:/SRV/app/*"),
    ("C:\\srv\\app\\sub\\a.txt", "C:\\srv\\app\\*"),
    ("C:\\srv\\app\\a.txt", "C:srv\\*"),
    ("C:\\srv\\app\\a.txt", "vibe-path:exact:c:\\SRV\\app\\A.txt"),
    ("C:\\srv\\app\\a.txt", "vibe-path:directory_recursive:C:\\srv"),
    ("\\\\server\\share\\a.txt", "\\\\server\\share\\*"),
    ("/srv/app/a.txt", "\\srv\\app\\*"),
)

#: Every action GNU findutils documents (``man find``, ACTIONS), plus the
#: ``-files0-from`` option, offered one at a time to the reference ``find``
#: policy. The ones it gates are what ``findExecutionPredicates`` records; the
#: rest (``-print``, ``-prune`` and the like) are the negative controls.
FIND_PRIMARY_CANDIDATES: tuple[str, ...] = (
    "-delete",
    "-exec",
    "-execdir",
    "-files0-from",
    "-fls",
    "-fprint",
    "-fprint0",
    "-fprintf",
    "-ls",
    "-ok",
    "-okdir",
    "-print",
    "-print0",
    "-printf",
    "-prune",
    "-quit",
)

#: PowerShell command strings whose grammar is recorded: the parts a command
#: splits into, each part's tokens and the forms the lists match it under, and
#: the output redirections that can write a file. The helpers are pure string
#: functions, so a POSIX host answers for them as a Windows host would.
WINDOWS_GRAMMAR_CASES: tuple[str, ...] = (
    "",
    "Get-ChildItem",
    "dir C:\\work && type notes.txt",
    "ls; cat a.txt | sls needle",
    "echo a & echo b",
    "cmd 2>&1 | more",
    "Get-Content x *>&1",
    "Write-Output $(Get-Date)",
    "Write-Output \"$(Get-Date) and $(whoami)\"",
    "Invoke-Command { Remove-Item x; ls }",
    "(Get-Item a).Name",
    "echo '$(not a subexpression)'",
    "echo `& not a separator",
    "echo a ^& b",
    "& 'C:\\Program Files\\tool.exe' -Flag",
    "& C:\\tools\\rm.exe -rf x",
    "C:\\tools\\RM.EXE x",
    "rm.exe x",
    "Remove-Item x",
    "gci -Recurse",
    "type > out.txt",
    "echo a > $null",
    "echo a > NUL",
    "echo a 2> 'err log.txt'",
    "echo a >> C:\\logs\\x.txt",
    "echo a >&2",
    "echo 'a > b'",
    "echo a >`\"q`\" x",
    "notepad",
    "powershell -NoExit",
    "git status\r\ngit log",
    "@{a=1}; ${x}",
    "echo ${env:PATH}",
)

#: Policy patterns whose match forms are recorded.
WINDOWS_PATTERN_CASES: tuple[str, ...] = (
    "rm",
    "Remove-Item",
    "cmd /k",
    "powershell -NoExit",
    "ls -la",
    "C:\\tools\\rm.exe",
    "git status",
    "",
)

#: Tokens whose PowerShell expansion is recorded, under a fixed environment and
#: a fixed working directory.
WINDOWS_EXPANSION_CASES: tuple[str, ...] = (
    "$HOME\\notes.txt",
    "$env:APPDATA\\x",
    "${env:APPDATA}\\x",
    "$Env:appdata\\x",
    "$PWD\\sub",
    "$pwd",
    "$other\\x",
    "${global:x}",
    "$env:MISSING\\x",
    "'C:\\quoted path'",
    "@(1)",
    "cost$",
    "plain.txt",
)

#: The environment the expansion cases resolve against.
WINDOWS_EXPANSION_ENVIRONMENT: dict[str, str] = {
    "USERPROFILE": "C:\\Users\\me",
    "AppData": "C:\\Users\\me\\AppData\\Roaming",
}

#: Tokens whose option, path and attached-value shape is recorded.
WINDOWS_TOKEN_CASES: tuple[str, ...] = (
    "-Path",
    "-Path:C:\\x",
    "-Path=C:\\x",
    "/s",
    "/s:x",
    "//server",
    "/c/x",
    "C:\\x",
    "C:",
    "x:y",
    "\\\\server\\share",
    "~\\x",
    ".\\x",
    "notes.txt",
    "'C:\\quoted'",
    "-",
)

#: Session commands the `*_stdin` permission is resolved against, per shell
#: family: input to a session that may be a pager asks, anything else defers.
#: `None` stands for a session the family does not know.
STDIN_PERMISSION_CASES: tuple[tuple[str, str | None], ...] = (
    ("posix", "git log"),
    ("posix", "less notes.txt"),
    ("posix", "more notes.txt"),
    ("posix", "cat notes.txt"),
    ("posix", "sleep 30; git log"),
    ("posix", "eval git log"),
    ("posix", "exec less notes.txt"),
    ("posix", "/usr/bin/GIT.exe log"),
    ("posix", "echo git"),
    ("posix", "gitk"),
    ("posix", "(git log)"),
    ("posix", "cat $(which less)"),
    ("posix", None),
    ("git_bash", "git log"),
    ("git_bash", "cat notes.txt"),
    ("powershell", "git log"),
    ("powershell", "& 'C:\\tools\\git.exe' log"),
    ("powershell", "type a | more"),
    ("powershell", "Get-Content a"),
    ("powershell", "less.exe x"),
    ("powershell", None),
)
