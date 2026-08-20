//! Differential oracle for what the tools actually *do*.
//!
//! The tool-surface oracle proves `grep` publishes the right schema. Nothing
//! proved `grep` returns the right matches, which is the ceiling
//! `docs/parity.md` names for itself. This module closes it: it drives this
//! port's tools over the same fixture tree
//! (`crates/vibe-app-server/tests/tool-execution/tree`) that
//! `scripts/parity/tool_execution.py` drove the reference over, projects both
//! results through the same rules, and diffs them as JSON pointers.
//!
//! Eleven tools are driven, which means eleven collaborators are stood up here
//! the way the capture script stands up its own: a loopback origin for the two
//! network tools, a scripted answerer for the two interactive ones, a scripted
//! subagent runner for `task`, and a skill root for `skill`. Every one of them
//! is the real registration path, not a reimplementation: `task` goes through
//! [`task_handler`], the interactive pair through
//! [`InteractiveSessionToolFactory`], and the rest through [`BuiltinTools`] and
//! [`WorkspaceTools`].
//!
//! The replay is unconditional. The committed corpus carries names, pointers,
//! counts and digests and no reference prose, so CI reports a conformance count
//! instead of skipping for want of a checkout, which is what FR-15 requires.
//! The one test that does need the checkout is the live probe at the bottom,
//! which recaptures and asserts the committed corpus is still what the pinned
//! reference answers.
//!
//! The surviving divergence is held in [`LEDGER`], one entry per known gap with
//! the story that closes it. A divergence outside the ledger fails the suite,
//! and so does a ledger entry whose divergence has been fixed: a ledger that
//! cannot rot is the only kind worth keeping.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use secrecy::SecretString;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use vibe_core::engine::CancellationToken;
use vibe_core::extensions::{
    ChildContext, SubagentFuture, SubagentManager, SubagentRun, SubagentRunner,
};
use vibe_core::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};
use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionStore, ToolGuard,
    TrustDecision, TrustRootKind,
};
use vibe_core::skills::SkillDiscovery;
use vibe_core::storage::SessionStore;
use vibe_core::tools::builtins::{BuiltinTools, WebSearchAccess};
use vibe_core::tools::{ToolError, ToolInvocation, ToolRegistry};
use vibe_core::workspace::{ReviewManager, Workspace, WorkspaceTools};

use crate::client::interactive::{InteractiveCallbackRequest, InteractiveSessionToolFactory};
use crate::client::live::delegation::{
    DEFAULT_SUBAGENT, built_in_subagent, task_handler, task_spec,
};
use crate::server::SessionToolFactory as _;

/// Grants every approval, so a case measures the tool body rather than the
/// policy in front of it.
///
/// The reference is driven with no permission store at all, which is what makes
/// its capture a measurement of the tool. Refusing here would compare this
/// port's guard against the reference's absent one and call the difference a
/// behavioral gap, which it is not. The permission model has its own epic.
struct GrantApproval;

impl ApprovalAgent for GrantApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
    }
}

const CORPUS_RELATIVE: &str = "crates/vibe-app-server/tests/tool-execution/corpus.json";
const TREE_RELATIVE: &str = "crates/vibe-app-server/tests/tool-execution/tree";
const CAPTURE_SCRIPT: &str = "scripts/parity/tool_execution.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 2;
/// The case floor this epic commits to, so a regeneration that captured almost
/// nothing fails instead of reporting a clean but empty run. NFR-1.
const MINIMUM_CASES: usize = 90;
/// The per-tool floor, so a corpus cannot reach the total above by driving one
/// tool ninety times. NFR-1.
const MINIMUM_CASES_PER_TOOL: usize = 4;
/// Every tool the epic drives, so dropping one is a named failure rather than a
/// smaller green run. NFR-1.
const MINIMUM_TOOLS: usize = 11;
/// The prefix a fixture dotfile is stored under, so a fixture `.gitignore`
/// cannot apply to this repository. Mirrors `DOT_PREFIX` in the capture script.
const DOT_PREFIX: &str = "dot-";
const TREE_PLACEHOLDER: &str = "{tree}";
const SCRATCHPAD_PLACEHOLDER: &str = "{scratchpad}";
/// The loopback origin's authority, which carries an ephemeral port and so
/// cannot be compared as it stands. Mirrors `SERVER_PLACEHOLDER`.
const SERVER_PLACEHOLDER: &str = "{server}";

/// What a divergence names when no story can close it, because closing it would
/// mean shipping reference prose.
const LICENSING: &str = "NOTICE";

/// Why a divergence stands, shared by every case that carries the same gap: the
/// reason is a property of the gap, not of the case that happens to reveal it.
/// Each is stated once so a story landing removes one sentence rather than
/// dozens.
const READ_FILE_NOTICE: &str = "this port writes its own guidance for an empty read and for an \
     offset past the last line; reaching the reference digest would mean copying its sentence";
const GREP_PROJECTION: &str = "the reference projects a parsed match list for the UI and drops the \
     pattern; this port publishes no second projection at all";
const EDIT_PROJECTION: &str = "the reference projects the occurrences it rewrote and drops the \
     message; this port publishes no second projection at all";
const EDIT_MESSAGE: &str = "this port writes its own applied-edit sentence; reaching the reference \
     digest would mean copying its wording";
const SKILL_PROSE: &str = "this port writes its own guidance lines around the skill body and its \
     own reuse sentence; reaching the reference digests would mean copying them";
const SKILL_DETACHED: &str = "the reference serves a skill registered with a prompt and no \
     directory; this port discovers skills from disk, so that skill does not exist here and the \
     call reports it missing. No story in this PRD closes the gap, so it is recorded by name \
     instead";
const ANSWER_KEYS: &str = "the reference publishes each answer as `{question, answer, is_other}`; \
     this port passes the client's `isOther` spelling straight through";
const ANSWER_TEXT: &str = "the reference renders `answers` and `cancelled` one field per line; \
     this port renders a sentence";
const PLAN_MESSAGE: &str = "this port writes its own message for each plan-review outcome; \
     reaching the reference digest would mean copying its wording";
const PLAN_TEXT: &str = "the reference renders `switched` and `message` one field per line; this \
     port renders the message alone";
const FETCH_MARKDOWN: &str = "the reference converts an HTML page with `markdownify`, which keeps \
     the heading marker and the blank line between blocks; this port strips the markup to prose \
     instead. No story in this PRD closes the gap, so it is recorded by name instead";
const FETCH_TRUNCATION: &str = "this port cuts the body at the smaller of the configured cap and \
     the remaining buffer, so it keeps two bytes fewer than the declared bound";
const FETCH_REQUEST: &str = "the reference sends `Accept` and `Accept-Language` alongside the user \
     agent; this port sends only what its HTTP client defaults to";
const FETCH_CHALLENGE: &str = "the reference retries a challenge response once under a different \
     user agent; this port reports the status and stops";
const FETCH_ERROR_KIND: &str = "the reference raises its own tool error for an argument it \
     rejects; this port raises a schema violation, which the oracle names `ValidationError`";
const TASK_DEPTH: &str = "the reference refuses to delegate from inside a subagent at call time; \
     this port lets the call through and enforces its limit deeper down";

