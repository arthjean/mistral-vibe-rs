//! Differential conformance runner for the chat-input parity corpus.
//!
//! Canonical traces recorded from the pinned Python reference are replayed
//! through the Rust transition boundary. `tests/parity/expectations.json`
//! declares, per trace and per dimension, how Rust stands against the
//! reference today:
//!
//! - `parity`: Rust must match; a divergence fails.
//! - `gap`: Rust is known to diverge, at exactly the events and pointers
//!   [`DIVERGENCES`] ledgers with their reasons; matching now also fails, so
//!   closing a gap cannot go unrecorded.
//! - `deferred`: the oracle records the dimension but no Rust observation
//!   exists yet. The runner does not compare it and names the story that will.
//! - `unavailable`: the scenario could not be recorded on this host.
//!
//! Every dimension a trace carries must be declared. A recorded dimension with
//! no entry fails the corpus check instead of being silently dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::tui::attachments::{PromptDraft, normalize_pasted_text, prepare_submission};
use crate::tui::chat_input::{ChatInputState, InputEffect, InputEvent, Safety};
use crate::tui::commands::CommandContext;
use crate::tui::completion::CompletionRequest;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Version 2 records completion descriptions as `{length, digest}`: they are
/// prose the reference authors, so the corpus never carries their text.
const SCHEMA_VERSION: u32 = 2;
const MAX_EFFECT_ROUNDS: usize = 8;
/// Recorded in the corpus but not compared yet, with the story that closes it.
const DEFERRED_DIMENSIONS: &[(&str, &str)] = &[];
/// Placeholder the oracle substitutes for the recording workspace path.
const WORKSPACE_PLACEHOLDER: &str = "__WORKSPACE__";
/// Fields the reference records inside `state` that Rust does not model yet.
///
/// They are dropped from the expected observation before comparison so a
/// single unmodeled field cannot mask every other state assertion in the
/// corpus. Each entry names the story that supplies the missing state; the
/// entry is removed when that story lands, turning the field into a real
/// assertion.
const UNMODELED_STATE_PATHS: &[(&str, &str)] = &[];

/// One pointer at which this port diverges from the pinned reference in a
/// trace whose dimension `expectations.json` declares `gap`, and the events
/// that diverge there.
///
/// `path` is the first differing field of an observation as the runner reports
/// it after the dimension name (`completion.items`, `[0].mode`), and `$` when
/// the whole observation differs, as an effect list of another length. A `gap`
/// dimension must diverge at exactly the ledgered events and pointers: an
/// unledgered one fails as a regression and a ledgered one that stopped
/// reproducing fails as stale.
struct LedgeredDivergence {
    trace: &'static str,
    dimension: &'static str,
    path: &'static str,
    events: &'static [usize],
    reason: &'static str,
}

// Reasons, one per reference change the corpus recaptured at
// 4a96003 (2.25.7). Every reference path is
// read at that commit; every port path at the state these entries were
// measured against.
const WHY_REGISTRY: &str = "row 2: the reference registry grew. v2.24.2 registers /log-level \
     (vibe/cli/commands.py:103) and v2.25.3 /branch (vibe/cli/commands.py:214), both ungated, and \
     v2.25.7 removes the Vibe Code gate, so /teleport (vibe/cli/commands.py:140) and \
     /remote-project (vibe/cli/commands.py:145) are always registered. This port has no LogLevel or \
     Branch command (crates/vibe-cli/src/tui/commands.rs:10-39) and offers /teleport and \
     /remote-project only when CommandContext::vibe_code_enabled is set \
     (crates/vibe-cli/src/tui/commands.rs:414), which the replay leaves off, so its popup lacks \
     the missing aliases";
const WHY_CLEAR_DESCRIPTION: &str = "row 2: v2.24.1 rewrote the /clear description \
     (vibe/cli/commands.py:75-80), recorded here as its digest. This port still describes /clear \
     with the 2.24.0 text (crates/vibe-cli/src/tui/commands.rs:175), whose digest the replay \
     reports";
const WHY_BARE_AT_LISTING: &str = "row 14: v2.25.3 answers a bare @ with \
     PathCompleter._list_current_directory (vibe/cli/autocompletion/completers.py:436-461, routed \
     at vibe/cli/autocompletion/completers.py:474-475): every non-hidden working-directory entry, \
     with no ignore rules, sorted case-insensitively, so @build.log and @ignored/ appear and \
     @README.md sorts after @notes.txt. This port answers it from its gitignore-filtered index \
     (crates/vibe-cli/src/tui/completion/path.rs:384-391 and :635-660) in its own order";
const WHY_CARET_REQUERY: &str = "row 14: v2.24.1 re-runs completion on a pure caret move \
     (ChatTextArea.watch_selection, vibe/cli/textual_ui/widgets/chat_input/text_area.py:452-464), \
     so moving the caret re-ranks, reopens or closes the popup for the text before it, and a later \
     Up, Tab or Enter acts on that popup. This port refreshes completion only after an edit bumps \
     the editor revision (crates/vibe-cli/src/tui/chat_input.rs:585-599), so the popup keeps the \
     list from before the move and the keys that follow act on it";
const WHY_WHOLE_WORD_REPLACEMENT: &str = "row 14: v2.24.1 makes \
     CommandCompleter.get_replacement_range replace the whole command word up to the first \
     whitespace regardless of the caret (vibe/cli/autocompletion/completers.py:91-101), so Tab \
     with the caret inside /mcp yields '/mcp add x' with the caret at 4. This port replaces only up \
     to the caret (crates/vibe-cli/src/tui/completion.rs:740-747), yielding '/mcp p add x' with \
     the caret at 5; the comparator reports the cursor field first, which masks the text \
     divergence behind it";
const WHY_PASTED_PATH_MENTION: &str = "row 14: v2.25.3 passes every paste through \
     maybe_prepend_at_for_path (vibe/cli/textual_ui/widgets/chat_input/text_area.py:387, \
     vibe/cli/textual_ui/widgets/chat_input/paste_path.py:27-40 and :80-92), which turns a pasted \
     existing absolute path of any type into an @ mention. This port's normalize_pasted_text \
     mentions image paths only (crates/vibe-cli/src/tui/path_mentions.rs:7-17), so a pasted text \
     file path stays verbatim";
const WHY_DRAFT_LOAD_MARKER: &str = "row 14: v2.25.5 clears the history load marker once Down \
     leaves history navigation (vibe/cli/textual_ui/widgets/chat_input/body.py:228-231), so the \
     restored draft reports loadedEntry false. This port restores the draft through \
     load_history_text (crates/vibe-cli/src/tui/input.rs:535-538), which sets history_loaded \
     (crates/vibe-cli/src/tui/input.rs:616)";
const WHY_TURN_MESSAGE: &str = "row 14: v2.24.1 renames TurnStartParams.input to message \
     (vibe/app_server/protocol.py:1925, input surviving only as a read-only property at \
     vibe/app_server/protocol.py:1932-1934). This port's TurnRequest still serializes input \
     (crates/vibe-app-server/src/client.rs:222). Masked behind that first field: with no session \
     directory v2.25.5 inlines the image as base64 (vibe/core/session/image_snapshot.py:77-82) \
     where this port records a file source";
const WHY_PATH_RANKING: &str = "row 14: v2.24.5 rewrote the reference fuzzy scorer \
     (vibe/cli/autocompletion/fuzzy.py, fuzzy_match at :47). Scoring the pattern /e with the \
     2.24.0 and 2.25.7 fuzzy_match gives docs/guide.md 90.0 in both but src/inner/deep.rs 93.5 \
     then 84.5, and PathCompleter ranks on that score here (MatchRank, \
     vibe/cli/autocompletion/completers.py:105-115), so for @/e the reference now ranks \
     @docs/guide.md second. This port's scorer (crates/vibe-cli/src/tui/completion/fuzzy.rs:60) \
     still ranks @src/inner/deep.rs second, as the 2.24.0 capture did";
