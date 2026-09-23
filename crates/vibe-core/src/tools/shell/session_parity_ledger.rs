// The gaps this port still carries against the reference managed shell.
//
// Generated from a failing run of the replay in `session_parity_tests.rs` and
// kept honest by three tests there: a difference no entry names fails the
// suite, an entry whose difference no longer reproduces fails it too, and
// every reason stated below has to be a row of a divergence table in
// `docs/parity.md`. Every entry belongs to row 6 of that document, which is the
// row this ledger lowers.
//
// Entries are at field granularity, which is what NFR "no ledger entry is
// scoped wider than one field on one case" asks for: `/modelText` is one
// field, `/sessions/0/manifest/shell` is one field, and `/typedResult` alone
// would be thirteen.

const WHY_RENDERED: &str = "the rendered text is the typed document read back one field per \
    line, so it carries whatever field that document still diverges on for this case (FR-05)";
const WHY_MESSAGE: &str = "the sentence the reference publishes for this action is \
    reference-authored text no committed artifact may carry, so the corpus holds a digest of it \
    and this port answers its own wording";
const WHY_ERROR_KIND: &str = "the reference refuses these arguments inside the handler and raises \
    its tool error, and this port refuses them at the schema boundary, so both refuse and the \
    kind differs";

/// Where `docs/parity.md` holds each reason above, so a gap the replay
/// tolerates is a row a reader can look up rather than a sentence in a test.
/// [`every_recorded_reason_has_exactly_one_divergence_row`] resolves both
/// directions: a reason naming a row nobody wrote fails, and a row the ledger
/// no longer reaches fails too.
const RECORDS: &[Recorded] = &[
    Recorded {
        why: WHY_ERROR_KIND,
        row: "A shell argument is refused at the schema boundary",
        table: Table::Accepted,
    },
    Recorded {
        why: WHY_MESSAGE,
        row: "The managed shell's own action sentences",
        table: Table::Accepted,
    },
];

/// Builds one entry. Every gap this ledger holds belongs to [`SHELL_ROW`],
/// which is the row the replay lowers, so the row is not spelled 2 500 times.
const fn gap(
    tool: &'static str,
    case: &'static str,
    pointer: &'static str,
    closed_by: &'static str,
    why: &'static str,
) -> Divergence {
    Divergence {
        tool,
        case,
        pointer,
        closed_by,
        row: SHELL_ROW,
        why,
    }
}

const LEDGER: &[Divergence] = &[
    gap("bash", "foreground-empty-command", "/error/type", RECORDED, WHY_ERROR_KIND),
    gap("bash_stdin", "write-nothing", "/error/type", RECORDED, WHY_ERROR_KIND),
    gap("bash_stdin", "write-text-and-control-together", "/error/type", RECORDED, WHY_ERROR_KIND),
    gap("bash_stdin", "write-invalid-base64", "/error/type", RECORDED, WHY_ERROR_KIND),
    gap("bash_sessions", "inspect-without-a-session-id", "/error/type", RECORDED, WHY_ERROR_KIND),
    gap("bash_sessions", "kill-without-a-session-id", "/error/type", RECORDED, WHY_ERROR_KIND),
    gap("bash_sessions", "reset-with-no-session", "/modelText", LICENSING, WHY_RENDERED),
    gap("bash_sessions", "reset-with-no-session", "/typedResult/message", LICENSING, WHY_MESSAGE),
    gap("bash_sessions", "reset-with-clear-logs-and-no-session", "/modelText", LICENSING, WHY_RENDERED),
    gap("bash_sessions", "reset-with-clear-logs-and-no-session", "/typedResult/message", LICENSING, WHY_MESSAGE),
    gap("bash_sessions", "kill-a-running-session", "/modelText", LICENSING, WHY_RENDERED),
    gap("bash_sessions", "kill-a-running-session", "/typedResult/message", LICENSING, WHY_MESSAGE),
    gap("bash_sessions", "kill-an-already-killed-session", "/modelText", LICENSING, WHY_RENDERED),
    gap("bash_sessions", "kill-an-already-killed-session", "/typedResult/message", LICENSING, WHY_MESSAGE),
    gap("bash_sessions", "reset-the-remaining-session", "/modelText", LICENSING, WHY_RENDERED),
    gap("bash_sessions", "reset-the-remaining-session", "/typedResult/message", LICENSING, WHY_MESSAGE),
    gap("bash_sessions", "reset-and-clear-the-logs", "/modelText", LICENSING, WHY_RENDERED),
    gap("bash_sessions", "reset-and-clear-the-logs", "/typedResult/message", LICENSING, WHY_MESSAGE),
    gap("bash_log_file", "read-without-a-session-or-a-path", "/error/type", RECORDED, WHY_ERROR_KIND),
];