/// The divergences this port still carries, each with what closes it.
///
/// A pointer is matched by prefix, so `/typedResult` covers every field under
/// it. Keep the list ordered by tool then by case.
///
/// Two kinds of entry live here. One names the story that closes it, and the
/// staleness check below deletes it as soon as the story lands. The other names
/// [`LICENSING`], which is the boundary `NOTICE` draws: reaching those digests
/// would mean writing the reference's own sentences into this repository, and
/// the PRD lists byte-identical message text as a non-goal for exactly that
/// reason. Everything around them still compares byte for byte, which is why
/// they are scoped to the one field rather than to the tool.
const LEDGER: &[Divergence] = &[
    Divergence {
        tool: "read_file",
        case: "empty-file",
        pointer: "/modelText",
        closed_by: LICENSING,
        why: READ_FILE_NOTICE,
    },
    Divergence {
        tool: "read_file",
        case: "empty-file",
        pointer: "/projectedResult",
        closed_by: LICENSING,
        why: READ_FILE_NOTICE,
    },
    Divergence {
        tool: "read_file",
        case: "empty-file",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: READ_FILE_NOTICE,
    },
    Divergence {
        tool: "read_file",
        case: "offset-one-past-the-last-line",
        pointer: "/modelText",
        closed_by: LICENSING,
        why: READ_FILE_NOTICE,
    },
    Divergence {
        tool: "read_file",
        case: "offset-one-past-the-last-line",
        pointer: "/projectedResult",
        closed_by: LICENSING,
        why: READ_FILE_NOTICE,
    },
    Divergence {
        tool: "read_file",
        case: "offset-one-past-the-last-line",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: READ_FILE_NOTICE,
    },
    Divergence {
        tool: "read_file",
        case: "offset-past-the-end",
        pointer: "/modelText",
        closed_by: LICENSING,
        why: READ_FILE_NOTICE,
    },
    Divergence {
        tool: "read_file",
        case: "offset-past-the-end",
        pointer: "/projectedResult",
        closed_by: LICENSING,
        why: READ_FILE_NOTICE,
    },
    Divergence {
        tool: "read_file",
        case: "offset-past-the-end",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: READ_FILE_NOTICE,
    },
    Divergence {
        tool: "grep",
        case: "anchored",
        pointer: "/projectedResult",
        closed_by: "US-246",
        why: GREP_PROJECTION,
    },
    Divergence {
        tool: "grep",
        case: "ignore-disabled",
        pointer: "/projectedResult",
        closed_by: "US-246",
        why: GREP_PROJECTION,
    },
    Divergence {
        tool: "grep",
        case: "ignore-honored",
        pointer: "/projectedResult",
        closed_by: "US-246",
        why: GREP_PROJECTION,
    },
    Divergence {
        tool: "grep",
        case: "lowercase-is-case-insensitive",
        pointer: "/projectedResult",
        closed_by: "US-246",
        why: GREP_PROJECTION,
    },
    Divergence {
        tool: "grep",
        case: "max-matches",
        pointer: "/projectedResult",
        closed_by: "US-246",
        why: GREP_PROJECTION,
    },
    Divergence {
        tool: "grep",
        case: "no-match",
        pointer: "/projectedResult",
        closed_by: "US-246",
        why: GREP_PROJECTION,
    },
    Divergence {
        tool: "grep",
        case: "regex-alternation",
        pointer: "/projectedResult",
        closed_by: "US-246",
        why: GREP_PROJECTION,
    },
    Divergence {
        tool: "grep",
        case: "scoped-to-a-subdirectory",
        pointer: "/projectedResult",
        closed_by: "US-246",
        why: GREP_PROJECTION,
    },
    Divergence {
        tool: "grep",
        case: "scoped-to-one-file",
        pointer: "/projectedResult",
        closed_by: "US-246",
        why: GREP_PROJECTION,
    },
    Divergence {
        tool: "grep",
        case: "uppercase-is-case-sensitive",
        pointer: "/projectedResult",
        closed_by: "US-246",
        why: GREP_PROJECTION,
    },
    Divergence {
        tool: "edit",
        case: "crlf-is-preserved",
        pointer: "/modelText",
        closed_by: LICENSING,
        why: EDIT_MESSAGE,
    },
    Divergence {
        tool: "edit",
        case: "crlf-is-preserved",
        pointer: "/projectedResult",
        closed_by: "US-247",
        why: EDIT_PROJECTION,
    },
    Divergence {
        tool: "edit",
        case: "crlf-is-preserved",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: EDIT_MESSAGE,
    },
    Divergence {
        tool: "edit",
        case: "replace-all",
        pointer: "/modelText",
        closed_by: LICENSING,
        why: EDIT_MESSAGE,
    },
    Divergence {
        tool: "edit",
        case: "replace-all",
        pointer: "/projectedResult",
        closed_by: "US-247",
        why: EDIT_PROJECTION,
    },
    Divergence {
        tool: "edit",
        case: "replace-all",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: EDIT_MESSAGE,
    },
    Divergence {
        tool: "edit",
        case: "single-replacement",
        pointer: "/modelText",
        closed_by: LICENSING,
        why: EDIT_MESSAGE,
    },
    Divergence {
        tool: "edit",
        case: "single-replacement",
        pointer: "/projectedResult",
        closed_by: "US-247",
        why: EDIT_PROJECTION,
    },
    Divergence {
        tool: "edit",
        case: "single-replacement",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: EDIT_MESSAGE,
    },
    Divergence {
        tool: "skill",
        case: "already-loaded-earlier",
        pointer: "/modelText",
        closed_by: LICENSING,
        why: SKILL_PROSE,
    },
    Divergence {
        tool: "skill",
        case: "already-loaded-earlier",
        pointer: "/projectedResult",
        closed_by: LICENSING,
        why: SKILL_PROSE,
    },
    Divergence {
        tool: "skill",
        case: "already-loaded-earlier",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: SKILL_PROSE,
    },
    Divergence {
        tool: "skill",
        case: "fewer-files-than-the-cap",
        pointer: "/modelText",
        closed_by: LICENSING,
        why: SKILL_PROSE,
    },
    Divergence {
        tool: "skill",
        case: "fewer-files-than-the-cap",
        pointer: "/projectedResult",
        closed_by: LICENSING,
        why: SKILL_PROSE,
    },
    Divergence {
        tool: "skill",
        case: "fewer-files-than-the-cap",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: SKILL_PROSE,
    },
    Divergence {
        tool: "skill",
        case: "more-files-than-the-cap",
        pointer: "/modelText",
        closed_by: LICENSING,
        why: SKILL_PROSE,
    },
    Divergence {
        tool: "skill",
        case: "more-files-than-the-cap",
        pointer: "/projectedResult",
        closed_by: LICENSING,
        why: SKILL_PROSE,
    },
    Divergence {
        tool: "skill",
        case: "more-files-than-the-cap",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: SKILL_PROSE,
    },
    Divergence {
        tool: "skill",
        case: "no-directory-on-disk",
        pointer: "/outcome",
        closed_by: "US-252",
        why: SKILL_DETACHED,
    },
    Divergence {
        tool: "ask_user_question",
        case: "cancelled",
        pointer: "/modelText",
        closed_by: "US-245",
        why: ANSWER_TEXT,
    },
    Divergence {
        tool: "ask_user_question",
        case: "multi-select",
        pointer: "/modelText",
        closed_by: "US-245",
        why: ANSWER_TEXT,
    },
    Divergence {
        tool: "ask_user_question",
        case: "multi-select",
        pointer: "/projectedResult",
        closed_by: "US-245",
        why: ANSWER_KEYS,
    },
    Divergence {
        tool: "ask_user_question",
        case: "multi-select",
        pointer: "/typedResult",
        closed_by: "US-245",
        why: ANSWER_KEYS,
    },
    Divergence {
        tool: "ask_user_question",
        case: "options-with-descriptions",
        pointer: "/modelText",
        closed_by: "US-245",
        why: ANSWER_TEXT,
    },
    Divergence {
        tool: "ask_user_question",
        case: "options-with-descriptions",
        pointer: "/projectedResult",
        closed_by: "US-245",
        why: ANSWER_KEYS,
    },
    Divergence {
        tool: "ask_user_question",
        case: "options-with-descriptions",
        pointer: "/typedResult",
        closed_by: "US-245",
        why: ANSWER_KEYS,
    },
    Divergence {
        tool: "ask_user_question",
        case: "other-free-text",
        pointer: "/modelText",
        closed_by: "US-245",
        why: ANSWER_TEXT,
    },
    Divergence {
        tool: "ask_user_question",
        case: "other-free-text",
        pointer: "/projectedResult",
        closed_by: "US-245",
        why: ANSWER_KEYS,
    },
    Divergence {
        tool: "ask_user_question",
        case: "other-free-text",
        pointer: "/typedResult",
        closed_by: "US-245",
        why: ANSWER_KEYS,
    },
    Divergence {
        tool: "ask_user_question",
        case: "other-hidden",
        pointer: "/modelText",
        closed_by: "US-245",
        why: ANSWER_TEXT,
    },
    Divergence {
        tool: "ask_user_question",
        case: "other-hidden",
        pointer: "/projectedResult",
        closed_by: "US-245",
        why: ANSWER_KEYS,
    },
    Divergence {
        tool: "ask_user_question",
        case: "other-hidden",
        pointer: "/typedResult",
        closed_by: "US-245",
        why: ANSWER_KEYS,
    },
    Divergence {
        tool: "ask_user_question",
        case: "single-select",
        pointer: "/modelText",
        closed_by: "US-245",
        why: ANSWER_TEXT,
    },
    Divergence {
        tool: "ask_user_question",
        case: "single-select",
        pointer: "/projectedResult",
        closed_by: "US-245",
        why: ANSWER_KEYS,
    },
    Divergence {
        tool: "ask_user_question",
        case: "single-select",
        pointer: "/typedResult",
        closed_by: "US-245",
        why: ANSWER_KEYS,
    },
    Divergence {
        tool: "ask_user_question",
        case: "two-questions",
        pointer: "/modelText",
        closed_by: "US-245",
        why: ANSWER_TEXT,
    },
    Divergence {
        tool: "ask_user_question",
        case: "two-questions",
        pointer: "/projectedResult",
        closed_by: "US-245",
        why: ANSWER_KEYS,
    },
    Divergence {
        tool: "ask_user_question",
        case: "two-questions",
        pointer: "/typedResult",
        closed_by: "US-245",
        why: ANSWER_KEYS,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "auto-approve",
        pointer: "/modelText",
        closed_by: "US-245",
        why: PLAN_TEXT,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "auto-approve",
        pointer: "/projectedResult",
        closed_by: LICENSING,
        why: PLAN_MESSAGE,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "auto-approve",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: PLAN_MESSAGE,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "cancelled",
        pointer: "/modelText",
        closed_by: "US-245",
        why: PLAN_TEXT,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "clear-context-and-auto-approve",
        pointer: "/modelText",
        closed_by: "US-245",
        why: PLAN_TEXT,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "clear-context-and-auto-approve",
        pointer: "/projectedResult",
        closed_by: LICENSING,
        why: PLAN_MESSAGE,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "clear-context-and-auto-approve",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: PLAN_MESSAGE,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "clear-context-without-a-callback",
        pointer: "/modelText",
        closed_by: "US-245",
        why: PLAN_TEXT,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "clear-context-without-a-callback",
        pointer: "/projectedResult",
        closed_by: LICENSING,
        why: PLAN_MESSAGE,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "clear-context-without-a-callback",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: PLAN_MESSAGE,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "declined",
        pointer: "/modelText",
        closed_by: "US-245",
        why: PLAN_TEXT,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "declined",
        pointer: "/projectedResult",
        closed_by: LICENSING,
        why: PLAN_MESSAGE,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "declined",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: PLAN_MESSAGE,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "manual-approval",
        pointer: "/modelText",
        closed_by: "US-245",
        why: PLAN_TEXT,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "manual-approval",
        pointer: "/projectedResult",
        closed_by: LICENSING,
        why: PLAN_MESSAGE,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "manual-approval",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: PLAN_MESSAGE,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "no-plan-file",
        pointer: "/modelText",
        closed_by: "US-245",
        why: PLAN_TEXT,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "no-plan-file",
        pointer: "/projectedResult",
        closed_by: LICENSING,
        why: PLAN_MESSAGE,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "no-plan-file",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: PLAN_MESSAGE,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "other-feedback",
        pointer: "/modelText",
        closed_by: "US-245",
        why: PLAN_TEXT,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "other-feedback",
        pointer: "/projectedResult",
        closed_by: LICENSING,
        why: PLAN_MESSAGE,
    },
    Divergence {
        tool: "exit_plan_mode",
        case: "other-feedback",
        pointer: "/typedResult",
        closed_by: LICENSING,
        why: PLAN_MESSAGE,
    },
    Divergence {
        tool: "task",
        case: "already-inside-a-subagent",
        pointer: "/outcome",
        closed_by: "US-249",
        why: TASK_DEPTH,
    },
    Divergence {
        tool: "web_fetch",
        case: "a-challenge-that-persists",
        pointer: "/requests",
        closed_by: "US-251",
        why: FETCH_REQUEST,
    },
    Divergence {
        tool: "web_fetch",
        case: "a-challenge-then-success",
        pointer: "/outcome",
        closed_by: "US-251",
        why: FETCH_CHALLENGE,
    },
    Divergence {
        tool: "web_fetch",
        case: "a-json-body",
        pointer: "/requests",
        closed_by: "US-251",
        why: FETCH_REQUEST,
    },
    Divergence {
        tool: "web_fetch",
        case: "a-non-positive-timeout",
        pointer: "/error",
        closed_by: "US-250",
        why: FETCH_ERROR_KIND,
    },
    Divergence {
        tool: "web_fetch",
        case: "a-plain-text-page",
        pointer: "/requests",
        closed_by: "US-251",
        why: FETCH_REQUEST,
    },
    Divergence {
        tool: "web_fetch",
        case: "a-redirect-chain",
        pointer: "/requests",
        closed_by: "US-251",
        why: FETCH_REQUEST,
    },
    Divergence {
        tool: "web_fetch",
        case: "a-server-error",
        pointer: "/requests",
        closed_by: "US-251",
        why: FETCH_REQUEST,
    },
    Divergence {
        tool: "web_fetch",
        case: "an-empty-body",
        pointer: "/requests",
        closed_by: "US-251",
        why: FETCH_REQUEST,
    },
    Divergence {
        tool: "web_fetch",
        case: "an-empty-url",
        pointer: "/error",
        closed_by: "US-250",
        why: FETCH_ERROR_KIND,
    },
    Divergence {
        tool: "web_fetch",
        case: "an-html-page",
        pointer: "/modelText",
        closed_by: "US-252",
        why: FETCH_MARKDOWN,
    },
    Divergence {
        tool: "web_fetch",
        case: "an-html-page",
        pointer: "/projectedResult/content",
        closed_by: "US-252",
        why: FETCH_MARKDOWN,
    },
    Divergence {
        tool: "web_fetch",
        case: "an-html-page",
        pointer: "/requests",
        closed_by: "US-251",
        why: FETCH_REQUEST,
    },
    Divergence {
        tool: "web_fetch",
        case: "an-html-page",
        pointer: "/typedResult/content",
        closed_by: "US-252",
        why: FETCH_MARKDOWN,
    },
    Divergence {
        tool: "web_fetch",
        case: "forbidden-without-the-challenge-header",
        pointer: "/requests",
        closed_by: "US-251",
        why: FETCH_REQUEST,
    },
    Divergence {
        tool: "web_fetch",
        case: "larger-than-max-content-bytes",
        pointer: "/modelText",
        closed_by: "US-250",
        why: FETCH_TRUNCATION,
    },
    Divergence {
        tool: "web_fetch",
        case: "larger-than-max-content-bytes",
        pointer: "/projectedResult",
        closed_by: "US-250",
        why: FETCH_TRUNCATION,
    },
    Divergence {
        tool: "web_fetch",
        case: "larger-than-max-content-bytes",
        pointer: "/requests",
        closed_by: "US-251",
        why: FETCH_REQUEST,
    },
    Divergence {
        tool: "web_fetch",
        case: "larger-than-max-content-bytes",
        pointer: "/typedResult",
        closed_by: "US-250",
        why: FETCH_TRUNCATION,
    },
    Divergence {
        tool: "web_fetch",
        case: "no-content-type",
        pointer: "/requests",
        closed_by: "US-251",
        why: FETCH_REQUEST,
    },
    Divergence {
        tool: "web_fetch",
        case: "not-found",
        pointer: "/requests",
        closed_by: "US-251",
        why: FETCH_REQUEST,
    },
];