const WHY_REGISTRY_AND_SLASH_RANKING: &str = "rows 2 and 14: two reference changes meet at \
     the query /mp. The registry grew (see WHY_REGISTRY): /remote-project, always registered \
     since v2.25.7 (vibe/cli/commands.py:145), matches mp and joins the list. And the v2.24.5 \
     fuzzy scorer rewrite (vibe/cli/autocompletion/fuzzy.py, fuzzy_match at :47) scores compact \
     110.0 where the 2.24.0 scorer gave 198.0, against 157.5 for mcp in both, so \
     CommandCompleter._fuzzy_filter (vibe/cli/autocompletion/completers.py:54-68, unchanged in \
     this range) now ranks /mcp above /compact. This port lacks /remote-project in the default \
     context (crates/vibe-cli/src/tui/commands.rs:414) and its scorer \
     (crates/vibe-cli/src/tui/completion/fuzzy.rs:60) still ranks /compact first";
const WHY_TELEPORT_MODE: &str = "row 14: v2.25.7 always registers /teleport \
     (vibe/cli/commands.py:140), so a leading & switches to teleport mode \
     (ChatTextArea.mode_characters, vibe/cli/textual_ui/widgets/chat_input/text_area.py:804-808). \
     This port enables that mode only when CommandContext::vibe_code_enabled is set \
     (crates/vibe-cli/src/tui/chat_input.rs:746-748, crates/vibe-cli/src/tui/commands.rs:414), \
     which the replay leaves off, so & stays literal prompt text";

