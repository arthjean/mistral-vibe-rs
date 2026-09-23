use super::*;
use crate::shell::lexer::{WordSplit, split_tokens};

/// `(command, requires approval, inspects positional paths, inspects the git
/// repository, option path values, path candidates without positional
/// inspection)`, as the pinned reference answered each.
type Case = (
    &'static str,
    bool,
    bool,
    bool,
    &'static [&'static str],
    &'static [&'static str],
);

const CASES: &[Case] = &[
    ("sort -o out.txt in.txt", true, false, false, &[], &[]),
    ("sort -ko x", false, false, false, &[], &[]),
    ("sort --out=x", true, false, false, &[], &[]),
    (
        "sort --random-source=/etc/x a",
        false,
        false,
        false,
        &["/etc/x"],
        &["/etc/x"],
    ),
    (
        "grep -f /etc/pats x",
        false,
        false,
        false,
        &["/etc/pats"],
        &["/etc/pats"],
    ),
    ("grep -Af x", false, false, false, &[], &[]),
    (
        "file -m a:b x",
        false,
        false,
        false,
        &["a", "b"],
        &["a", "b"],
    ),
    ("file -z x", true, false, false, &[], &[]),
    ("du --files0-from=x", true, false, false, &["x"], &["x"]),
    (
        "du -X /etc/ex .",
        false,
        false,
        false,
        &["/etc/ex"],
        &["/etc/ex"],
    ),
    ("date -s 10:00", true, false, false, &[], &[]),
    ("date 0101120025", true, false, false, &[], &[]),
    ("date -j 0101120025", false, false, false, &[], &[]),
    ("date -f fmt x", true, false, false, &["fmt"], &["fmt"]),
    ("date -Iseconds", false, false, false, &[], &[]),
    (
        "diff -X /etc/x a b",
        false,
        false,
        false,
        &["/etc/x"],
        &["/etc/x"],
    ),
    ("find . -delete", true, false, false, &[], &[]),
    ("md5sum -c sums", true, false, false, &[], &[]),
    ("md5sum -ac x", false, false, false, &[], &[]),
    ("tree -o out", true, true, false, &[], &["out"]),
    (
        "tree -L 2 --gitfile=/etc/g",
        false,
        true,
        false,
        &["/etc/g"],
        &["/etc/g", "2"],
    ),
    ("git log --ext-diff", true, false, true, &[], &[]),
    ("git log --diff-merges=R", true, false, true, &[], &[]),
    (
        "git diff --no-index a b",
        false,
        true,
        true,
        &[],
        &["diff", "a", "b"],
    ),
    (
        "git diff -O/etc/order",
        false,
        false,
        true,
        &["/etc/order"],
        &["/etc/order"],
    ),
    ("git status", false, false, true, &[], &[]),
    ("git reset --hard", false, false, false, &[], &[]),
    ("uniq a b", true, false, false, &[], &[]),
    ("uniq -f 2 a", false, false, false, &[], &[]),
    ("uniq +3 a", false, false, false, &[], &[]),
    ("wc --files0-from=x", true, false, false, &["x"], &["x"]),
    ("less -k keys", true, false, false, &[], &[]),
    ("less +/pat", false, false, false, &[], &[]),
    ("less +:e", true, false, false, &[], &[]),
    ("less -P'x$k'", true, false, false, &[], &[]),
    ("less --log-f=x", true, false, false, &[], &[]),
    ("less -5o", true, false, false, &[], &[]),
    ("less --tabs=4k", true, false, false, &[], &[]),
    ("less file", false, false, false, &[], &[]),
    ("less -- -k", false, false, false, &[], &[]),
    ("more -O x", true, false, false, &[], &[]),
    ("less '+/a$k'", true, false, false, &[], &[]),
    ("less -x4,8k", true, false, false, &[], &[]),
    ("sort -S 10T x", false, false, false, &[], &[]),
    ("less --pattern=a$-o", true, false, false, &[], &[]),
    ("/usr/bin/SORT.EXE -o x", true, false, false, &[], &[]),
];

#[test]
fn every_program_policy_answers_as_the_reference_does() {
    for (command, requires, positional, repository, values, candidates) in CASES {
        let tokens = split_tokens(command, WordSplit::Posix);
        let policy = analyze_command_policy(&tokens);
        assert_eq!(policy.requires_approval, *requires, "`{command}` approval");
        assert_eq!(
            policy.inspect_positional_paths, *positional,
            "`{command}` positional paths"
        );
        assert_eq!(
            policy.inspect_git_repository, *repository,
            "`{command}` repository"
        );
        assert_eq!(
            policy.option_path_values, *values,
            "`{command}` option values"
        );
        assert_eq!(
            path_candidates(&tokens, false),
            *candidates,
            "`{command}` candidates"
        );
    }
}

#[test]
fn a_program_is_named_by_its_folded_basename() {
    assert_eq!(command_name("/usr/bin/SORT.EXE"), "sort");
    assert_eq!(command_name(r#""C:\Tools\git.cmd""#), "git");
    assert!(has_option_guardrails(&["git", "log"]));
    assert!(!has_option_guardrails(&["cat"]));
}