/// One tolerated gap between this port and the reference.
#[derive(Debug, Clone, Copy)]
struct Divergence {
    tool: &'static str,
    /// The one case this entry answers for. Wildcards are refused by the audit
    /// test: a gap that spans a tool spans it one case at a time, and saying so
    /// is what keeps the ledger from outliving the divergence.
    case: &'static str,
    /// Matched by prefix against the reported JSON pointer.
    pointer: &'static str,
    /// The story that closes this gap, or [`LICENSING`] when none can.
    closed_by: &'static str,
    /// Why the gap stands, asserted non-empty so an entry cannot be added
    /// without a stated reason.
    why: &'static str,
}

impl Divergence {
    fn covers(&self, tool: &str, case: &str, pointer: &str) -> bool {
        self.tool == tool && self.case == case && pointer.starts_with(self.pointer)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference_commit: String,
    platform: String,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    tool: String,
    case: String,
    arguments: Value,
    /// What the case asked its collaborators to do: the loopback responses, the
    /// skills on offer, the answers a user gives, the subagent run. Absent for
    /// a tool that needs none.
    #[serde(default)]
    script: Value,
    outcome: String,
    #[serde(default)]
    typed_result: Option<Value>,
    /// The second published result shape, which the reference lets a tool
    /// override. Only `grep` and `edit` do.
    #[serde(default)]
    projected_result: Option<Value>,
    #[serde(default)]
    model_text: Option<Value>,
    /// What went out on the wire, recorded for `web_fetch` alone.
    #[serde(default)]
    requests: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

impl Case {
    fn id(&self) -> String {
        format!("{}/{}", self.tool, self.case)
    }

    /// The corpus entry as one comparable document, so a missing field on one
    /// side is a pointer difference rather than a special case.
    fn document(&self) -> Value {
        let mut document = Map::new();
        document.insert("outcome".to_owned(), Value::String(self.outcome.clone()));
        if let Some(typed) = &self.typed_result {
            document.insert("typedResult".to_owned(), typed.clone());
        }
        if let Some(projected) = &self.projected_result {
            document.insert("projectedResult".to_owned(), projected.clone());
        }
        if let Some(text) = &self.model_text {
            document.insert("modelText".to_owned(), text.clone());
        }
        if let Some(requests) = &self.requests {
            document.insert("requests".to_owned(), requests.clone());
        }
        if let Some(error) = &self.error {
            // The corpus records the message digest so a re-pin that reworded a
            // refusal is visible in its diff. It is not a conformance target:
            // the PRD lists byte-identical error text as a non-goal, because
            // reaching a reference digest means writing the reference's own
            // sentence. Only the presence and the kind are compared.
            let mut error = error.clone();
            if let Some(message) = error.pointer_mut("/message").and_then(Value::as_object_mut) {
                message.remove("digest");
            }
            document.insert("error".to_owned(), error);
        }
        Value::Object(document)
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

/// Why this corpus cannot answer for this host, or [`None`] when it can.
///
/// A corpus records what the reference did on one platform, so replaying a
/// Linux capture on a Windows workstation would diff the host rather than the
/// port. That skips with a named reason rather than failing, which is the rule
/// `skip_reason_for` in the tool-surface oracle already states: a corpus that
/// cannot answer must neither fail nor pass silently. Both tests below consult
/// this, so they cannot drift into disagreeing about the same mismatch.
fn skip_reason(corpus: &Corpus) -> Option<String> {
    (corpus.platform != std::env::consts::OS).then(|| {
        format!(
            "skipping the tool-execution replay: the corpus records the {} behavior and this host \
             is {}; recapture with `scripts/parity/tool_execution.py --corpus`",
            corpus.platform,
            std::env::consts::OS
        )
    })
}

fn corpus() -> Corpus {
    let raw =
        fs::read_to_string(repo_root().join(CORPUS_RELATIVE)).expect("the corpus is committed");
    let corpus: Corpus = serde_json::from_str(&raw).expect("the corpus parses");
    assert_eq!(
        corpus.schema_version, CORPUS_SCHEMA_VERSION,
        "the corpus layout moved; regenerate with `scripts/parity/tool_execution.py --corpus`"
    );
    assert_eq!(
        corpus.reference_commit, REFERENCE_COMMIT,
        "the corpus was captured from another commit than this build asserts"
    );
    corpus
}

/// Fails when the corpus no longer covers what NFR-1 commits to, naming the
/// count so a shrunken corpus cannot pass as a green one.
fn assert_corpus_floor(corpus: &Corpus) {
    assert!(
        corpus.cases.len() >= MINIMUM_CASES,
        "the corpus shrank to {} cases, below the floor of {MINIMUM_CASES}",
        corpus.cases.len()
    );
    let mut per_tool: BTreeMap<&str, usize> = BTreeMap::new();
    for case in &corpus.cases {
        *per_tool.entry(case.tool.as_str()).or_default() += 1;
    }
    assert!(
        per_tool.len() >= MINIMUM_TOOLS,
        "the corpus drives {} tools, below the floor of {MINIMUM_TOOLS}: {:?}",
        per_tool.len(),
        per_tool.keys().collect::<Vec<_>>()
    );
    let thin = per_tool
        .iter()
        .filter(|(_, count)| **count < MINIMUM_CASES_PER_TOOL)
        .map(|(tool, count)| format!("{tool} has {count}"))
        .collect::<Vec<_>>();
    assert!(
        thin.is_empty(),
        "every tool carries at least {MINIMUM_CASES_PER_TOOL} cases, but {}",
        thin.join(", ")
    );
}

// --------------------------------------------------------------------------
// The fixture tree
// --------------------------------------------------------------------------

/// Copies the checked-in tree, restoring the dotfiles it stores prefixed.
///
/// Mirrors `materialize_tree` in the capture script: both sides must see the
/// same bytes, or the diff measures the fixture rather than the tool.
fn materialize_tree(source: &Path, destination: &Path) {
    let entries = fs::read_dir(source).expect("the fixture tree is readable");
    fs::create_dir_all(destination).expect("the destination is writable");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_str().expect("a fixture name is UTF-8");
        let restored = name
            .strip_prefix(DOT_PREFIX)
            .map_or_else(|| name.to_owned(), |rest| format!(".{rest}"));
        let target = destination.join(&restored);
        if entry.path().is_dir() {
            materialize_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("a fixture copies");
        }
    }
}

// --------------------------------------------------------------------------
// Normalization and projection, mirroring the capture script
// --------------------------------------------------------------------------

/// The three volatile roots a result can carry, replaced by the placeholders
/// the corpus stores. The tree goes first because it sits inside the
/// scratchpad, so replacing the shorter root first would swallow it.
fn normalize(value: &Value, tree: &str, scratchpad: &str, authority: Option<&str>) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| (key.clone(), normalize(item, tree, scratchpad, authority)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| normalize(item, tree, scratchpad, authority))
                .collect(),
        ),
        Value::String(text) => {
            let mut replaced = text
                .replace(tree, TREE_PLACEHOLDER)
                .replace(scratchpad, SCRATCHPAD_PLACEHOLDER);
            if let Some(authority) = authority {
                replaced = replaced.replace(authority, SERVER_PLACEHOLDER);
            }
            Value::String(if replaced.contains(TREE_PLACEHOLDER) {
                replaced.replace('\\', "/")
            } else {
                replaced
            })
        }
        other => other.clone(),
    }
}