const DIVERGENCES: &[LedgeredDivergence] = &[
    LedgeredDivergence {
        trace: "async-cancelled-by-space",
        dimension: "state",
        path: "completion.items",
        events: &[0],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "async-cancelled-by-space",
        dimension: "render",
        path: "popupRows",
        events: &[0],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "async-generation-supersedes",
        dimension: "state",
        path: "completion.items",
        events: &[0],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "async-generation-supersedes",
        dimension: "render",
        path: "popupRows",
        events: &[0],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "async-history-recall-closes",
        dimension: "state",
        path: "completion.items",
        events: &[0],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "async-history-recall-closes",
        dimension: "state",
        path: "completion.items",
        events: &[4, 5],
        reason: WHY_CARET_REQUERY,
    },
    LedgeredDivergence {
        trace: "async-history-recall-closes",
        dimension: "effects",
        path: "$",
        events: &[5],
        reason: WHY_CARET_REQUERY,
    },
    LedgeredDivergence {
        trace: "async-history-recall-closes",
        dimension: "render",
        path: "popupRows",
        events: &[0],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "async-history-recall-closes",
        dimension: "render",
        path: "popupRows",
        events: &[4],
        reason: WHY_CARET_REQUERY,
    },
    LedgeredDivergence {
        trace: "async-history-recall-closes",
        dimension: "render",
        path: "cursorCell[1]",
        events: &[5],
        reason: WHY_CARET_REQUERY,
    },
    LedgeredDivergence {
        trace: "commands-excluded",
        dimension: "state",
        path: "completion.items",
        events: &[0],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "commands-excluded",
        dimension: "render",
        path: "popupRows",
        events: &[0],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "commands-full-surface",
        dimension: "state",
        path: "completion.items",
        events: &[0],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "commands-full-surface",
        dimension: "render",
        path: "popupRows",
        events: &[0],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "commands-unknown-alias",
        dimension: "state",
        path: "completion.items",
        events: &[0],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "commands-unknown-alias",
        dimension: "render",
        path: "popupRows",
        events: &[0],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "corpus-dot-query",
        dimension: "state",
        path: "completion.items",
        events: &[5],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "corpus-dot-query",
        dimension: "render",
        path: "popupRows",
        events: &[5],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "corpus-gitignore",
        dimension: "state",
        path: "completion.items",
        events: &[5],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "corpus-gitignore",
        dimension: "render",
        path: "popupRows",
        events: &[5],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "corpus-hidden-files",
        dimension: "state",
        path: "completion.items",
        events: &[5],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "corpus-hidden-files",
        dimension: "render",
        path: "popupRows",
        events: &[5],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "corpus-nested-ranking",
        dimension: "state",
        path: "completion.items",
        events: &[5],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "corpus-nested-ranking",
        dimension: "render",
        path: "popupRows",
        events: &[5],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "dnd-text-path-untouched",
        dimension: "state",
        path: "cursor",
        events: &[0],
        reason: WHY_PASTED_PATH_MENTION,
    },
    LedgeredDivergence {
        trace: "dnd-text-path-untouched",
        dimension: "render",
        path: "visualLines[0]",
        events: &[0],
        reason: WHY_PASTED_PATH_MENTION,
    },
    LedgeredDivergence {
        trace: "external-editor-refreshes-completion",
        dimension: "state",
        path: "completion.items",
        events: &[5],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "history-down-restores-draft",
        dimension: "state",
        path: "history.loadedEntry",
        events: &[9],
        reason: WHY_DRAFT_LOAD_MARKER,
    },
    LedgeredDivergence {
        trace: "mention-directory-submission",
        dimension: "state",
        path: "completion.items",
        events: &[7],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "mention-directory-submission",
        dimension: "render",
        path: "popupRows",
        events: &[7],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "mention-external-path-submission",
        dimension: "state",
        path: "completion.items",
        events: &[8],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "mention-external-path-submission",
        dimension: "state",
        path: "completion.items[1].label",
        events: &[10],
        reason: WHY_PATH_RANKING,
    },
    LedgeredDivergence {
        trace: "mention-external-path-submission",
        dimension: "render",
        path: "popupRows",
        events: &[8],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "mention-external-path-submission",
        dimension: "render",
        path: "popupRows[1]",
        events: &[10],
        reason: WHY_PATH_RANKING,
    },
    LedgeredDivergence {
        trace: "mention-image-payload-submission",
        dimension: "submission",
        path: "input",
        events: &[1],
        reason: WHY_TURN_MESSAGE,
    },
    LedgeredDivergence {
        trace: "mention-missing-path-submission",
        dimension: "state",
        path: "completion.items",
        events: &[5],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "mention-missing-path-submission",
        dimension: "render",
        path: "popupRows",
        events: &[5],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "mention-text-file-submission",
        dimension: "state",
        path: "completion.items",
        events: &[10],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "mention-text-file-submission",
        dimension: "render",
        path: "popupRows",
        events: &[10],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "mode-backspace-keeps-text",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1, 3, 4],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "mode-backspace-resets",
        dimension: "state",
        path: "completion.items",
        events: &[0],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "modes-slash-prefix",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "modes-slash-prefix",
        dimension: "state",
        path: "completion.items[0].description.digest",
        events: &[2],
        reason: WHY_CLEAR_DESCRIPTION,
    },
    LedgeredDivergence {
        trace: "modes-teleport-available",
        dimension: "state",
        path: "mode",
        events: &[0, 1, 2, 3, 4, 5, 6],
        reason: WHY_TELEPORT_MODE,
    },
    LedgeredDivergence {
        trace: "modes-teleport-available",
        dimension: "effects",
        path: "[0].mode",
        events: &[0],
        reason: WHY_TELEPORT_MODE,
    },
    LedgeredDivergence {
        trace: "modes-teleport-available",
        dimension: "render",
        path: "cursorCell[1]",
        events: &[0, 1, 2, 3, 4, 5, 6],
        reason: WHY_TELEPORT_MODE,
    },
    LedgeredDivergence {
        trace: "paste-mode-characters",
        dimension: "state",
        path: "completion.items[0].description.digest",
        events: &[0],
        reason: WHY_CLEAR_DESCRIPTION,
    },
    LedgeredDivergence {
        trace: "path-accept-directory",
        dimension: "state",
        path: "completion.items",
        events: &[8],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-accept-directory",
        dimension: "render",
        path: "popupRows",
        events: &[8],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-accept-inserts-space",
        dimension: "state",
        path: "completion.items",
        events: &[8],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-accept-inserts-space",
        dimension: "render",
        path: "popupRows",
        events: &[8],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-mid-token-completion",
        dimension: "state",
        path: "completion.items",
        events: &[8],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-mid-token-completion",
        dimension: "state",
        path: "completion.items",
        events: &[16, 17],
        reason: WHY_CARET_REQUERY,
    },
    LedgeredDivergence {
        trace: "path-mid-token-completion",
        dimension: "state",
        path: "cursor",
        events: &[18],
        reason: WHY_CARET_REQUERY,
    },
    LedgeredDivergence {
        trace: "path-mid-token-completion",
        dimension: "render",
        path: "popupRows",
        events: &[8],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-mid-token-completion",
        dimension: "render",
        path: "popupRows",
        events: &[16, 17],
        reason: WHY_CARET_REQUERY,
    },
    LedgeredDivergence {
        trace: "path-mid-token-completion",
        dimension: "render",
        path: "cursorCell[1]",
        events: &[18],
        reason: WHY_CARET_REQUERY,
    },
    LedgeredDivergence {
        trace: "path-trigger-after-punctuation",
        dimension: "state",
        path: "completion.items",
        events: &[4],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-trigger-after-punctuation",
        dimension: "render",
        path: "popupRows",
        events: &[4],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-trigger-bare-at",
        dimension: "state",
        path: "completion.items",
        events: &[0],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-trigger-bare-at",
        dimension: "render",
        path: "popupRows",
        events: &[0],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-trigger-basic",
        dimension: "state",
        path: "completion.items",
        events: &[8],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-trigger-basic",
        dimension: "render",
        path: "popupRows",
        events: &[8],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-trigger-nested",
        dimension: "state",
        path: "completion.items",
        events: &[5],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-trigger-nested",
        dimension: "render",
        path: "popupRows",
        events: &[5],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-trigger-space-closes",
        dimension: "state",
        path: "completion.items",
        events: &[5],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "path-trigger-space-closes",
        dimension: "render",
        path: "popupRows",
        events: &[5],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "popup-render-120",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "popup-render-120",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "popup-render-40",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "popup-render-40",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "popup-render-80",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "popup-render-80",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "popup-render-path-bound",
        dimension: "state",
        path: "completion.items",
        events: &[0],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "popup-render-path-bound",
        dimension: "render",
        path: "popupRows",
        events: &[0],
        reason: WHY_BARE_AT_LISTING,
    },
    LedgeredDivergence {
        trace: "slash-double-marker",
        dimension: "state",
        path: "completion.items",
        events: &[0],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-double-marker",
        dimension: "render",
        path: "popupRows",
        events: &[0],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-no-match",
        dimension: "state",
        path: "completion.items",
        events: &[0],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-no-match",
        dimension: "render",
        path: "popupRows",
        events: &[0],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-arrows",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1, 2, 3, 4],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-arrows",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1, 2, 3, 4],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-caret-at-line-start",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-caret-at-line-start",
        dimension: "state",
        path: "completion.items[0].description.digest",
        events: &[2],
        reason: WHY_CLEAR_DESCRIPTION,
    },
    LedgeredDivergence {
        trace: "slash-popup-caret-at-line-start",
        dimension: "state",
        path: "completion.items",
        events: &[3],
        reason: WHY_CARET_REQUERY,
    },
    LedgeredDivergence {
        trace: "slash-popup-caret-at-line-start",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-caret-at-line-start",
        dimension: "render",
        path: "popupRows",
        events: &[3],
        reason: WHY_CARET_REQUERY,
    },
    LedgeredDivergence {
        trace: "slash-popup-enter-submits",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-enter-submits",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-escape",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1, 2],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-escape",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1, 2],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-mid-token-cursor",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1, 16],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-mid-token-cursor",
        dimension: "state",
        path: "completion.items",
        events: &[2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14],
        reason: WHY_REGISTRY_AND_SLASH_RANKING,
    },
    LedgeredDivergence {
        trace: "slash-popup-mid-token-cursor",
        dimension: "state",
        path: "completion.items",
        events: &[15],
        reason: WHY_CARET_REQUERY,
    },
    LedgeredDivergence {
        trace: "slash-popup-mid-token-cursor",
        dimension: "state",
        path: "cursor",
        events: &[17],
        reason: WHY_WHOLE_WORD_REPLACEMENT,
    },
    LedgeredDivergence {
        trace: "slash-popup-mid-token-cursor",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1, 16],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-mid-token-cursor",
        dimension: "render",
        path: "popupRows",
        events: &[2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14],
        reason: WHY_REGISTRY_AND_SLASH_RANKING,
    },
    LedgeredDivergence {
        trace: "slash-popup-mid-token-cursor",
        dimension: "render",
        path: "popupRows",
        events: &[15],
        reason: WHY_CARET_REQUERY,
    },
    LedgeredDivergence {
        trace: "slash-popup-mid-token-cursor",
        dimension: "render",
        path: "cursorCell[1]",
        events: &[17],
        reason: WHY_WHOLE_WORD_REPLACEMENT,
    },
    LedgeredDivergence {
        trace: "slash-popup-right",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1, 3],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-right",
        dimension: "state",
        path: "completion.items",
        events: &[2],
        reason: WHY_CARET_REQUERY,
    },
    LedgeredDivergence {
        trace: "slash-popup-right",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1, 3],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-right",
        dimension: "render",
        path: "popupRows",
        events: &[2],
        reason: WHY_CARET_REQUERY,
    },
    LedgeredDivergence {
        trace: "slash-popup-tab-applies",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-tab-applies",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-wraps-selection",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1, 2],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-popup-wraps-selection",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1, 2],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-ranking-c",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-ranking-c",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-ranking-empty-boosts",
        dimension: "state",
        path: "completion.items",
        events: &[0],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-ranking-empty-boosts",
        dimension: "render",
        path: "popupRows",
        events: &[0],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-ranking-fuzzy",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-ranking-fuzzy",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-skill-entries",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1, 2],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-skill-entries",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1, 2],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-uppercase-alias",
        dimension: "state",
        path: "completion.items",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
    LedgeredDivergence {
        trace: "slash-uppercase-alias",
        dimension: "state",
        path: "completion.items[0].description.digest",
        events: &[2],
        reason: WHY_CLEAR_DESCRIPTION,
    },
    LedgeredDivergence {
        trace: "slash-uppercase-alias",
        dimension: "render",
        path: "popupRows",
        events: &[0, 1],
        reason: WHY_REGISTRY,
    },
];
const OBSERVABLE_EFFECTS: &[&str] = &[
    "submitRequested",
    "submit",
    "modeChanged",
    "historyPrevious",
    "historyNext",
    "historyReset",
    "completionReset",
    "clipboardImageRequested",
    "feedbackRating",
    "feedbackSnooze",
    "feedbackDismissed",
    "notify",
    "recordingStartRequested",
    "recordingStopRequested",
    "recordingCancelRequested",
];
const TRACE_KEYS: &[&str] = &[
    "id",
    "gap",
    "story",
    "title",
    "capabilities",
    "setup",
    "initial",
    "events",
    "observations",
    "schemaVersion",
    "reference",
];
const DIMENSIONS: &[&str] = &["state", "effects", "render", "history", "submission"];
const STATUSES: &[&str] = &["parity", "gap", "deferred", "unavailable"];

// ---------------------------------------------------------------------------
// Corpus model
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    reference: Reference,
    workspaces: BTreeMap<String, BTreeMap<String, String>>,
    gaps: Vec<Gap>,
    traces: Vec<ManifestTrace>,
    unavailable: Vec<UnavailableTrace>,
}