fn digest(text: &str) -> String {
    let hash = Sha256::digest(text.as_bytes());
    let hex = hash.iter().fold(String::new(), |mut accumulator, byte| {
        use std::fmt::Write;
        let _ = write!(accumulator, "{byte:02x}");
        accumulator
    });
    format!("sha256:{}", &hex[..32])
}

/// Whether a captured string may be compared as it stands, mirroring
/// `keeps_literal` in the capture script.
fn keeps_literal(text: &str, authored: &BTreeSet<String>) -> bool {
    if authored.contains(text) {
        return true;
    }
    if (text.starts_with(TREE_PLACEHOLDER) || text.starts_with(SCRATCHPAD_PLACEHOLDER))
        && !text.contains(' ')
    {
        return true;
    }
    // A request target: a URL path and nothing else. Mirrors `_REQUEST_TARGET`.
    if text.starts_with('/')
        && text.len() <= 64
        && text[1..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '/' | '-'))
    {
        return true;
    }
    // An identifier carries no sentence. Mirrors `_IDENTIFIER`.
    let mut characters = text.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && text.len() <= 32
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

fn project(value: &Value, authored: &BTreeSet<String>) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| (key.clone(), project(item, authored)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|i| project(i, authored)).collect()),
        Value::String(text) if !keeps_literal(text, authored) => json!({
            "length": text.chars().count(),
            "digest": digest(text),
        }),
        other => other.clone(),
    }
}

/// Every string the case supplied, at any depth.
fn authored_values(value: &Value, into: &mut BTreeSet<String>) {
    match value {
        Value::Object(fields) => fields.values().for_each(|item| authored_values(item, into)),
        Value::Array(items) => items.iter().for_each(|item| authored_values(item, into)),
        Value::String(text) => {
            into.insert(text.clone());
        }
        _ => {}
    }
}

/// The vocabulary one case authored: the placeholders, its arguments, and its
/// script. Mirrors the seeding in `project_case`.
fn authored_vocabulary(case: &Case) -> BTreeSet<String> {
    let mut authored = BTreeSet::from([
        TREE_PLACEHOLDER.to_owned(),
        SERVER_PLACEHOLDER.to_owned(),
        SCRATCHPAD_PLACEHOLDER.to_owned(),
    ]);
    authored_values(&case.arguments, &mut authored);
    // The script is this repository's own text: the fixture bodies a loopback
    // response served, the skill prompts, the free-text answers. A result that
    // only reads one of them back is a value this corpus supplied, not a
    // reference sentence.
    authored_values(&case.script, &mut authored);
    authored
}

// --------------------------------------------------------------------------
// The loopback origin
// --------------------------------------------------------------------------

/// A single-purpose HTTP origin on `127.0.0.1`, mirroring `LoopbackServer` in
/// the capture script.
///
/// It answers the scripted responses in order, repeating the last one when the
/// client asks again, and records what actually went out on the wire so this
/// port's request can be compared against the one the reference built. NFR-6:
/// nothing here reaches beyond the loopback interface.
struct LoopbackServer {
    authority: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LoopbackServer {
    fn start(responses: Vec<Value>) -> Self {
        let responses = if responses.is_empty() {
            vec![json!({"status": 200, "body": ""})]
        } else {
            responses
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port is available");
        let authority = listener
            .local_addr()
            .expect("the bound address is readable")
            .to_string();
        // A blocking accept would outlive the case, because nothing else
        // connects once the tool has answered. Polling is what lets the thread
        // observe the stop flag and exit with the case.
        listener
            .set_nonblocking(true)
            .expect("the listener polls rather than blocks");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut served = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            exchange(stream, &responses, served, &requests);
                            served += 1;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        Err(_) => return,
                    }
                }
            })
        };
        Self {
            authority,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .map(|recorded| recorded.clone())
            .unwrap_or_default()
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One request answered, mirroring `LoopbackServer._exchange`.
fn exchange(
    mut stream: TcpStream,
    responses: &[Value],
    served: usize,
    requests: &Mutex<Vec<Value>>,
) {
    let mut head = Vec::new();
    let mut buffer = [0u8; 65536];
    while find(&head, b"\r\n\r\n").is_none() {
        let read = match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };
        head.extend_from_slice(&buffer[..read]);
        // A client that opened a TLS handshake against this plain origin is
        // never going to send a request line. Hanging up now is what turns the
        // no-scheme case into a connection error instead of a timeout.
        if !head.first().is_some_and(u8::is_ascii_alphabetic) {
            return;
        }
    }
    let boundary = find(&head, b"\r\n\r\n").unwrap_or(head.len());
    let text = String::from_utf8_lossy(&head[..boundary]).into_owned();
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let (method, remainder) = request_line.split_once(' ').unwrap_or((request_line, ""));
    let target = remainder.split(' ').next().unwrap_or_default();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':').unwrap_or((line, ""));
            (!name.is_empty()).then(|| json!({"name": name, "value": value.trim()}))
        })
        .collect::<Vec<_>>();
    let length = headers
        .iter()
        .find(|header| {
            header["name"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case("content-length"))
        })
        .and_then(|header| header["value"].as_str())
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = head.len().saturating_sub(boundary + 4);
    while body < length {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => body += count,
        }
    }
    if let Ok(mut recorded) = requests.lock() {
        recorded.push(json!({"method": method, "target": target, "headers": headers}));
    }

    let spec = &responses[served.min(responses.len().saturating_sub(1))];
    let payload = response_body(spec);
    let status = spec.get("status").and_then(Value::as_u64).unwrap_or(200);
    let reason = spec.get("reason").and_then(Value::as_str).unwrap_or("OK");
    let mut rendered = vec![format!("HTTP/1.1 {status} {reason}")];
    if let Some(headers) = spec.get("headers").and_then(Value::as_object) {
        for (name, value) in headers {
            rendered.push(format!("{name}: {}", value.as_str().unwrap_or_default()));
        }
    }
    rendered.push(format!("Content-Length: {}", payload.len()));
    rendered.push("Connection: close".to_owned());
    rendered.push(String::new());
    rendered.push(String::new());
    let mut wire = rendered.join("\r\n").into_bytes();
    wire.extend_from_slice(&payload);
    let _ = stream.write_all(&wire);
    let _ = stream.flush();
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A response body, either written out or repeated to a declared size. Mirrors
/// `_response_body`.
fn response_body(spec: &Value) -> Vec<u8> {
    if let Some(repeat) = spec.get("bodyRepeat") {
        let unit = repeat["unit"].as_str().unwrap_or_default();
        let count = repeat["count"]
            .as_u64()
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or_default();
        return unit.repeat(count).into_bytes();
    }
    if let Some(document) = spec.get("json") {
        return python_json(document).into_bytes();
    }
    spec.get("body")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .as_bytes()
        .to_vec()
}

/// `json.dumps(value, sort_keys=True)`, whose default separators carry a space
/// that `serde_json::to_string` omits.
///
/// The body's byte count is what `web_fetch` reports and what the corpus
/// digests, so the spacing is load-bearing rather than cosmetic. Key order is
/// already sorted, because this workspace builds `serde_json` without
/// `preserve_order`.
fn python_json(value: &Value) -> String {
    match value {
        Value::Object(fields) => {
            let body = fields
                .iter()
                .map(|(key, item)| format!("{}: {}", Value::String(key.clone()), python_json(item)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{body}}}")
        }
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_json).collect::<Vec<_>>().join(", ")
        ),
        other => other.to_string(),
    }
}

// --------------------------------------------------------------------------
// Scripted collaborators
// --------------------------------------------------------------------------