#[derive(Debug, Deserialize)]
struct Reference {
    commit: String,
    version: String,
}

#[derive(Debug, Deserialize)]
struct Gap {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ManifestTrace {
    id: String,
    gap: String,
    file: String,
}

#[derive(Debug, Deserialize)]
struct UnavailableTrace {
    id: String,
    gap: String,
    #[serde(rename = "missingCapabilities")]
    missing_capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Expectations {
    version: u32,
    traces: BTreeMap<String, BTreeMap<String, String>>,
}

fn parity_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("parity")
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let contents =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_str(&contents).map_err(|error| format!("{}: {error}", path.display()))
}

fn load_manifest() -> Result<Manifest, String> {
    let manifest: Manifest = read_json(&parity_directory().join("manifest.json"))?;
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "corpus schema version {} is not supported by this runner (expected {SCHEMA_VERSION})",
            manifest.schema_version
        ));
    }
    Ok(manifest)
}

fn load_expectations() -> Result<Expectations, String> {
    let expectations: Expectations = read_json(&parity_directory().join("expectations.json"))?;
    if expectations.version != SCHEMA_VERSION {
        return Err(format!(
            "expectations version {} is not supported (expected {SCHEMA_VERSION})",
            expectations.version
        ));
    }
    Ok(expectations)
}

/// Dimensions a trace actually records, across all of its observations.
fn recorded_dimensions(observations: &[Value]) -> BTreeSet<String> {
    let mut recorded = BTreeSet::new();
    for observation in observations {
        let Some(map) = observation.as_object() else {
            continue;
        };
        for dimension in DIMENSIONS {
            if map.contains_key(*dimension) {
                recorded.insert((*dimension).to_owned());
            }
        }
    }
    recorded
}