/// A user who answers by option index, never by reading a label.
///
/// Selecting by index is what keeps this repository free of the reference's own
/// option strings: `exit_plan_mode` builds its four labels itself, and the case
/// says "the first one" rather than repeating what it says. The labels are read
/// back out of the callback detail, which is the same request a real client
/// renders.
fn spawn_interactive_responder(
    mut receiver: mpsc::Receiver<InteractiveCallbackRequest>,
    script: Value,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(request) = receiver.recv().await {
            match request {
                InteractiveCallbackRequest::Approval { response, .. } => {
                    let _ = response.send(ApprovalDecision::ApproveOnce);
                }
                InteractiveCallbackRequest::ClearContext { response, .. } => {
                    let _ = response.send(Ok(()));
                }
                InteractiveCallbackRequest::Tool {
                    detail, response, ..
                } => {
                    let _ = response.send(Ok(scripted_user_input(&detail, &script)));
                }
            }
        }
    })
}

/// The client output one scripted answer produces, in the shape
/// `user_input_result` reads.
fn scripted_user_input(detail: &Value, script: &Value) -> Value {
    if script
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return json!({"type": "user_input", "result": {"answers": [], "cancelled": true}});
    }
    let empty = Vec::new();
    let specifications = script
        .get("answers")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let questions = detail
        .pointer("/request/questions")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut answers = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        let Some(specification) =
            specifications.get(index.min(specifications.len().saturating_sub(1)))
        else {
            continue;
        };
        let asked = question
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(other) = specification.get("other").and_then(Value::as_str) {
            answers.push(json!({"question": asked, "answer": other, "isOther": true}));
            continue;
        }
        let chosen = specification
            .get("options")
            .and_then(Value::as_array)
            .map_or_else(
                || {
                    specification
                        .get("option")
                        .and_then(Value::as_u64)
                        .into_iter()
                        .collect::<Vec<_>>()
                },
                |options| options.iter().filter_map(Value::as_u64).collect(),
            );
        let labels = chosen
            .into_iter()
            .filter_map(|position| {
                question
                    .pointer(&format!("/options/{position}/label"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>()
            .join(", ");
        answers.push(json!({"question": asked, "answer": labels, "isOther": false}));
    }
    json!({"type": "user_input", "result": {"answers": answers, "cancelled": false}})
}

/// A [`SubagentRunner`] that returns a declared run and reaches no provider.
///
/// The script declares the turn count and the completion flag beside the
/// response because neither can be derived from the text: the reference counts
/// assistant messages in the child transcript and reads completion from the
/// child turn's own terminal status.
struct ScriptedRunner {
    plan: Value,
}

impl SubagentRunner for ScriptedRunner {
    fn run<'a>(
        &'a self,
        _context: ChildContext,
        _cancellation: CancellationToken,
    ) -> SubagentFuture<'a> {
        let run = SubagentRun {
            response: self
                .plan
                .get("response")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            turns_used: self
                .plan
                .get("turns")
                .and_then(Value::as_u64)
                .and_then(|turns| u32::try_from(turns).ok())
                .unwrap_or_default(),
            completed: self
                .plan
                .get("completed")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        };
        Box::pin(async move { Ok(run) })
    }
}

// --------------------------------------------------------------------------
// Driving this port
// --------------------------------------------------------------------------

/// Everything one case needs standing up around its registry.
struct Harness {
    registry: ToolRegistry,
    /// Kept alive for the call: dropping it stops the origin thread.
    server: Option<LoopbackServer>,
    /// Kept alive for the call: dropping it closes the callback queue.
    _responder: Option<tokio::task::JoinHandle<()>>,
}

/// A registry publishing the tools this case drives, with the collaborators its
/// script declares.
async fn harness_for(
    case: &Case,
    tree: &Path,
    home: &Path,
    scratch: &Path,
    index: usize,
) -> Harness {
    let server = case
        .script
        .get("responses")
        .and_then(Value::as_array)
        .map(|responses| LoopbackServer::start(responses.clone()));
    let authority = server.as_ref().map(|server| server.authority.as_str());

    let workspace = Arc::new(Workspace::open(tree).expect("workspace"));
    let review = Arc::new(ReviewManager::new(workspace.clone()));
    let policy = PermissionStore::default();
    policy
        .set_trust(tree, TrustDecision::Trusted, TrustRootKind::Workspace)
        .await
        .expect("trust");
    // A mutating tool snapshots the file it is about to change, which this port
    // only allows inside a turn. A real session always has one open, so the
    // oracle opens one too: without it every write would report the turn guard
    // instead of the tool body.
    review.begin_turn("turn-1").expect("a turn opens");

    let registry = ToolRegistry::default();
    let guard = ToolGuard::new(policy, Arc::new(GrantApproval));

    // A skill the case placed on disk is reachable through the fixture tree's
    // own root; one declared without a directory has no representation here,
    // which is a divergence the ledger carries rather than a fixture to invent.
    let skills = case
        .script
        .get("skills")
        .and_then(Value::as_array)
        .filter(|entries| entries.iter().any(|entry| entry.get("directory").is_some()))
        .map_or_else(SkillDiscovery::default, |_| SkillDiscovery {
            roots: vec![tree.join("skills")],
            ..SkillDiscovery::default()
        });

    // `web_search` is published only where a credential resolves, so the case
    // that names an absent variable leaves the tool unregistered.
    let access = match (case.tool.as_str(), case.script.get("apiKeyVariable")) {
        ("web_search", None) => authority.map(|authority| WebSearchAccess {
            endpoint: format!("http://{authority}"),
            api_key: SecretString::from("parity-key"),
        }),
        _ => None,
    };

    BuiltinTools::new(home, access)
        .register("session-1", skills, &registry, &guard)
        .expect("universal tools register");
    WorkspaceTools::new(workspace, review)
        .register(&registry, &guard)
        .expect("workspace tools register");

    // A skill the case declares already loaded is loaded the only way a session
    // ever loads one: by calling the tool.
    for name in case
        .script
        .get("loaded")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let _ = registry
            .invoke(
                "skill",
                ToolInvocation {
                    call_id: "preload".to_owned(),
                    arguments: json!({"name": name}),
                },
            )
            .await;
    }

    let mut responder = None;
    if matches!(case.tool.as_str(), "ask_user_question" | "exit_plan_mode") {
        let (sender, receiver) = mpsc::channel(8);
        // `exit_plan_mode` is published only inside plan mode, which this port
        // expresses as a plan directory the factory was handed.
        let plan_directory = (case.script.get("agent").and_then(Value::as_str) == Some("plan"))
            .then(|| {
                let directory = scratch.join(format!("plans-{index}"));
                fs::create_dir_all(&directory).expect("the plan directory is writable");
                directory
            });
        InteractiveSessionToolFactory {
            sender,
            plan_directory,
        }
        .register("session-1", &registry)
        .expect("the interactive tools register");
        // A script that declares no answer models a client that never answers.
        // Dropping the receiver closes the queue, which is the failure this
        // port reports when no interaction source is attached.
        if case.script.get("answers").is_some() || case.script.get("cancelled").is_some() {
            responder = Some(spawn_interactive_responder(receiver, case.script.clone()));
        }
    }

    if let Some(plan) = case.script.get("runner") {
        let store = SessionStore::new(scratch.join(format!("sessions-{index}")));
        let mut metadata = store
            .create("parent-1", &tree.display().to_string(), None, 1)
            .expect("the parent session is created");
        // The case names the profile the call runs under. Anything but the
        // default one means the call already sits inside a subagent, which is
        // the depth the delegation guard reads off the parent.
        if case.script.get("agent").and_then(Value::as_str) != Some("default") {
            metadata.agent_profile = Some(json!({
                "name": DEFAULT_SUBAGENT,
                "kind": "subagent",
                "depth": 1,
                "logging": "summary_only",
            }));
            store
                .update_metadata(&metadata)
                .expect("the parent depth is recorded");
        }
        let manager = Arc::new(SubagentManager::new(
            store,
            Arc::new(ScriptedRunner { plan: plan.clone() }),
        ));
        registry
            .register(
                task_spec(),
                task_handler(
                    manager,
                    BTreeMap::from([(DEFAULT_SUBAGENT.to_owned(), built_in_subagent())]),
                    "parent-1".to_owned(),
                ),
            )
            .map(drop)
            .expect("task registers");
    }

    Harness {
        registry,
        server,
        _responder: responder,
    }
}

/// The arguments the reference was given, with the relative paths this corpus
/// stores resolved against the materialized tree and the origin placeholder
/// replaced by the port the case actually bound. Mirrors `case_arguments`.
fn absolute_arguments(arguments: &Value, tree: &Path, authority: Option<&str>) -> Value {
    let mut arguments = arguments.clone();
    if let Some(fields) = arguments.as_object_mut() {
        for key in ["file_path", "path"] {
            if let Some(Value::String(relative)) = fields.get(key)
                && !relative.is_empty()
            {
                let joined = tree.join(relative);
                fields.insert(key.to_owned(), Value::String(joined.display().to_string()));
            }
        }
        if let (Some(authority), Some(Value::String(url))) = (authority, fields.get("url")) {
            let resolved = url.replace(SERVER_PLACEHOLDER, authority);
            fields.insert("url".to_owned(), Value::String(resolved));
        }
    }
    arguments
}

/// What this port produces for one case, in the corpus document shape, with
/// the loopback authority it bound so the caller can normalize it away.
async fn observe(
    case: &Case,
    tree: &Path,
    home: &Path,
    scratch: &Path,
    index: usize,
) -> (Value, Option<String>) {
    let harness = harness_for(case, tree, home, scratch, index).await;
    let authority = harness
        .server
        .as_ref()
        .map(|server| server.authority.as_str());
    let arguments = absolute_arguments(&case.arguments, tree, authority);
    let outcome = harness
        .registry
        .invoke(
            &case.tool,
            ToolInvocation {
                call_id: "call-1".to_owned(),
                arguments,
            },
        )
        .await;

    let mut document = Map::new();
    match outcome {
        Ok(output) => {
            document.insert("outcome".to_owned(), Value::String("returned".to_owned()));
            document.insert("typedResult".to_owned(), output.typed_result.clone());
            // This port publishes no second projection: no tool overrides the
            // typed result on its way to the UI, so the honest observation is
            // that the two are the same document. US-246 and US-247 are what
            // make `grep` and `edit` disagree with that here.
            document.insert("projectedResult".to_owned(), output.typed_result);
            document.insert("modelText".to_owned(), Value::String(output.model_text));
        }
        Err(error) => {
            document.insert("outcome".to_owned(), Value::String("raised".to_owned()));
            // The message is compared by presence, never by content: the PRD
            // lists byte-identical error text as a non-goal, so a message must
            // name the same cause rather than reproduce the reference wording.
            document.insert(
                "error".to_owned(),
                json!({
                    "type": error_type(&error),
                    "message": { "present": !error.to_string().is_empty() },
                }),
            );
        }
    }
    // Only `web_fetch` records the wire, matching the capture script: the
    // search backend's request carries a credential and a body, and neither
    // belongs in a committed corpus.
    if case.tool == "web_fetch"
        && let Some(server) = &harness.server
    {
        document.insert("requests".to_owned(), Value::Array(server.requests()));
    }
    let authority = harness
        .server
        .as_ref()
        .map(|server| server.authority.clone());
    (Value::Object(document), authority)
}

/// The error's kind, named the way the reference names its exception class, so
/// the two sides compare a kind rather than a language's type name.
fn error_type(error: &ToolError) -> &'static str {
    match error {
        ToolError::SchemaViolation { .. } => "ValidationError",
        _ => "ToolError",
    }
}

// --------------------------------------------------------------------------
// Diffing
// --------------------------------------------------------------------------

/// Every difference between the two documents, as a JSON pointer and both
/// values.
///
/// When the two sides disagree on the outcome itself, only that is reported:
/// every field below it is a consequence of returning where the reference
/// raised, and reporting them as well would turn one gap into four ledger
/// entries that all close together.
fn compare(expected: &Value, actual: &Value) -> Vec<Difference> {
    let outcome = |document: &Value| document.get("outcome").cloned().unwrap_or(Value::Null);
    if outcome(expected) != outcome(actual) {
        return vec![Difference {
            pointer: "/outcome".to_owned(),
            expected: outcome(expected),
            actual: outcome(actual),
        }];
    }
    let mut found = Vec::new();
    differences("", expected, actual, &mut found);
    found
}

/// Every difference under `pointer`, as a JSON pointer and both values.
///
/// All of them rather than the first, because the ledger is matched per
/// pointer: reporting only the earliest would leave an entry for a later field
/// permanently unexercised, and the staleness check would then delete a gap
/// that is still open.
fn differences(pointer: &str, expected: &Value, actual: &Value, into: &mut Vec<Difference>) {
    match (expected, actual) {
        (Value::Object(left), Value::Object(right)) => {
            for (key, value) in left {
                let nested = format!("{pointer}/{key}");
                match right.get(key) {
                    Some(other) => differences(&nested, value, other, into),
                    None => into.push(Difference {
                        pointer: nested,
                        expected: value.clone(),
                        actual: Value::String("absent".to_owned()),
                    }),
                }
            }
            for key in right.keys().filter(|key| !left.contains_key(*key)) {
                into.push(Difference {
                    pointer: format!("{pointer}/{key}"),
                    expected: Value::String("absent".to_owned()),
                    actual: right[key].clone(),
                });
            }
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
            for (index, (a, b)) in left.iter().zip(right).enumerate() {
                differences(&format!("{pointer}/{index}"), a, b, into);
            }
        }
        _ if expected == actual => {}
        _ => into.push(Difference {
            pointer: pointer.to_owned(),
            expected: expected.clone(),
            actual: actual.clone(),
        }),
    }
}

#[derive(Debug)]
struct Difference {
    pointer: String,
    expected: Value,
    actual: Value,
}