fn trace_observations(trace: &Map<String, Value>) -> Vec<Value> {
    trace
        .get("observations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Corpus integrity
// ---------------------------------------------------------------------------

#[test]
fn corpus_declares_every_gap_trace_and_expectation() -> Result<(), String> {
    let manifest = load_manifest()?;
    let expectations = load_expectations()?;
    let directory = parity_directory();

    if manifest.reference.commit.len() != 40 {
        return Err(format!(
            "reference commit `{}` is not a full git revision",
            manifest.reference.commit
        ));
    }
    // A corpus captured from another revision is not an oracle for this pin, and
    // nothing else here would notice: `scripts/parity/oracle.py` records whatever
    // HEAD it finds, so this corpus sat two releases behind the pin until US-142
    // recaptured it. `AGENTS.md` requires a re-pin to regenerate every committed
    // corpus in the same change, and this is what makes that mechanical.
    if manifest.reference.commit != vibe_core::parity::REFERENCE_COMMIT {
        return Err(format!(
            "the corpus was captured from `{}` and the pin names `{}`; recapture it with \
             `scripts/parity/oracle.py`",
            manifest.reference.commit,
            vibe_core::parity::REFERENCE_COMMIT
        ));
    }
    if manifest.reference.version.is_empty() {
        return Err("reference version is missing from the manifest".to_owned());
    }

    let declared = manifest
        .gaps
        .iter()
        .map(|gap| gap.id.clone())
        .collect::<BTreeSet<_>>();
    let covered = manifest
        .traces
        .iter()
        .map(|trace| trace.gap.clone())
        .chain(manifest.unavailable.iter().map(|trace| trace.gap.clone()))
        .collect::<BTreeSet<_>>();
    let uncovered = declared.difference(&covered).collect::<Vec<_>>();
    if !uncovered.is_empty() {
        return Err(format!("gaps without a canonical trace: {uncovered:?}"));
    }
    let unknown = covered.difference(&declared).collect::<Vec<_>>();
    if !unknown.is_empty() {
        return Err(format!("traces referencing unknown gaps: {unknown:?}"));
    }

    let mut expected_ids = expectations.traces.keys().cloned().collect::<BTreeSet<_>>();
    for trace in &manifest.traces {
        let Some(dimensions) = expectations.traces.get(&trace.id) else {
            return Err(format!(
                "trace `{}` has no entry in expectations.json; a trace is never assumed to pass",
                trace.id
            ));
        };
        expected_ids.remove(&trace.id);
        for (dimension, value) in dimensions {
            if !DIMENSIONS.contains(&dimension.as_str()) {
                return Err(format!(
                    "trace `{}` declares an unknown dimension `{dimension}`",
                    trace.id
                ));
            }
            if !STATUSES.contains(&value.as_str()) {
                return Err(format!(
                    "trace `{}` declares an unsupported expectation `{value}` for `{dimension}`",
                    trace.id
                ));
            }
        }

        // A dimension the oracle recorded must have a declared status. Without
        // this, a recorded observation could be dropped without anyone noticing.
        let raw: Value = read_json(&directory.join(&trace.file))?;
        let object = raw
            .as_object()
            .ok_or_else(|| format!("trace `{}` is not a JSON object", trace.id))?;
        for dimension in recorded_dimensions(&trace_observations(object)) {
            if !dimensions.contains_key(&dimension) {
                return Err(format!(
                    "trace `{}` records observations for `{dimension}` but expectations.json does \
                     not declare it; declare `parity`, `gap` or `deferred` instead of dropping it",
                    trace.id
                ));
            }
        }
    }
    for trace in &manifest.unavailable {
        let Some(dimensions) = expectations.traces.get(&trace.id) else {
            return Err(format!(
                "unavailable trace `{}` has no entry in expectations.json",
                trace.id
            ));
        };
        expected_ids.remove(&trace.id);
        if dimensions.get("state").map(String::as_str) != Some("unavailable") {
            return Err(format!(
                "trace `{}` is unavailable ({:?}) and must be declared as `unavailable`",
                trace.id, trace.missing_capabilities
            ));
        }
    }
    if !expected_ids.is_empty() {
        return Err(format!(
            "expectations.json declares traces missing from the manifest: {expected_ids:?}"
        ));
    }
    // The ledger and the declared gaps must name each other: a `gap` without a
    // ledgered pointer is a blanket, and an entry outside a `gap` is dead.
    let mut ledgered = BTreeSet::new();
    for entry in DIVERGENCES {
        if entry.reason.trim().is_empty()
            || entry.path.is_empty()
            || entry.path.contains('*')
            || entry.events.is_empty()
        {
            return Err(format!(
                "DIVERGENCES entry `{}` / `{}` / `{}` needs a concrete pointer, its events and a reason",
                entry.trace, entry.dimension, entry.path
            ));
        }
        for event in entry.events {
            if !ledgered.insert((entry.trace, entry.dimension, entry.path, *event)) {
                return Err(format!(
                    "DIVERGENCES ledgers `{}` / `{}` / `{}` at event {event} twice",
                    entry.trace, entry.dimension, entry.path
                ));
            }
        }
        let status = expectations
            .traces
            .get(entry.trace)
            .and_then(|dimensions| dimensions.get(entry.dimension));
        if status.map(String::as_str) != Some("gap") {
            return Err(format!(
                "DIVERGENCES ledgers `{}` / `{}`, which expectations.json declares {status:?} rather than `gap`",
                entry.trace, entry.dimension
            ));
        }
    }
    for (trace, dimensions) in &expectations.traces {
        for (dimension, status) in dimensions {
            if status == "gap"
                && !DIVERGENCES
                    .iter()
                    .any(|entry| entry.trace == trace && entry.dimension == dimension)
            {
                return Err(format!(
                    "trace `{trace}` declares `gap` on `{dimension}` without a DIVERGENCES entry"
                ));
            }
        }
    }
    if !manifest.workspaces.contains_key("empty") {
        return Err("the manifest must describe the `empty` workspace fixture".to_owned());
    }
    Ok(())
}

/// Every deferred dimension names the story that will start comparing it.
#[test]
fn deferred_dimensions_name_their_story() -> Result<(), String> {
    let expectations = load_expectations()?;
    let deferrable = DEFERRED_DIMENSIONS
        .iter()
        .map(|(dimension, _)| *dimension)
        .collect::<BTreeSet<_>>();
    for (id, dimensions) in &expectations.traces {
        for (dimension, status) in dimensions {
            if status == "deferred" && !deferrable.contains(dimension.as_str()) {
                return Err(format!(
                    "trace `{id}` defers `{dimension}`, which is not a known deferred dimension; \
                     add it to DEFERRED_DIMENSIONS with the story that closes it"
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

struct Divergence {
    dimension: &'static str,
    event_index: Option<usize>,
    path: String,
    expected: Value,
    actual: Value,
}

impl Divergence {
    /// The differing field without its leading separator, `$` for the root.
    fn pointer(&self) -> &str {
        match self.path.strip_prefix('.').unwrap_or(&self.path) {
            "" => "$",
            pointer => pointer,
        }
    }

    fn describe(&self, trace: &str, commit: &str) -> String {
        let position = match self.event_index {
            Some(index) => format!("event {index}"),
            None => "initial state".to_owned(),
        };
        format!(
            "trace `{trace}` diverges at {position} on {}{}\n  expected: {}\n  actual:   {}\n  fixture revision: {commit}",
            self.dimension, self.path, self.expected, self.actual
        )
    }
}

struct Replay {
    state: ChatInputState,
    workspace: PathBuf,
    voice_start_error: Option<String>,
}

struct StepObservation {
    effects: Vec<Value>,
    submission: Option<Value>,
}

impl Replay {
    fn new(workspace: PathBuf, setup: &Value) -> Result<Self, String> {
        let skills = setup
            .get("skills")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|entry| {
                        Some((
                            entry.get("command")?.as_str()?,
                            entry.get("description")?.as_str()?,
                        ))
                    })
                    .map(|(command, description)| (command.to_owned(), description.to_owned()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut state = ChatInputState::new();
        if let Some(width) = setup.pointer("/viewport/width").and_then(Value::as_u64)
            && let Ok(width) = u16::try_from(width)
        {
            state.set_viewport_width(width);
        }
        let vibe_code_enabled = setup
            .pointer("/commands/vibeCodeEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let excluded = setup
            .pointer("/commands/excluded")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        state.set_command_context(
            CommandContext::new(vibe_code_enabled).with_excluded(excluded.iter().copied()),
        );
        state.set_user_skills(
            skills
                .iter()
                .map(|(command, description)| (command.as_str(), description.as_str())),
        );
        state.set_agent_name(
            setup
                .get("agentName")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        state.set_safety(
            setup
                .get("safety")
                .cloned()
                .map(serde_json::from_value::<Safety>)
                .transpose()
                .map_err(|error| format!("safety setup is invalid: {error}"))?
                .unwrap_or(Safety::Neutral),
        );
        state.set_voice_enabled(
            setup
                .pointer("/voice/enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
        if let Some(active) = setup
            .pointer("/initial/feedbackActive")
            .and_then(Value::as_bool)
        {
            state.apply(InputEvent::Feedback { active });
        }
        if let Some(active) = setup.pointer("/initial/switching").and_then(Value::as_bool) {
            state.apply(InputEvent::Switching { active });
        }
        let history = setup_history(setup);
        if !history.is_empty() {
            let contents = history
                .iter()
                .filter_map(|entry| serde_json::to_string(entry).ok())
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(workspace.join("vibehistory"), contents)
                .map_err(|error| format!("history workspace fixture: {error}"))?;
        }
        state.replace_history(history);
        let voice_start_error = setup
            .pointer("/voice/startError")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(Self {
            state,
            workspace,
            voice_start_error,
        })
    }

    /// Applies one trace event and settles every effect it triggers.
    fn step(&mut self, event: InputEvent) -> Result<StepObservation, String> {
        let mut observable = Vec::new();
        let mut submitted = None;
        let mut pending = self.state.apply(event);
        let mut rounds = 0usize;
        while !pending.is_empty() {
            rounds = rounds.saturating_add(1);
            if rounds > MAX_EFFECT_ROUNDS {
                return Err(format!(
                    "effects did not settle after {MAX_EFFECT_ROUNDS} rounds; {} remain",
                    pending.len()
                ));
            }
            let mut follow_up = Vec::new();
            for effect in std::mem::take(&mut pending) {
                if let InputEffect::Submit { text } = &effect {
                    submitted = Some(text.clone());
                }
                let encoded = serde_json::to_value(&effect)
                    .map_err(|error| format!("effect does not serialize: {error}"))?;
                if encoded
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| OBSERVABLE_EFFECTS.contains(&kind))
                {
                    observable.push(encoded);
                }
                match effect {
                    InputEffect::RequestCompletion { request } => {
                        follow_up.push(self.resolve_completion(request));
                    }
                    InputEffect::NormalizePastedPath { text, snapshot } => {
                        let normalized = normalize_pasted_text(&text);
                        if normalized != text {
                            follow_up.push(InputEvent::PasteNormalized {
                                snapshot,
                                text: normalized,
                            });
                        }
                    }
                    InputEffect::NormalizeCurrentText { snapshot } => {
                        let normalized = normalize_pasted_text(&snapshot.text);
                        if normalized != snapshot.text {
                            follow_up.push(InputEvent::TextNormalized {
                                snapshot,
                                text: normalized,
                            });
                        }
                    }
                    InputEffect::RecordingStartRequested => {
                        follow_up.push(InputEvent::VoiceStartResolved {
                            generation: self.state.voice_generation(),
                            error: self.voice_start_error.clone(),
                        });
                    }
                    _ => {}
                }
            }
            for event in follow_up {
                pending.extend(self.state.apply(event));
            }
        }
        let submission = submitted
            .map(|text| {
                prepare_submission(
                    &self.workspace,
                    &PromptDraft::text_only(text),
                    "oracle-model",
                    true,
                )
                .map_err(|error| format!("submission preparation failed: {error}"))
                .and_then(|prepared| {
                    serde_json::to_value(prepared.turn)
                        .map_err(|error| format!("submission does not serialize: {error}"))
                })
            })
            .transpose()?;
        Ok(StepObservation {
            effects: observable,
            submission,
        })
    }

    /// Resolves the boundary request through the same engine as the live TUI.
    fn resolve_completion(&self, request: CompletionRequest) -> InputEvent {
        InputEvent::CompletionResolved {
            resolution: self
                .state
                .completion()
                .resolve_request(request, &self.workspace),
        }
    }
}

fn setup_history(setup: &Value) -> Vec<String> {
    if let Some(entries) = setup.pointer("/history/entries").and_then(Value::as_array) {
        return entries
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
    }
    setup
        .pointer("/history/raw")
        .and_then(Value::as_str)
        .map(|raw| {
            raw.lines()
                .filter(|line| !line.is_empty())
                .map(|line| {
                    serde_json::from_str::<String>(line).unwrap_or_else(|_| line.to_owned())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn decode_event(raw: &Value) -> Result<InputEvent, String> {
    let kind = raw
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("event without a type: {raw}"))?;
    match kind {
        "key" | "paste" | "resize" | "mouse" | "transcript" | "switching" | "feedback" => {
            serde_json::from_value(rename_event(raw, kind)).map_err(|error| {
                format!("event `{kind}` cannot be decoded by this runner: {error} ({raw})")
            })
        }
        "safety" => Ok(InputEvent::SafetyChanged {
            value: serde_json::from_value(
                raw.get("value")
                    .cloned()
                    .ok_or_else(|| "safety event without a value".to_owned())?,
            )
            .map_err(|error| format!("safety value is invalid: {error}"))?,
        }),
        "externalEditor" => Ok(InputEvent::ExternalEditor {
            text: raw.get("text").and_then(Value::as_str).map(str::to_owned),
        }),
        other => Err(format!(
            "trace uses event `{other}` which this runner cannot replay; the trace fails instead of being skipped"
        )),
    }
}

fn rename_event(raw: &Value, kind: &str) -> Value {
    let mut object = raw.as_object().cloned().unwrap_or_default();
    if kind == "key" {
        // `char` is absent for named keys and null-safe for the boundary.
        if object.get("char").is_some_and(Value::is_null) {
            object.remove("char");
        }
    }
    Value::Object(object)
}

/// Reduces every completion description to the `{length, digest}` fingerprint
/// the oracle records (`digest` in `scripts/parity/harness.py`): the length in
/// Unicode scalar values, as Python's `len`, and the SHA-256 of the UTF-8 bytes.
fn digest_descriptions(state: &mut Value) {
    let Some(items) = state
        .pointer_mut("/completion/items")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for item in items {
        let Some(description) = item.get_mut("description") else {
            continue;
        };
        let Some(text) = description.as_str() else {
            continue;
        };
        let digest = Sha256::digest(text.as_bytes())
            .iter()
            .fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            });
        *description = json!({"length": text.chars().count(), "digest": digest});
    }
}

/// Drops the state fields Rust does not model yet from an expected observation.
fn strip_unmodeled_state(state: &mut Value) {
    for (path, _story) in UNMODELED_STATE_PATHS {
        let mut cursor = &mut *state;
        let mut segments = path.split('.').peekable();
        while let Some(segment) = segments.next() {
            let Some(map) = cursor.as_object_mut() else {
                break;
            };
            if segments.peek().is_none() {
                map.remove(segment);
                break;
            }
            let Some(next) = map.get_mut(segment) else {
                break;
            };
            cursor = next;
        }
    }
}

/// Whether a trace's render is compared on the composer projection alone.
///
/// The external-editor and prompt-history traces drive the composer through a
/// round trip that leaves the rest of the frame free to differ, so only the
/// four composer fields are held to the reference for them.
fn compares_composer_render_only(trace: &Map<String, Value>) -> bool {
    matches!(
        trace.get("story").and_then(Value::as_str),
        Some("US-004" | "US-005" | "US-006" | "US-007")
    )
}

fn composer_render_projection(render: &Value) -> Value {
    let mut projection = Map::new();
    for field in ["cursorCell", "prompt", "visualLines", "wrapWidth"] {
        if let Some(value) = render.get(field) {
            projection.insert(field.to_owned(), value.clone());
        }
    }
    Value::Object(projection)
}

fn normalize_workspace_render(render: &mut Value, workspace: &Path) {
    let workspace = workspace.to_string_lossy();
    let Some(render) = render.as_object_mut() else {
        return;
    };
    let Some(lines) = render.get("visualLines").and_then(Value::as_array) else {
        return;
    };
    let text = lines.iter().filter_map(Value::as_str).collect::<String>();
    if !text.contains(workspace.as_ref()) {
        return;
    }
    // Fixture paths are replaced after recording, so their host-dependent
    // length can change wrapping and cursor columns. Keep content and chrome
    // assertions exact while excluding only those derived path coordinates.
    render.insert("visualLines".to_owned(), json!([text]));
    render.remove("cursorCell");
}

/// Rewrites the recorded workspace placeholder to this run's temporary path.
///
/// Applied to events and to expected observations alike: both sides must name
/// the same directory or a path-shaped assertion can never be satisfied.
fn substitute_workspace(value: &mut Value, workspace: &str) {
    match value {
        Value::String(text) => {
            if text.contains(WORKSPACE_PLACEHOLDER) {
                *text = text.replace(WORKSPACE_PLACEHOLDER, workspace);
            }
        }
        Value::Array(items) => {
            for item in items {
                substitute_workspace(item, workspace);
            }
        }
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str).map(str::to_owned) {
                if let Some(cursor) = map.get_mut("cursor")
                    && let Some(offset) = cursor.as_u64()
                {
                    *cursor = json!(workspace_adjusted_offset(&text, offset, workspace));
                }
                if let Some(selection) = map.get_mut("selection").and_then(Value::as_array_mut) {
                    for offset in selection {
                        if let Some(value) = offset.as_u64() {
                            *offset = json!(workspace_adjusted_offset(&text, value, workspace));
                        }
                    }
                }
            }
            for item in map.values_mut() {
                substitute_workspace(item, workspace);
            }
        }
        _ => {}
    }
}

fn workspace_adjusted_offset(text: &str, offset: u64, workspace: &str) -> u64 {
    let offset = usize::try_from(offset).unwrap_or(usize::MAX);
    if offset >= text.chars().count() {
        return u64::try_from(
            text.replace(WORKSPACE_PLACEHOLDER, workspace)
                .chars()
                .count(),
        )
        .unwrap_or(u64::MAX);
    }
    let prefix = text.chars().take(offset).collect::<String>();
    let occurrences = prefix.matches(WORKSPACE_PLACEHOLDER).count();
    let placeholder_chars = WORKSPACE_PLACEHOLDER.chars().count();
    let workspace_chars = workspace.chars().count();
    if workspace_chars >= placeholder_chars {
        u64::try_from(
            offset.saturating_add(occurrences.saturating_mul(workspace_chars - placeholder_chars)),
        )
        .unwrap_or(u64::MAX)
    } else {
        u64::try_from(
            offset.saturating_sub(occurrences.saturating_mul(placeholder_chars - workspace_chars)),
        )
        .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

fn first_difference(
    expected: &Value,
    actual: &Value,
    path: &str,
) -> Option<(String, Value, Value)> {
    match (expected, actual) {
        (Value::Object(expected_map), Value::Object(actual_map)) => {
            let keys = expected_map
                .keys()
                .chain(actual_map.keys())
                .collect::<BTreeSet<_>>();
            for key in keys {
                let expected_value = expected_map.get(key).unwrap_or(&Value::Null);
                let actual_value = actual_map.get(key).unwrap_or(&Value::Null);
                if let Some(difference) =
                    first_difference(expected_value, actual_value, &format!("{path}.{key}"))
                {
                    return Some(difference);
                }
            }
            None
        }
        (Value::Array(expected_items), Value::Array(actual_items)) => {
            if expected_items.len() != actual_items.len() {
                return Some((path.to_owned(), expected.clone(), actual.clone()));
            }
            for (index, (expected_item, actual_item)) in
                expected_items.iter().zip(actual_items).enumerate()
            {
                if let Some(difference) =
                    first_difference(expected_item, actual_item, &format!("{path}[{index}]"))
                {
                    return Some(difference);
                }
            }
            None
        }
        _ if expected == actual => None,
        _ => Some((path.to_owned(), expected.clone(), actual.clone())),
    }
}

fn compare(
    dimension: &'static str,
    event_index: Option<usize>,
    expected: &Value,
    actual: &Value,
) -> Option<Divergence> {
    first_difference(expected, actual, "").map(|(path, expected, actual)| Divergence {
        dimension,
        event_index,
        path,
        expected,
        actual,
    })
}

/// Reports every divergent event and pointer of a `gap` dimension that
/// `DIVERGENCES` does not ledger, and every ledgered one that no longer
/// diverges.
fn unledgered_divergences(
    trace: &str,
    dimension: &str,
    divergences: &[Divergence],
    commit: &str,
) -> Vec<String> {
    let ledgered = DIVERGENCES
        .iter()
        .filter(|entry| entry.trace == trace && entry.dimension == dimension)
        .flat_map(|entry| entry.events.iter().map(|event| (*event, entry.path)))
        .collect::<BTreeSet<_>>();
    let observed = divergences
        .iter()
        .filter_map(|divergence| Some((divergence.event_index?, divergence.pointer())))
        .collect::<BTreeSet<_>>();
    let mut failures = Vec::new();
    for divergence in divergences {
        let pointer = divergence.pointer();
        let ledgered_here = divergence
            .event_index
            .is_some_and(|event| ledgered.contains(&(event, pointer)));
        if !ledgered_here {
            failures.push(format!(
                "{}\n  not ledgered: add event {:?} to `{trace}` / `{dimension}` / `{pointer}` in DIVERGENCES",
                divergence.describe(trace, commit),
                divergence.event_index
            ));
        }
    }
    for (event, path) in ledgered.difference(&observed) {
        failures.push(format!(
            "trace `{trace}` no longer diverges on {dimension} at `{path}` for event {event}; remove it from DIVERGENCES"
        ));
    }
    failures
}

fn materialize_workspace(
    root: &Path,
    files: &BTreeMap<String, String>,
) -> Result<(), std::io::Error> {
    for (relative, contents) in files {
        let target = root.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, contents)?;
    }
    Ok(())
}

fn validate_trace_schema(trace: &Map<String, Value>, id: &str) -> Result<(), String> {
    for key in trace.keys() {
        if !TRACE_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "trace `{id}` carries unknown field `{key}`; the schema must be updated before it can pass"
            ));
        }
    }
    for key in ["setup", "initial", "events", "observations"] {
        if !trace.contains_key(key) {
            return Err(format!("trace `{id}` is missing required field `{key}`"));
        }
    }
    let version = trace
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if version != u64::from(SCHEMA_VERSION) {
        return Err(format!(
            "trace `{id}` uses schema version {version}; this runner replays version {SCHEMA_VERSION}"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The differential test
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::unwrap_in_result)]
fn canonical_traces_replay_with_their_declared_parity() -> Result<(), String> {
    let manifest = load_manifest()?;
    let expectations = load_expectations()?;
    let directory = parity_directory();
    let temporary = tempfile::tempdir().map_err(|error| format!("workspace root: {error}"))?;

    let mut failures = Vec::new();
    let mut matched = 0usize;
    let mut diverged = 0usize;
    let mut deferred = 0usize;
    // Calibration mode: rewrite the declared expectations from what this run
    // observed. It is opt-in so a normal run can never silence a regression.
    let calibrating = std::env::var_os("VIBE_PARITY_CALIBRATE").is_some();
    let mut calibrated: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for trace in &manifest.unavailable {
        calibrated.insert(
            trace.id.clone(),
            BTreeMap::from([("state".to_owned(), "unavailable".to_owned())]),
        );
    }

    for entry in &manifest.traces {
        let mut raw: Value = read_json(&directory.join(&entry.file))?;
        let workspace = temporary.path().join(&entry.id);
        substitute_workspace(&mut raw, &workspace.to_string_lossy());
        let object = raw
            .as_object()
            .ok_or_else(|| format!("trace `{}` is not a JSON object", entry.id))?;
        validate_trace_schema(object, &entry.id)?;

        let setup = object
            .get("setup")
            .ok_or_else(|| format!("trace `{}` has no setup", entry.id))?;
        let workspace_name = setup
            .get("workspace")
            .and_then(Value::as_str)
            .unwrap_or("empty");
        let files = manifest.workspaces.get(workspace_name).ok_or_else(|| {
            format!(
                "trace `{}` needs workspace fixture `{workspace_name}` which the manifest does not describe",
                entry.id
            )
        })?;
        std::fs::create_dir_all(&workspace).map_err(|error| format!("workspace: {error}"))?;
        materialize_workspace(&workspace, files)
            .map_err(|error| format!("workspace fixture `{workspace_name}`: {error}"))?;

        let declared = expectations
            .traces
            .get(&entry.id)
            .ok_or_else(|| format!("trace `{}` has no expectation entry", entry.id))?;

        let mut replay = Replay::new(workspace, setup)?;
        // Every event is compared on every dimension, so a `gap` dimension is
        // held to the exact set of pointers `DIVERGENCES` ledgers for it rather
        // than to its first divergence alone.
        let mut observed: BTreeMap<&'static str, Vec<Divergence>> = BTreeMap::new();

        let events = object
            .get("events")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("trace `{}` has no events", entry.id))?;
        let observations = object
            .get("observations")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("trace `{}` has no observations", entry.id))?;
        if events.len() != observations.len() {
            return Err(format!(
                "trace `{}` records {} events for {} observations",
                entry.id,
                events.len(),
                observations.len()
            ));
        }

        for (index, (raw_event, observation)) in events.iter().zip(observations).enumerate() {
            let event = decode_event(raw_event)?;
            let step = replay.step(event)?;
            let mut state = serde_json::to_value(replay.state.observe())
                .map_err(|error| format!("state does not serialize: {error}"))?;
            digest_descriptions(&mut state);

            let mut expected_state = observation.get("state").cloned().unwrap_or(Value::Null);
            strip_unmodeled_state(&mut expected_state);
            if let Some(divergence) = compare("state", Some(index), &expected_state, &state) {
                observed.entry("state").or_default().push(divergence);
            }

            let expected_effects = observation
                .get("effects")
                .cloned()
                .unwrap_or_else(|| json!([]));
            if let Some(divergence) = compare(
                "effects",
                Some(index),
                &expected_effects,
                &Value::Array(step.effects),
            ) {
                observed.entry("effects").or_default().push(divergence);
            }

            if let Some(expected_history) = observation.get("history") {
                let recorded = observed.entry("history").or_default();
                if let Some(divergence) = compare(
                    "history",
                    Some(index),
                    expected_history,
                    &json!(replay.state.history_entries()),
                ) {
                    recorded.push(divergence);
                }
            }

            if let Some(expected_submission) = observation.get("submission") {
                let recorded = observed.entry("submission").or_default();
                if let Some(divergence) = compare(
                    "submission",
                    Some(index),
                    expected_submission,
                    step.submission.as_ref().unwrap_or(&Value::Null),
                ) {
                    recorded.push(divergence);
                }
            }

            if let Some(expected_render) = observation.get("render") {
                let mut actual_render = serde_json::to_value(replay.state.observe_render())
                    .map_err(|error| format!("render observation does not serialize: {error}"))?;
                let mut expected_render = if compares_composer_render_only(object) {
                    actual_render = composer_render_projection(&actual_render);
                    composer_render_projection(expected_render)
                } else {
                    expected_render.clone()
                };
                normalize_workspace_render(&mut expected_render, &replay.workspace);
                normalize_workspace_render(&mut actual_render, &replay.workspace);
                let recorded = observed.entry("render").or_default();
                if let Some(divergence) =
                    compare("render", Some(index), &expected_render, &actual_render)
                {
                    recorded.push(divergence);
                }
            }
        }

        if calibrating {
            let mut dimensions = BTreeMap::new();
            for dimension in DIMENSIONS {
                // A deferred dimension keeps its status: this run produced no
                // observation for it, so it has nothing to calibrate against.
                if let Some(status) = declared.get(*dimension)
                    && status == "deferred"
                    && !observed.contains_key(dimension)
                {
                    dimensions.insert((*dimension).to_owned(), status.clone());
                    continue;
                }
                if !declared.contains_key(*dimension) {
                    continue;
                }
                let status = if observed
                    .get(dimension)
                    .is_some_and(|found| !found.is_empty())
                {
                    "gap"
                } else {
                    "parity"
                };
                dimensions.insert((*dimension).to_owned(), status.to_owned());
            }
            calibrated.insert(entry.id.clone(), dimensions);
            continue;
        }

        for dimension in DIMENSIONS {
            let Some(expectation) = declared.get(*dimension) else {
                continue;
            };
            let divergences = observed
                .get(dimension)
                .map(Vec::as_slice)
                .unwrap_or_default();
            match (expectation.as_str(), divergences.first()) {
                ("deferred", _) => deferred = deferred.saturating_add(1),
                ("parity", Some(divergence)) => {
                    diverged = diverged.saturating_add(1);
                    failures.push(divergence.describe(&entry.id, &manifest.reference.commit));
                }
                ("parity", None) => matched = matched.saturating_add(1),
                ("gap", Some(_)) => {
                    diverged = diverged.saturating_add(1);
                    failures.extend(unledgered_divergences(
                        &entry.id,
                        dimension,
                        divergences,
                        &manifest.reference.commit,
                    ));
                }
                ("gap", None) => {
                    matched = matched.saturating_add(1);
                    failures.push(format!(
                        "trace `{}` ({}) now matches the reference on {dimension}; update expectations.json to `parity`",
                        entry.id, entry.gap
                    ));
                }
                (other, _) => {
                    return Err(format!(
                        "trace `{}` declares expectation `{other}` for `{dimension}`",
                        entry.id
                    ));
                }
            }
        }
    }

    if calibrating {
        let document = json!({"version": SCHEMA_VERSION, "traces": calibrated});
        let encoded = serde_json::to_string_pretty(&document)
            .map_err(|error| format!("expectations do not serialize: {error}"))?;
        std::fs::write(directory.join("expectations.json"), format!("{encoded}\n"))
            .map_err(|error| format!("expectations cannot be written: {error}"))?;
        return Err(
            "expectations.json was rewritten from this run; review the diff and rerun without VIBE_PARITY_CALIBRATE"
                .to_owned(),
        );
    }

    if failures.is_empty() {
        return Ok(());
    }
    let mut report = format!(
        "{matched} parity assertions matched, {diverged} diverged, {deferred} deferred, {} unresolved\n",
        failures.len()
    );
    for failure in &failures {
        let _ = writeln!(report, "\n{failure}");
    }
    Err(report)
}

// ---------------------------------------------------------------------------
// Explicit failure modes
// ---------------------------------------------------------------------------

#[test]
fn unknown_trace_fields_fail_instead_of_being_ignored() {
    let mut trace = Map::new();
    trace.insert("setup".to_owned(), json!({}));
    trace.insert("initial".to_owned(), json!({}));
    trace.insert("events".to_owned(), json!([]));
    trace.insert("observations".to_owned(), json!([]));
    trace.insert("schemaVersion".to_owned(), json!(SCHEMA_VERSION));
    trace.insert("futureField".to_owned(), json!(true));
    let error = validate_trace_schema(&trace, "sample").expect_err("unknown fields must fail");
    assert!(error.contains("futureField"), "{error}");
}

#[test]
fn schema_version_mismatch_fails_the_trace() {
    let mut trace = Map::new();
    trace.insert("setup".to_owned(), json!({}));
    trace.insert("initial".to_owned(), json!({}));
    trace.insert("events".to_owned(), json!([]));
    trace.insert("observations".to_owned(), json!([]));
    trace.insert("schemaVersion".to_owned(), json!(SCHEMA_VERSION + 1));
    let error = validate_trace_schema(&trace, "sample").expect_err("schema drift must fail");
    assert!(error.contains("schema version"), "{error}");
}

#[test]
fn unsupported_events_fail_instead_of_being_skipped() {
    let error = decode_event(&json!({"type": "gesture", "x": 1, "y": 2}))
        .expect_err("an unreplayable event must fail");
    assert!(error.contains("cannot replay"), "{error}");
    let error = decode_event(&json!({"type": "key", "key": "f13", "mods": []}))
        .expect_err("an unknown key must fail");
    assert!(error.contains("cannot be decoded"), "{error}");
}

#[test]
fn differences_report_the_first_divergent_field() {
    let expected = json!({"completion": {"items": [{"label": "/config"}]}});
    let actual = json!({"completion": {"items": [{"label": "/clear"}]}});
    let divergence = compare("state", Some(3), &expected, &actual).expect("a divergence");
    let report = divergence.describe("sample", "abc123");
    assert!(report.contains("event 3"), "{report}");
    assert!(
        report.contains("state.completion.items[0].label"),
        "{report}"
    );
    assert!(report.contains("/config"), "{report}");
    assert!(report.contains("/clear"), "{report}");
    assert!(report.contains("abc123"), "{report}");
}

#[test]
fn the_workspace_placeholder_is_substituted_on_both_sides() {
    let mut trace = json!({
        "events": [{"type": "paste", "text": "__WORKSPACE__/image one.png"}],
        "observations": [{"state": {"text": "@'__WORKSPACE__/image one.png'"}}],
    });
    substitute_workspace(&mut trace, "/tmp/ws");
    let encoded = trace.to_string();
    assert!(!encoded.contains(WORKSPACE_PLACEHOLDER), "{encoded}");
    assert!(encoded.contains("/tmp/ws/image one.png"), "{encoded}");
}

#[test]
fn modeled_state_fields_are_not_dropped() {
    let mut state = json!({
        "text": "draft",
        "history": {"navigating": true, "loadedEntry": true, "cursorMovedSinceLoad": false},
    });
    strip_unmodeled_state(&mut state);
    assert_eq!(
        state,
        json!({
            "text": "draft",
            "history": {
                "navigating": true,
                "loadedEntry": true,
                "cursorMovedSinceLoad": false
            }
        })
    );
}

/// The composer must answer for every state field the runner still compares.
#[test]
fn every_compared_state_field_is_modeled() {
    let observation =
        serde_json::to_value(ChatInputState::new().observe()).expect("the observation serializes");
    for (path, story) in UNMODELED_STATE_PATHS {
        let mut cursor = &observation;
        for segment in path.split('.') {
            let Some(next) = cursor.get(segment) else {
                cursor = &Value::Null;
                break;
            };
            cursor = next;
        }
        assert!(
            cursor.is_null(),
            "`{path}` is declared unmodeled ({story}) but the composer now reports it; \
             remove it from UNMODELED_STATE_PATHS so it becomes a real assertion"
        );
    }
}

#[test]
fn gap_divergences_are_held_to_their_ledgered_events_and_pointers() {
    let divergence = compare(
        "state",
        Some(2),
        &json!({"cursor": 1}),
        &json!({"cursor": 2}),
    )
    .expect("a divergence");
    let unledgered = unledgered_divergences("unledgered-trace", "state", &[divergence], "abc123");
    assert_eq!(unledgered.len(), 1, "{unledgered:?}");
    assert!(unledgered[0].contains("not ledgered"), "{unledgered:?}");
    assert!(unledgered[0].contains("`cursor`"), "{unledgered:?}");

    let stale = unledgered_divergences("history-down-restores-draft", "state", &[], "abc123");
    assert_eq!(stale.len(), 1, "{stale:?}");
    assert!(stale[0].contains("no longer diverges"), "{stale:?}");

    let root = compare("effects", Some(0), &json!([{"type": "submit"}]), &json!([]))
        .expect("a divergence");
    assert_eq!(root.pointer(), "$");
}

#[test]
fn completion_descriptions_are_compared_by_their_fingerprint() {
    let mut state = json!({"completion": {"items": [
        {"label": "/resume", "description": "Résumé"},
        {"label": "@notes.txt", "description": ""},
    ]}});
    digest_descriptions(&mut state);
    assert_eq!(
        state,
        json!({"completion": {"items": [
            {"label": "/resume", "description": {
                "length": 6,
                "digest": "9d750fae748681ca14e2c3fe0c1ed0a2b56984cf12747e281cf815833de21c9c",
            }},
            {"label": "@notes.txt", "description": {
                "length": 0,
                "digest": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            }},
        ]}})
    );
}

#[test]
fn a_recorded_dimension_is_detected_for_declaration() {
    let observations = vec![
        json!({"state": {}, "effects": []}),
        json!({"state": {}, "effects": [], "render": {}}),
    ];
    let recorded = recorded_dimensions(&observations);
    assert!(recorded.contains("render"), "{recorded:?}");
    assert!(!recorded.contains("history"), "{recorded:?}");
}