/// What this port produces for one case, projected the way the corpus is.
async fn observed_document(case: &Case, source: &Path, scratch: &Path, index: usize) -> Value {
    let tree = scratch.join(format!("case-{index}"));
    materialize_tree(source, &tree);
    let tree = tree.canonicalize().expect("the tree resolves");
    let home = scratch.join("home");
    fs::create_dir_all(&home).expect("home");
    let scratchpad = scratch.canonicalize().expect("the scratch root resolves");

    let (observed, authority) = observe(case, &tree, &home, &scratchpad, index).await;
    project(
        &normalize(
            &observed,
            &tree.display().to_string(),
            &scratchpad.display().to_string(),
            authority.as_deref(),
        ),
        &authored_vocabulary(case),
    )
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

#[tokio::test]
async fn tool_execution_matches_the_reference_except_for_the_recorded_gap() {
    let corpus = corpus();
    assert_corpus_floor(&corpus);
    if let Some(reason) = skip_reason(&corpus) {
        eprintln!("{reason}");
        return;
    }

    let root = repo_root();
    let source = root.join(TREE_RELATIVE);
    let scratch = tempfile::tempdir().expect("tempdir");

    let mut conforming = 0;
    let mut tolerated: BTreeSet<(String, String)> = BTreeSet::new();
    let mut unlisted = Vec::new();

    for (index, case) in corpus.cases.iter().enumerate() {
        // A fresh tree per case, so a mutating tool cannot leak into the next
        // one. The capture script isolates the same way.
        let observed = observed_document(case, &source, scratch.path(), index).await;

        let found = compare(&case.document(), &observed);
        if found.is_empty() {
            conforming += 1;
        }
        for difference in found {
            match LEDGER
                .iter()
                .find(|entry| entry.covers(&case.tool, &case.case, &difference.pointer))
            {
                Some(entry) => {
                    tolerated.insert((
                        format!("{}/{} at {}", entry.tool, entry.case, entry.pointer),
                        entry.closed_by.to_owned(),
                    ));
                }
                None => unlisted.push(format!(
                    "{} at {}: the reference says {}, this port says {}",
                    case.id(),
                    difference.pointer,
                    difference.expected,
                    difference.actual
                )),
            }
        }
    }

    println!(
        "tool execution: {conforming}/{} cases match the reference at {}, {} ledger entries \
         exercised",
        corpus.cases.len(),
        &corpus.reference_commit[..12],
        tolerated.len()
    );
    for (entry, closed_by) in &tolerated {
        println!("  tolerated {entry} until {closed_by}");
    }

    assert!(
        unlisted.is_empty(),
        "execution divergences outside the ledger:\n  {}",
        unlisted.join("\n  ")
    );
}

#[tokio::test]
async fn a_ledger_entry_whose_divergence_is_fixed_fails_the_suite() {
    let corpus = corpus();
    if let Some(reason) = skip_reason(&corpus) {
        eprintln!("{reason}");
        return;
    }
    let root = repo_root();
    let source = root.join(TREE_RELATIVE);
    let scratch = tempfile::tempdir().expect("tempdir");

    // Which ledger entries an actual divergence still reaches.
    let mut exercised: BTreeSet<usize> = BTreeSet::new();
    for (index, case) in corpus.cases.iter().enumerate() {
        let observed = observed_document(case, &source, scratch.path(), index).await;
        for difference in compare(&case.document(), &observed) {
            if let Some(position) = LEDGER
                .iter()
                .position(|entry| entry.covers(&case.tool, &case.case, &difference.pointer))
            {
                exercised.insert(position);
            }
        }
    }

    let stale = LEDGER
        .iter()
        .enumerate()
        .filter(|(position, _)| !exercised.contains(position))
        .map(|(_, entry)| {
            format!(
                "{}/{} at {} ({})",
                entry.tool, entry.case, entry.pointer, entry.closed_by
            )
        })
        .collect::<Vec<_>>();

    assert!(
        stale.is_empty(),
        "these ledger entries no longer describe a divergence and must be removed, which is what \
         keeps the ledger from rotting:\n  {}",
        stale.join("\n  ")
    );
}

#[test]
fn the_committed_corpus_carries_no_reference_prose() {
    let corpus = corpus();
    let mut offending = Vec::new();
    for case in &corpus.cases {
        let authored = authored_vocabulary(case);
        collect_literals(&case.document(), &authored, &mut offending);
    }
    assert!(
        offending.is_empty(),
        "the corpus carries strings that are neither an argument it authored, a normalized path, \
         nor an identifier, so they may be reference prose: {offending:?}"
    );
}

/// Every committed string that `keeps_literal` would not have admitted, which
/// is what a prose leak would look like.
fn collect_literals(value: &Value, authored: &BTreeSet<String>, into: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            // A digested string is recorded as exactly these two keys.
            if fields.len() == 2 && fields.contains_key("digest") && fields.contains_key("length") {
                return;
            }
            fields
                .values()
                .for_each(|item| collect_literals(item, authored, into));
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| collect_literals(item, authored, into)),
        Value::String(text) if !keeps_literal(text, authored) => into.push(text.clone()),
        _ => {}
    }
}

#[test]
fn the_projection_agrees_with_the_capture_script() {
    // The two sides must digest identically, or every projected string would
    // read as a divergence. Anchored on a value computed by the Python side
    // with `hashlib.sha256(text.encode()).hexdigest()[:32]`.
    assert_eq!(
        digest("alpha one"),
        "sha256:447ddb49ae0e88206741f4e0d10b1371"
    );

    let authored = BTreeSet::from(["gather".to_owned()]);
    assert!(keeps_literal("gather", &authored));
    assert!(keeps_literal("{tree}/alpha.txt", &authored));
    assert!(keeps_literal("ToolError", &authored));
    assert!(keeps_literal("in_progress", &authored));
    // The request-target rule, which is what lets the corpus say which path the
    // reference asked for.
    assert!(keeps_literal("/page.html", &authored));
    assert!(keeps_literal("/", &authored));
    // A header name carries a hyphen, which the identifier rule admits.
    assert!(keeps_literal("Accept-Language", &authored));
    assert!(!keeps_literal(
        "File not found at: {tree}/nowhere.txt",
        &authored
    ));
    assert!(!keeps_literal("Updated 2 todos", &authored));
    assert!(!keeps_literal("", &authored));

    // `json.dumps` separators, which decide the byte count a fetched body
    // reports.
    assert_eq!(
        python_json(&json!({"count": 2, "fixture": true})),
        r#"{"count": 2, "fixture": true}"#
    );
}

#[test]
fn every_ledger_entry_names_what_closes_it() {
    for entry in LEDGER {
        assert!(
            entry.closed_by.starts_with("US-") || entry.closed_by == LICENSING,
            "a tolerated divergence names the story that closes it or the licensing boundary \
             that keeps it open, not {}",
            entry.closed_by
        );
        assert!(entry.pointer.starts_with('/'), "{}", entry.pointer);
        assert_ne!(
            entry.case, "*",
            "a ledger entry answers for one case, not for every case of {}: a wildcard outlives \
             the divergence it was written for",
            entry.tool
        );
        assert!(!entry.why.is_empty(), "{}/{}", entry.tool, entry.case);
    }
}

#[test]
fn the_fixture_tree_is_committed_and_deterministic() {
    let source = repo_root().join(TREE_RELATIVE);
    let scratch = tempfile::tempdir().expect("tempdir");
    let first = scratch.path().join("first");
    let second = scratch.path().join("second");
    materialize_tree(&source, &first);
    materialize_tree(&source, &second);

    let listing = |root: &Path| -> BTreeMap<String, Vec<u8>> {
        let mut found = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).expect("readable").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let relative = path
                        .strip_prefix(root)
                        .expect("under the root")
                        .display()
                        .to_string();
                    found.insert(relative, fs::read(&path).expect("readable"));
                }
            }
        }
        found
    };

    let first = listing(&first);
    assert_eq!(
        first,
        listing(&second),
        "materialization is not deterministic"
    );
    assert!(
        first.contains_key(".gitignore"),
        "the dot-prefixed fixture is restored: {:?}",
        first.keys().collect::<Vec<_>>()
    );
    assert!(first.contains_key("alpha.txt"));
    assert!(
        first.contains_key("skills/small/SKILL.md"),
        "the skill fixtures are committed"
    );
}

/// Recaptures against the local checkout and asserts the committed corpus is
/// still what the pinned reference answers.
///
/// This is the only test here that needs the checkout, and it skips naming the
/// pin and the way back when the checkout is absent or off-pin. The replay
/// above runs regardless, which is what keeps a missing checkout from failing
/// `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "tool execution") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let recaptured = repository.join("target/tool-execution-corpus.json");
    let output = Command::new("python3")
        .arg(repository.join(CAPTURE_SCRIPT))
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/tool-execution-full.json"))
        .arg("--corpus")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the tool-execution capture script runs");
    assert!(
        output.status.success(),
        "the tool-execution capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fresh: Value = serde_json::from_str(
        &fs::read_to_string(&recaptured).expect("the recaptured corpus is readable"),
    )
    .expect("the recaptured corpus parses");
    let committed: Value = serde_json::from_str(
        &fs::read_to_string(repository.join(CORPUS_RELATIVE)).expect("the corpus is readable"),
    )
    .expect("the corpus parses");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate it \
         with `{CAPTURE_SCRIPT} --corpus`"
    );
}
