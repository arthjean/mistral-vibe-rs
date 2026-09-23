//! Differential oracle for the command registry.
//!
//! `scripts/parity/commands.py` drives the pinned reference's own
//! `CommandRegistry` over inputs the script authors, and records eight families
//! into `crates/vibe-cli/tests/commands/corpus.json`. This module replays that
//! corpus against this build unconditionally: only the recapture probe at the
//! bottom skips when the checkout is absent or off-pin.
//!
//! Row 2 of `docs/parity.md` used to be measured by diffing two lists of names,
//! which cannot see what an alias resolves to, which commands a context leaves
//! standing, or what `/help` prints. Those are what the corpus records and what
//! this module compares.
//!
//! The reference's help lines are authored prose `NOTICE` forbids reproducing,
//! so the corpus measures each one as a byte length and a SHA-256 rather than
//! carrying it. [`this_ports_help_prose_never_matches_a_reference_digest`] holds
//! this port's own lines permanently unequal to every one of those digests, and
//! [`the_corpus_carries_no_reference_help_line_in_cleartext`] fails if a
//! reference line is ever pasted back into the corpus it was reduced from.
//!
//! A key is `family/field/case`, and a trailing `*` covers every key that starts
//! with the prefix. A divergence no entry names fails the replay; an entry whose
//! divergence stopped reproducing fails as stale, which is what forces a row out
//! once the behavior conforms.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use vibe_core::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};

use super::commands::{COMMANDS, CommandContext, command_available_in, parse_command_in};
use super::help;

const CORPUS_RELATIVE: &str = "crates/vibe-cli/tests/commands/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/commands.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the capture
/// script.
const CORPUS_SCHEMA_VERSION: u32 = 2;
/// The comparison floor this replay commits to, read off the first real capture
/// rather than estimated, so a regeneration that captured almost nothing fails
/// instead of reporting a clean but empty run.
const MINIMUM_COMPARISONS: usize = 445;

/// Keys the corpus carries that are not families: the pin, the layout and the
/// prose-free note.
const METADATA: [&str; 3] = ["schemaVersion", "reference", "note"];

/// Every family the corpus declares, with the case fields that are *inputs* the
/// capture authored and the case fields that are *answers* both sides give.
const FAMILIES: &[Family] = &[
    Family {
        name: "counts",
        inputs: &[],
        answers: &["count"],
    },
    Family {
        name: "inventory",
        inputs: &[],
        answers: &["aliases"],
    },
    Family {
        name: "availability",
        inputs: &[
            "registrySkillsEnabled",
            "experimentalHarness",
            "clipboardSupported",
            "excluded",
        ],
        answers: &["keys", "count"],
    },
    Family {
        name: "parse",
        inputs: &["context", "input"],
        answers: &["key", "alias", "arguments"],
    },
    Family {
        name: "helpDocument",
        inputs: &[],
        answers: &["count"],
    },
    Family {
        name: "helpSections",
        inputs: &[],
        answers: &["index", "headingLine", "level", "lineCount"],
    },
    Family {
        name: "helpCommands",
        inputs: &[],
        answers: &["index", "line", "aliases"],
    },
    Family {
        name: "helpProse",
        inputs: &[],
        answers: &["length", "digest"],
    },
];

/// Cases where this build answers something other than the reference, each with
/// the reason and the story that closes it.
///
/// `ACCEPTED` entries are permanent. `NOTICE` forbids reproducing the
/// reference's authored lines, so the three headings, the eight shortcut lines
/// and the two prefix lines are this port's own prose and differ from the
/// reference's in both byte length and digest. The command lines are not prose
/// and carry no `ACCEPTED` entry.
///
/// `OPEN` entries record what the re-pin to 2.25.7 measured and this port has
/// not followed: six registry keys added upstream since 2.24.0, the
/// `vibe_code_enabled` gate the reference dropped from `/teleport` and
/// `/remote-project`, and the rewritten `clear` description. Every other
/// command line this port renders still hashes to a reference digest; the
/// `helpCommands` and `helpProse` entries for them record only the offset the
/// missing keys displace them by.
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "helpProse/length/line-00",
        "ACCEPTED: the first heading is authored prose, so this port writes its own",
    ),
    (
        "helpProse/digest/line-00",
        "ACCEPTED: the first heading is authored prose, so this port writes its own",
    ),
    (
        "helpProse/length/line-02",
        "ACCEPTED: the send-prompt shortcut line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/digest/line-02",
        "ACCEPTED: the send-prompt shortcut line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/length/line-03",
        "ACCEPTED: the new-line shortcut line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/digest/line-03",
        "ACCEPTED: the new-line shortcut line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/length/line-04",
        "ACCEPTED: the stop-agent shortcut line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/digest/line-04",
        "ACCEPTED: the stop-agent shortcut line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/length/line-05",
        "ACCEPTED: the quit shortcut line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/digest/line-05",
        "ACCEPTED: the quit shortcut line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/length/line-06",
        "ACCEPTED: the external-editor line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/digest/line-06",
        "ACCEPTED: the external-editor line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/length/line-07",
        "ACCEPTED: the fold-tools line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/digest/line-07",
        "ACCEPTED: the fold-tools line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/length/line-08",
        "ACCEPTED: the switch-agent shortcut line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/digest/line-08",
        "ACCEPTED: the switch-agent shortcut line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/length/line-09",
        "ACCEPTED: the rewind shortcut line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/digest/line-09",
        "ACCEPTED: the rewind shortcut line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/length/line-11",
        "ACCEPTED: the second heading is authored prose, so this port writes its own",
    ),
    (
        "helpProse/digest/line-11",
        "ACCEPTED: the second heading is authored prose, so this port writes its own",
    ),
    (
        "helpProse/length/line-13",
        "ACCEPTED: the shell-prefix line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/digest/line-13",
        "ACCEPTED: the shell-prefix line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/length/line-14",
        "ACCEPTED: the path-completion line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/digest/line-14",
        "ACCEPTED: the path-completion line is authored prose, so this port writes its own",
    ),
    (
        "helpProse/length/line-16",
        "ACCEPTED: the third heading is authored prose, so this port writes its own",
    ),
    (
        "helpProse/digest/line-16",
        "ACCEPTED: the third heading is authored prose, so this port writes its own",
    ),
    (
        "counts/count/keys",
        "OPEN: the reference counts the six keys it added since v2.24.0 (`branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo`), each with one slash alias (vibe/cli/commands.py:41-250 @4a960031); this port's table lacks them (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "counts/count/aliases",
        "OPEN: the reference counts the six keys it added since v2.24.0 (`branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo`), each with one slash alias (vibe/cli/commands.py:41-250 @4a960031); this port's table lacks them (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "counts/count/slashAliases",
        "OPEN: the reference counts the six keys it added since v2.24.0 (`branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo`), each with one slash alias (vibe/cli/commands.py:41-250 @4a960031); this port's table lacks them (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "inventory/aliases/branch",
        "OPEN: reference v2.25.3 added the `branch` key (ungated, vibe/cli/commands.py:214-222 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "inventory/aliases/log-level",
        "OPEN: reference v2.24.2 added the `log-level` key (ungated, vibe/cli/commands.py:103-109 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "inventory/aliases/plugins",
        "OPEN: reference v2.24.5 added the `plugins` key (gated on experimental_harness at :180, vibe/cli/commands.py:176-181 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "inventory/aliases/reload-plugins",
        "OPEN: reference v2.24.5 added the `reload-plugins` key (gated on experimental_harness at :186, vibe/cli/commands.py:182-187 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "inventory/aliases/skills",
        "OPEN: reference v2.25.0 added the `skills` key (gated on registry_skills_enabled at :63, vibe/cli/commands.py:59-64 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "inventory/aliases/todo",
        "OPEN: reference v2.25.5 added the `todo` key (gated on experimental_harness at :192, vibe/cli/commands.py:188-193 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "availability/keys/baseline",
        "OPEN: under this context the reference keeps 29 keys; this port lacks `branch`, `log-level` and closes its `vibe_code_enabled` gate on `remote-project` and `teleport` (crates/vibe-cli/src/tui/commands.rs:414), which reference v2.25.7 ungated (vibe/cli/commands.py:140-149 @4a960031); the missing keys come from vibe/cli/commands.py:103-109, :214-222 @4a960031",
    ),
    (
        "availability/count/baseline",
        "OPEN: under this context the reference keeps 29 keys; this port lacks `branch`, `log-level` and closes its `vibe_code_enabled` gate on `remote-project` and `teleport` (crates/vibe-cli/src/tui/commands.rs:414), which reference v2.25.7 ungated (vibe/cli/commands.py:140-149 @4a960031); the missing keys come from vibe/cli/commands.py:103-109, :214-222 @4a960031",
    ),
    (
        "availability/keys/clipboard",
        "OPEN: under this context the reference keeps 30 keys; this port lacks `branch`, `log-level` and closes its `vibe_code_enabled` gate on `remote-project` and `teleport` (crates/vibe-cli/src/tui/commands.rs:414), which reference v2.25.7 ungated (vibe/cli/commands.py:140-149 @4a960031); the missing keys come from vibe/cli/commands.py:103-109, :214-222 @4a960031",
    ),
    (
        "availability/count/clipboard",
        "OPEN: under this context the reference keeps 30 keys; this port lacks `branch`, `log-level` and closes its `vibe_code_enabled` gate on `remote-project` and `teleport` (crates/vibe-cli/src/tui/commands.rs:414), which reference v2.25.7 ungated (vibe/cli/commands.py:140-149 @4a960031); the missing keys come from vibe/cli/commands.py:103-109, :214-222 @4a960031",
    ),
    (
        "availability/keys/registrySkills",
        "OPEN: under this context the reference keeps 30 keys; this port lacks `branch`, `log-level`, `skills` and closes its `vibe_code_enabled` gate on `remote-project` and `teleport` (crates/vibe-cli/src/tui/commands.rs:414), which reference v2.25.7 ungated (vibe/cli/commands.py:140-149 @4a960031); the missing keys come from vibe/cli/commands.py:103-109, :214-222, :59-64 @4a960031",
    ),
    (
        "availability/count/registrySkills",
        "OPEN: under this context the reference keeps 30 keys; this port lacks `branch`, `log-level`, `skills` and closes its `vibe_code_enabled` gate on `remote-project` and `teleport` (crates/vibe-cli/src/tui/commands.rs:414), which reference v2.25.7 ungated (vibe/cli/commands.py:140-149 @4a960031); the missing keys come from vibe/cli/commands.py:103-109, :214-222, :59-64 @4a960031",
    ),
    (
        "availability/keys/experimentalHarness",
        "OPEN: under this context the reference keeps 32 keys; this port lacks `branch`, `log-level`, `plugins`, `reload-plugins`, `todo` and closes its `vibe_code_enabled` gate on `remote-project` and `teleport` (crates/vibe-cli/src/tui/commands.rs:414), which reference v2.25.7 ungated (vibe/cli/commands.py:140-149 @4a960031); the missing keys come from vibe/cli/commands.py:103-109, :176-181, :182-187, :188-193, :214-222 @4a960031",
    ),
    (
        "availability/count/experimentalHarness",
        "OPEN: under this context the reference keeps 32 keys; this port lacks `branch`, `log-level`, `plugins`, `reload-plugins`, `todo` and closes its `vibe_code_enabled` gate on `remote-project` and `teleport` (crates/vibe-cli/src/tui/commands.rs:414), which reference v2.25.7 ungated (vibe/cli/commands.py:140-149 @4a960031); the missing keys come from vibe/cli/commands.py:103-109, :176-181, :182-187, :188-193, :214-222 @4a960031",
    ),
    (
        "availability/keys/full",
        "OPEN: under this context the reference keeps 34 keys; this port lacks `branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo`; the missing keys come from vibe/cli/commands.py:103-109, :176-181, :182-187, :188-193, :214-222, :59-64 @4a960031",
    ),
    (
        "availability/count/full",
        "OPEN: under this context the reference keeps 34 keys; this port lacks `branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo`; the missing keys come from vibe/cli/commands.py:103-109, :176-181, :182-187, :188-193, :214-222, :59-64 @4a960031",
    ),
    (
        "availability/keys/excluded",
        "OPEN: under this context the reference keeps 31 keys; this port lacks `branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo`; the missing keys come from vibe/cli/commands.py:103-109, :176-181, :182-187, :188-193, :214-222, :59-64 @4a960031",
    ),
    (
        "availability/count/excluded",
        "OPEN: under this context the reference keeps 31 keys; this port lacks `branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo`; the missing keys come from vibe/cli/commands.py:103-109, :176-181, :182-187, :188-193, :214-222, :59-64 @4a960031",
    ),
    (
        "parse/key/teleport-baseline",
        "OPEN: reference v2.25.7 dropped `vibe_code_enabled` from CommandContext and ungated `teleport` and `remote-project` (vibe/cli/commands.py:10-13, :140-149 @4a960031); this port still gates both on it (crates/vibe-cli/src/tui/commands.rs:414), closed in every context that closes a reference gate",
    ),
    (
        "parse/alias/teleport-baseline",
        "OPEN: reference v2.25.7 dropped `vibe_code_enabled` from CommandContext and ungated `teleport` and `remote-project` (vibe/cli/commands.py:10-13, :140-149 @4a960031); this port still gates both on it (crates/vibe-cli/src/tui/commands.rs:414), closed in every context that closes a reference gate",
    ),
    (
        "parse/arguments/teleport-baseline",
        "OPEN: reference v2.25.7 dropped `vibe_code_enabled` from CommandContext and ungated `teleport` and `remote-project` (vibe/cli/commands.py:10-13, :140-149 @4a960031); this port still gates both on it (crates/vibe-cli/src/tui/commands.rs:414), closed in every context that closes a reference gate",
    ),
    (
        "parse/key/teleport-experimental-harness",
        "OPEN: reference v2.25.7 dropped `vibe_code_enabled` from CommandContext and ungated `teleport` and `remote-project` (vibe/cli/commands.py:10-13, :140-149 @4a960031); this port still gates both on it (crates/vibe-cli/src/tui/commands.rs:414), closed in every context that closes a reference gate",
    ),
    (
        "parse/alias/teleport-experimental-harness",
        "OPEN: reference v2.25.7 dropped `vibe_code_enabled` from CommandContext and ungated `teleport` and `remote-project` (vibe/cli/commands.py:10-13, :140-149 @4a960031); this port still gates both on it (crates/vibe-cli/src/tui/commands.rs:414), closed in every context that closes a reference gate",
    ),
    (
        "parse/arguments/teleport-experimental-harness",
        "OPEN: reference v2.25.7 dropped `vibe_code_enabled` from CommandContext and ungated `teleport` and `remote-project` (vibe/cli/commands.py:10-13, :140-149 @4a960031); this port still gates both on it (crates/vibe-cli/src/tui/commands.rs:414), closed in every context that closes a reference gate",
    ),
    (
        "parse/key/remote-project-baseline",
        "OPEN: reference v2.25.7 dropped `vibe_code_enabled` from CommandContext and ungated `teleport` and `remote-project` (vibe/cli/commands.py:10-13, :140-149 @4a960031); this port still gates both on it (crates/vibe-cli/src/tui/commands.rs:414), closed in every context that closes a reference gate",
    ),
    (
        "parse/alias/remote-project-baseline",
        "OPEN: reference v2.25.7 dropped `vibe_code_enabled` from CommandContext and ungated `teleport` and `remote-project` (vibe/cli/commands.py:10-13, :140-149 @4a960031); this port still gates both on it (crates/vibe-cli/src/tui/commands.rs:414), closed in every context that closes a reference gate",
    ),
    (
        "parse/arguments/remote-project-baseline",
        "OPEN: reference v2.25.7 dropped `vibe_code_enabled` from CommandContext and ungated `teleport` and `remote-project` (vibe/cli/commands.py:10-13, :140-149 @4a960031); this port still gates both on it (crates/vibe-cli/src/tui/commands.rs:414), closed in every context that closes a reference gate",
    ),
    (
        "parse/key/skills-registry-skills",
        "OPEN: reference v2.25.0 added the `skills` key (gated on registry_skills_enabled at :63, vibe/cli/commands.py:59-64 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "parse/alias/skills-registry-skills",
        "OPEN: reference v2.25.0 added the `skills` key (gated on registry_skills_enabled at :63, vibe/cli/commands.py:59-64 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "parse/arguments/skills-registry-skills",
        "OPEN: reference v2.25.0 added the `skills` key (gated on registry_skills_enabled at :63, vibe/cli/commands.py:59-64 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "parse/key/todo-experimental-harness",
        "OPEN: reference v2.25.5 added the `todo` key (gated on experimental_harness at :192, vibe/cli/commands.py:188-193 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "parse/alias/todo-experimental-harness",
        "OPEN: reference v2.25.5 added the `todo` key (gated on experimental_harness at :192, vibe/cli/commands.py:188-193 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "parse/arguments/todo-experimental-harness",
        "OPEN: reference v2.25.5 added the `todo` key (gated on experimental_harness at :192, vibe/cli/commands.py:188-193 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpDocument/count/lineCount",
        "OPEN: the reference's command section lists 34 keys under the full context (vibe/cli/commands.py:325-334 @4a960031); this port lists its 28, lacking `branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo`",
    ),
    (
        "helpDocument/count/commandLineCount",
        "OPEN: the reference's command section lists 34 keys under the full context (vibe/cli/commands.py:325-334 @4a960031); this port lists its 28, lacking `branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo`",
    ),
    (
        "helpSections/lineCount/commands",
        "OPEN: the reference's command section lists 34 keys under the full context (vibe/cli/commands.py:325-334 @4a960031); this port lists its 28, lacking `branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo`",
    ),
    (
        "helpCommands/index/branch",
        "OPEN: reference v2.25.3 added the `branch` key (ungated, vibe/cli/commands.py:214-222 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/line/branch",
        "OPEN: reference v2.25.3 added the `branch` key (ungated, vibe/cli/commands.py:214-222 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/aliases/branch",
        "OPEN: reference v2.25.3 added the `branch` key (ungated, vibe/cli/commands.py:214-222 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/index/clear",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `clear` upstream but are absent here, so its index shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/line/clear",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `clear` upstream but are absent here, so its line shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/index/compact",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `compact` upstream but are absent here, so its index shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/line/compact",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `compact` upstream but are absent here, so its line shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/index/config",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `config` upstream but are absent here, so its index shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/line/config",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `config` upstream but are absent here, so its line shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/index/copy",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `copy` upstream but are absent here, so its index shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/line/copy",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `copy` upstream but are absent here, so its line shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/index/data-retention",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `data-retention` upstream but are absent here, so its index shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/line/data-retention",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `data-retention` upstream but are absent here, so its line shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/index/debug",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `debug` upstream but are absent here, so its index shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/line/debug",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `debug` upstream but are absent here, so its line shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/index/exit",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `exit` upstream but are absent here, so its index shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/line/exit",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `exit` upstream but are absent here, so its line shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/index/help",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `help` upstream but are absent here, so its index shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/line/help",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `help` upstream but are absent here, so its line shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/index/leanstall",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `leanstall` upstream but are absent here, so its index shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/line/leanstall",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `leanstall` upstream but are absent here, so its line shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/index/log",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `log` upstream but are absent here, so its index shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/line/log",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch` sort before `log` upstream but are absent here, so its line shifts by 1; its alias list conforms",
    ),
    (
        "helpCommands/index/log-level",
        "OPEN: reference v2.24.2 added the `log-level` key (ungated, vibe/cli/commands.py:103-109 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/line/log-level",
        "OPEN: reference v2.24.2 added the `log-level` key (ungated, vibe/cli/commands.py:103-109 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/aliases/log-level",
        "OPEN: reference v2.24.2 added the `log-level` key (ungated, vibe/cli/commands.py:103-109 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/index/loop",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level` sort before `loop` upstream but are absent here, so its index shifts by 2; its alias list conforms",
    ),
    (
        "helpCommands/line/loop",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level` sort before `loop` upstream but are absent here, so its line shifts by 2; its alias list conforms",
    ),
    (
        "helpCommands/index/mcp",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level` sort before `mcp` upstream but are absent here, so its index shifts by 2; its alias list conforms",
    ),
    (
        "helpCommands/line/mcp",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level` sort before `mcp` upstream but are absent here, so its line shifts by 2; its alias list conforms",
    ),
    (
        "helpCommands/index/model",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level` sort before `model` upstream but are absent here, so its index shifts by 2; its alias list conforms",
    ),
    (
        "helpCommands/line/model",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level` sort before `model` upstream but are absent here, so its line shifts by 2; its alias list conforms",
    ),
    (
        "helpCommands/index/paste-image",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level` sort before `paste-image` upstream but are absent here, so its index shifts by 2; its alias list conforms",
    ),
    (
        "helpCommands/line/paste-image",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level` sort before `paste-image` upstream but are absent here, so its line shifts by 2; its alias list conforms",
    ),
    (
        "helpCommands/index/plugins",
        "OPEN: reference v2.24.5 added the `plugins` key (gated on experimental_harness at :180, vibe/cli/commands.py:176-181 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/line/plugins",
        "OPEN: reference v2.24.5 added the `plugins` key (gated on experimental_harness at :180, vibe/cli/commands.py:176-181 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/aliases/plugins",
        "OPEN: reference v2.24.5 added the `plugins` key (gated on experimental_harness at :180, vibe/cli/commands.py:176-181 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/index/proxy-setup",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins` sort before `proxy-setup` upstream but are absent here, so its index shifts by 3; its alias list conforms",
    ),
    (
        "helpCommands/line/proxy-setup",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins` sort before `proxy-setup` upstream but are absent here, so its line shifts by 3; its alias list conforms",
    ),
    (
        "helpCommands/index/reload",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins` sort before `reload` upstream but are absent here, so its index shifts by 3; its alias list conforms",
    ),
    (
        "helpCommands/line/reload",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins` sort before `reload` upstream but are absent here, so its line shifts by 3; its alias list conforms",
    ),
    (
        "helpCommands/index/reload-plugins",
        "OPEN: reference v2.24.5 added the `reload-plugins` key (gated on experimental_harness at :186, vibe/cli/commands.py:182-187 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/line/reload-plugins",
        "OPEN: reference v2.24.5 added the `reload-plugins` key (gated on experimental_harness at :186, vibe/cli/commands.py:182-187 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/aliases/reload-plugins",
        "OPEN: reference v2.24.5 added the `reload-plugins` key (gated on experimental_harness at :186, vibe/cli/commands.py:182-187 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/index/remote-project",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins` sort before `remote-project` upstream but are absent here, so its index shifts by 4; its alias list conforms",
    ),
    (
        "helpCommands/line/remote-project",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins` sort before `remote-project` upstream but are absent here, so its line shifts by 4; its alias list conforms",
    ),
    (
        "helpCommands/index/rename",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins` sort before `rename` upstream but are absent here, so its index shifts by 4; its alias list conforms",
    ),
    (
        "helpCommands/line/rename",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins` sort before `rename` upstream but are absent here, so its line shifts by 4; its alias list conforms",
    ),
    (
        "helpCommands/index/resume",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins` sort before `resume` upstream but are absent here, so its index shifts by 4; its alias list conforms",
    ),
    (
        "helpCommands/line/resume",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins` sort before `resume` upstream but are absent here, so its line shifts by 4; its alias list conforms",
    ),
    (
        "helpCommands/index/retry",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins` sort before `retry` upstream but are absent here, so its index shifts by 4; its alias list conforms",
    ),
    (
        "helpCommands/line/retry",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins` sort before `retry` upstream but are absent here, so its line shifts by 4; its alias list conforms",
    ),
    (
        "helpCommands/index/rewind",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins` sort before `rewind` upstream but are absent here, so its index shifts by 4; its alias list conforms",
    ),
    (
        "helpCommands/line/rewind",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins` sort before `rewind` upstream but are absent here, so its line shifts by 4; its alias list conforms",
    ),
    (
        "helpCommands/index/skills",
        "OPEN: reference v2.25.0 added the `skills` key (gated on registry_skills_enabled at :63, vibe/cli/commands.py:59-64 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/line/skills",
        "OPEN: reference v2.25.0 added the `skills` key (gated on registry_skills_enabled at :63, vibe/cli/commands.py:59-64 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/aliases/skills",
        "OPEN: reference v2.25.0 added the `skills` key (gated on registry_skills_enabled at :63, vibe/cli/commands.py:59-64 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/index/status",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins`, `skills` sort before `status` upstream but are absent here, so its index shifts by 5; its alias list conforms",
    ),
    (
        "helpCommands/line/status",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins`, `skills` sort before `status` upstream but are absent here, so its line shifts by 5; its alias list conforms",
    ),
    (
        "helpCommands/index/teleport",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins`, `skills` sort before `teleport` upstream but are absent here, so its index shifts by 5; its alias list conforms",
    ),
    (
        "helpCommands/line/teleport",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins`, `skills` sort before `teleport` upstream but are absent here, so its line shifts by 5; its alias list conforms",
    ),
    (
        "helpCommands/index/theme",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins`, `skills` sort before `theme` upstream but are absent here, so its index shifts by 5; its alias list conforms",
    ),
    (
        "helpCommands/line/theme",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins`, `skills` sort before `theme` upstream but are absent here, so its line shifts by 5; its alias list conforms",
    ),
    (
        "helpCommands/index/thinking",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins`, `skills` sort before `thinking` upstream but are absent here, so its index shifts by 5; its alias list conforms",
    ),
    (
        "helpCommands/line/thinking",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins`, `skills` sort before `thinking` upstream but are absent here, so its line shifts by 5; its alias list conforms",
    ),
    (
        "helpCommands/index/todo",
        "OPEN: reference v2.25.5 added the `todo` key (gated on experimental_harness at :192, vibe/cli/commands.py:188-193 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/line/todo",
        "OPEN: reference v2.25.5 added the `todo` key (gated on experimental_harness at :192, vibe/cli/commands.py:188-193 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/aliases/todo",
        "OPEN: reference v2.25.5 added the `todo` key (gated on experimental_harness at :192, vibe/cli/commands.py:188-193 @4a960031); this port's table has no such key (crates/vibe-cli/src/tui/commands.rs:145-304)",
    ),
    (
        "helpCommands/index/unleanstall",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo` sort before `unleanstall` upstream but are absent here, so its index shifts by 6; its alias list conforms",
    ),
    (
        "helpCommands/line/unleanstall",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo` sort before `unleanstall` upstream but are absent here, so its line shifts by 6; its alias list conforms",
    ),
    (
        "helpCommands/index/voice",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo` sort before `voice` upstream but are absent here, so its index shifts by 6; its alias list conforms",
    ),
    (
        "helpCommands/line/voice",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo` sort before `voice` upstream but are absent here, so its line shifts by 6; its alias list conforms",
    ),
    (
        "helpCommands/index/whoami",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo` sort before `whoami` upstream but are absent here, so its index shifts by 6; its alias list conforms",
    ),
    (
        "helpCommands/line/whoami",
        "OPEN: the command section sorts by key (vibe/cli/commands.py:325-334 @4a960031) and `branch`, `log-level`, `plugins`, `reload-plugins`, `skills`, `todo` sort before `whoami` upstream but are absent here, so its line shifts by 6; its alias list conforms",
    ),
    (
        "helpProse/length/line-18",
        "OPEN: reference line 18 is the line of `branch`, which this port lacks (vibe/cli/commands.py:214-222 @4a960031, v2.25.3); this port's line 18 is its `clear` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-18",
        "OPEN: reference line 18 is the line of `branch`, which this port lacks (vibe/cli/commands.py:214-222 @4a960031, v2.25.3); this port's line 18 is its `clear` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-19",
        "OPEN: reference line 19 is the `clear` line, whose description v2.24.1 rewrote (vibe/cli/commands.py:75-81 @4a960031; this port keeps the old one at crates/vibe-cli/src/tui/commands.rs:175); this port's line 19 is its `compact` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-19",
        "OPEN: reference line 19 is the `clear` line, whose description v2.24.1 rewrote (vibe/cli/commands.py:75-81 @4a960031; this port keeps the old one at crates/vibe-cli/src/tui/commands.rs:175); this port's line 19 is its `compact` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-20",
        "OPEN: reference line 20 is the `compact` line, which this port renders identically one or more lines earlier; this port's line 20 is its `config` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-20",
        "OPEN: reference line 20 is the `compact` line, which this port renders identically one or more lines earlier; this port's line 20 is its `config` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-21",
        "OPEN: reference line 21 is the `config` line, which this port renders identically one or more lines earlier; this port's line 21 is its `copy` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-21",
        "OPEN: reference line 21 is the `config` line, which this port renders identically one or more lines earlier; this port's line 21 is its `copy` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-22",
        "OPEN: reference line 22 is the `copy` line, which this port renders identically one or more lines earlier; this port's line 22 is its `data-retention` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-22",
        "OPEN: reference line 22 is the `copy` line, which this port renders identically one or more lines earlier; this port's line 22 is its `data-retention` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-23",
        "OPEN: reference line 23 is the `data-retention` line, which this port renders identically one or more lines earlier; this port's line 23 is its `debug` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-23",
        "OPEN: reference line 23 is the `data-retention` line, which this port renders identically one or more lines earlier; this port's line 23 is its `debug` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-24",
        "OPEN: reference line 24 is the `debug` line, which this port renders identically one or more lines earlier; this port's line 24 is its `exit` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-24",
        "OPEN: reference line 24 is the `debug` line, which this port renders identically one or more lines earlier; this port's line 24 is its `exit` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-25",
        "OPEN: reference line 25 is the `exit` line, which this port renders identically one or more lines earlier; this port's line 25 is its `help` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-25",
        "OPEN: reference line 25 is the `exit` line, which this port renders identically one or more lines earlier; this port's line 25 is its `help` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-26",
        "OPEN: reference line 26 is the `help` line, which this port renders identically one or more lines earlier; this port's line 26 is its `leanstall` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-26",
        "OPEN: reference line 26 is the `help` line, which this port renders identically one or more lines earlier; this port's line 26 is its `leanstall` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-27",
        "OPEN: reference line 27 is the `leanstall` line, which this port renders identically one or more lines earlier; this port's line 27 is its `log` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-27",
        "OPEN: reference line 27 is the `leanstall` line, which this port renders identically one or more lines earlier; this port's line 27 is its `log` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-28",
        "OPEN: reference line 28 is the `log` line, which this port renders identically one or more lines earlier; this port's line 28 is its `loop` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-28",
        "OPEN: reference line 28 is the `log` line, which this port renders identically one or more lines earlier; this port's line 28 is its `loop` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-29",
        "OPEN: reference line 29 is the line of `log-level`, which this port lacks (vibe/cli/commands.py:103-109 @4a960031, v2.24.2); this port's line 29 is its `mcp` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-29",
        "OPEN: reference line 29 is the line of `log-level`, which this port lacks (vibe/cli/commands.py:103-109 @4a960031, v2.24.2); this port's line 29 is its `mcp` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-30",
        "OPEN: reference line 30 is the `loop` line, which this port renders identically one or more lines earlier; this port's line 30 is its `model` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-30",
        "OPEN: reference line 30 is the `loop` line, which this port renders identically one or more lines earlier; this port's line 30 is its `model` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-31",
        "OPEN: reference line 31 is the `mcp` line, which this port renders identically one or more lines earlier; this port's line 31 is its `paste-image` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-31",
        "OPEN: reference line 31 is the `mcp` line, which this port renders identically one or more lines earlier; this port's line 31 is its `paste-image` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-32",
        "OPEN: reference line 32 is the `model` line, which this port renders identically one or more lines earlier; this port's line 32 is its `proxy-setup` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-32",
        "OPEN: reference line 32 is the `model` line, which this port renders identically one or more lines earlier; this port's line 32 is its `proxy-setup` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-33",
        "OPEN: reference line 33 is the `paste-image` line, which this port renders identically one or more lines earlier; this port's line 33 is its `reload` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-33",
        "OPEN: reference line 33 is the `paste-image` line, which this port renders identically one or more lines earlier; this port's line 33 is its `reload` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-34",
        "OPEN: reference line 34 is the line of `plugins`, which this port lacks (vibe/cli/commands.py:176-181 @4a960031, v2.24.5); this port's line 34 is its `remote-project` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-34",
        "OPEN: reference line 34 is the line of `plugins`, which this port lacks (vibe/cli/commands.py:176-181 @4a960031, v2.24.5); this port's line 34 is its `remote-project` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-35",
        "OPEN: reference line 35 is the `proxy-setup` line, which this port renders identically one or more lines earlier; this port's line 35 is its `rename` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-35",
        "OPEN: reference line 35 is the `proxy-setup` line, which this port renders identically one or more lines earlier; this port's line 35 is its `rename` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-36",
        "OPEN: reference line 36 is the `reload` line, which this port renders identically one or more lines earlier; this port's line 36 is its `resume` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-36",
        "OPEN: reference line 36 is the `reload` line, which this port renders identically one or more lines earlier; this port's line 36 is its `resume` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-37",
        "OPEN: reference line 37 is the line of `reload-plugins`, which this port lacks (vibe/cli/commands.py:182-187 @4a960031, v2.24.5); this port's line 37 is its `retry` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-37",
        "OPEN: reference line 37 is the line of `reload-plugins`, which this port lacks (vibe/cli/commands.py:182-187 @4a960031, v2.24.5); this port's line 37 is its `retry` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-38",
        "OPEN: reference line 38 is the `remote-project` line, which this port renders identically one or more lines earlier; this port's line 38 is its `rewind` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-38",
        "OPEN: reference line 38 is the `remote-project` line, which this port renders identically one or more lines earlier; this port's line 38 is its `rewind` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-39",
        "OPEN: reference line 39 is the `rename` line, which this port renders identically one or more lines earlier; this port's line 39 is its `status` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-39",
        "OPEN: reference line 39 is the `rename` line, which this port renders identically one or more lines earlier; this port's line 39 is its `status` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-40",
        "OPEN: reference line 40 is the `resume` line, which this port renders identically one or more lines earlier; this port's line 40 is its `teleport` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-40",
        "OPEN: reference line 40 is the `resume` line, which this port renders identically one or more lines earlier; this port's line 40 is its `teleport` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-41",
        "OPEN: reference line 41 is the `retry` line, which this port renders identically one or more lines earlier; this port's line 41 is its `theme` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-41",
        "OPEN: reference line 41 is the `retry` line, which this port renders identically one or more lines earlier; this port's line 41 is its `theme` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-42",
        "OPEN: reference line 42 is the `rewind` line, which this port renders identically one or more lines earlier; this port's line 42 is its `thinking` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-42",
        "OPEN: reference line 42 is the `rewind` line, which this port renders identically one or more lines earlier; this port's line 42 is its `thinking` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-43",
        "OPEN: reference line 43 is the line of `skills`, which this port lacks (vibe/cli/commands.py:59-64 @4a960031, v2.25.0); this port's line 43 is its `unleanstall` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-43",
        "OPEN: reference line 43 is the line of `skills`, which this port lacks (vibe/cli/commands.py:59-64 @4a960031, v2.25.0); this port's line 43 is its `unleanstall` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-44",
        "OPEN: reference line 44 is the `status` line, which this port renders identically one or more lines earlier; this port's line 44 is its `voice` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-44",
        "OPEN: reference line 44 is the `status` line, which this port renders identically one or more lines earlier; this port's line 44 is its `voice` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-45",
        "OPEN: reference line 45 is the `teleport` line, which this port renders identically one or more lines earlier; this port's line 45 is its `whoami` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-45",
        "OPEN: reference line 45 is the `teleport` line, which this port renders identically one or more lines earlier; this port's line 45 is its `whoami` line, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-46",
        "OPEN: reference line 46 is the `theme` line, which this port renders identically one or more lines earlier; this port's document ends before line 46, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-46",
        "OPEN: reference line 46 is the `theme` line, which this port renders identically one or more lines earlier; this port's document ends before line 46, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-47",
        "OPEN: reference line 47 is the `thinking` line, which this port renders identically one or more lines earlier; this port's document ends before line 47, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-47",
        "OPEN: reference line 47 is the `thinking` line, which this port renders identically one or more lines earlier; this port's document ends before line 47, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-48",
        "OPEN: reference line 48 is the line of `todo`, which this port lacks (vibe/cli/commands.py:188-193 @4a960031, v2.25.5); this port's document ends before line 48, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-48",
        "OPEN: reference line 48 is the line of `todo`, which this port lacks (vibe/cli/commands.py:188-193 @4a960031, v2.25.5); this port's document ends before line 48, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-49",
        "OPEN: reference line 49 is the `unleanstall` line, which this port renders identically one or more lines earlier; this port's document ends before line 49, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-49",
        "OPEN: reference line 49 is the `unleanstall` line, which this port renders identically one or more lines earlier; this port's document ends before line 49, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-50",
        "OPEN: reference line 50 is the `voice` line, which this port renders identically one or more lines earlier; this port's document ends before line 50, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-50",
        "OPEN: reference line 50 is the `voice` line, which this port renders identically one or more lines earlier; this port's document ends before line 50, displaced by the keys it lacks",
    ),
    (
        "helpProse/length/line-51",
        "OPEN: reference line 51 is the `whoami` line, which this port renders identically one or more lines earlier; this port's document ends before line 51, displaced by the keys it lacks",
    ),
    (
        "helpProse/digest/line-51",
        "OPEN: reference line 51 is the `whoami` line, which this port renders identically one or more lines earlier; this port's document ends before line 51, displaced by the keys it lacks",
    ),
];

/// One family's shape: which case fields the capture authored and which ones
/// both sides answer.
struct Family {
    name: &'static str,
    inputs: &'static [&'static str],
    answers: &'static [&'static str],
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

fn corpus() -> Map<String, Value> {
    let path = repo_root().join(CORPUS_RELATIVE);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()));
    let parsed: Value = serde_json::from_str(&raw).expect("the registry corpus parses");
    let corpus = parsed
        .as_object()
        .expect("the registry corpus is an object")
        .clone();
    assert_eq!(
        corpus.get("schemaVersion").and_then(Value::as_u64),
        Some(u64::from(CORPUS_SCHEMA_VERSION)),
        "the corpus layout moved; regenerate it with {CAPTURE_SCRIPT}"
    );
    assert_eq!(
        corpus
            .get("reference")
            .and_then(|reference| reference.get("commit"))
            .and_then(Value::as_str),
        Some(REFERENCE_COMMIT),
        "the corpus was captured from an unpinned reference; the replay compares one revision or \
         none"
    );
    corpus
}

fn cases<'a>(corpus: &'a Map<String, Value>, family: &str) -> &'a Vec<Value> {
    corpus
        .get(family)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("the corpus carries the {family} family as an array"))
}

fn case_id(case: &Value) -> &str {
    case.get("id")
        .and_then(Value::as_str)
        .expect("every corpus case carries an id")
}

// --------------------------------------------------------------------------
// The ledger
// --------------------------------------------------------------------------

/// Whether a ledger entry covers a divergence key: an exact match, or a
/// `prefix*` entry the key starts with.
fn covers(entry: &str, key: &str) -> bool {
    entry
        .strip_suffix('*')
        .map_or(entry == key, |prefix| key.starts_with(prefix))
}

/// Records one comparison, so a family reports a count and a divergence names
/// itself instead of stopping at the first one.
#[derive(Default)]
struct Report {
    conformant: usize,
    total: usize,
    divergences: Vec<String>,
    observed: Vec<String>,
}

impl Report {
    fn check(
        &mut self,
        family: &str,
        field: &str,
        case: &str,
        expected: &Value,
        actual: Option<&Value>,
    ) {
        self.total = self.total.saturating_add(1);
        if actual == Some(expected) {
            self.conformant = self.conformant.saturating_add(1);
            return;
        }
        self.observed.push(format!("{family}/{field}/{case}"));
        self.divergences.push(format!(
            "{family}/{field}/{case}: reference {expected}, port {}",
            actual.map_or_else(|| "absent".to_owned(), Value::to_string)
        ));
    }
}

fn audit(report: &Report, family: &str, ledger: &[(&str, &str)]) -> (Vec<String>, Vec<String>) {
    let unrecorded = report
        .divergences
        .iter()
        .filter(|line| {
            let key = line.split(':').next().unwrap_or_default();
            !ledger.iter().any(|(entry, _)| covers(entry, key))
        })
        .cloned()
        .collect::<Vec<_>>();
    let family_prefix = format!("{family}/");
    let stale = ledger
        .iter()
        .map(|(entry, _)| (*entry).to_owned())
        .filter(|entry| entry.starts_with(&family_prefix))
        .filter(|entry| !report.observed.iter().any(|key| covers(entry, key)))
        .collect::<Vec<_>>();
    (unrecorded, stale)
}

fn settle(report: &Report, family: &str) -> usize {
    let (unrecorded, stale) = audit(report, family, DIVERGENCES);
    assert!(
        unrecorded.is_empty(),
        "{family} diverges from the reference and is unrecorded:\n{}",
        unrecorded.join("\n")
    );
    assert!(
        stale.is_empty(),
        "these {family} entries conform now and their ledger entry is stale: {stale:?}"
    );
    let ledgered = report.total.saturating_sub(report.conformant);
    println!(
        "commands: {family} {}/{} conform ({ledgered} ledgered)",
        report.conformant, report.total
    );
    report.total
}

// --------------------------------------------------------------------------
// This build's answers
// --------------------------------------------------------------------------

/// The help lines this port *authors*: the section headings, the shortcut lines
/// and the feature lines.
///
/// They are held permanently unequal to every reference digest by
/// [`this_ports_help_prose_never_matches_a_reference_digest`], which is how the
/// licensing boundary stays measurable without a reference line entering this
/// repository.
///
/// The twenty-eight command lines are deliberately not among them. A command
/// line is `- <aliases>: <description>`, and both halves are the observable
/// contract rather than prose: the aliases come from the registry and the
/// descriptions are already byte-identical to the reference's, which the four
/// `commands-*` popup traces assert and which no story of this PRD may rewrite.
/// Measured against the corpus captured at 2.25.7, twenty-seven of the
/// twenty-eight lines rebuilt from `COMMANDS` hash to a digest `helpProse`
/// records; `clear` is the exception because upstream rewrote its description,
/// which the `OPEN` ledger records. Routing them through this
/// function would make US-231 unsatisfiable: it would forbid the very lines its
/// own criteria require. `helpCommands` is what compares them, on their order
/// and their alias list, which is the part a port can get wrong.
fn port_help_lines() -> Vec<String> {
    help::authored_lines()
}

/// The availability contexts the corpus declares, rebuilt as this port's own
/// [`CommandContext`].
///
/// The definitions are read out of the `availability` family rather than
/// restated here, so the parse family resolves under exactly the contexts the
/// capture recorded and a new context is added in one place.
///
/// The reference's `CommandContext` carries `registry_skills_enabled` and
/// `experimental_harness` (`vibe/cli/commands.py:10-13` at the pin), and this
/// port's carries `vibe_code_enabled`, a gate the reference dropped. Neither
/// side can be handed the other's flags, so the port's surplus gate is opened
/// exactly where the context opens every reference gate (`full` and
/// `excluded`) and closed everywhere else. That keeps the gate this port still
/// applies to `/teleport` and `/remote-project` observable in the contexts that
/// close a gate, instead of hiding it behind the port's default of `true`.
fn port_contexts(corpus: &Map<String, Value>) -> BTreeMap<String, CommandContext> {
    cases(corpus, "availability")
        .iter()
        .map(|case| {
            let excluded = case
                .get("excluded")
                .and_then(Value::as_array)
                .expect("every availability case declares an excluded list")
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .expect("an excluded entry is a command name")
                        .to_owned()
                })
                .collect::<Vec<_>>();
            let flag = |name: &str| {
                case.get(name)
                    .and_then(Value::as_bool)
                    .unwrap_or_else(|| panic!("every availability case declares {name}"))
            };
            let every_reference_gate_open =
                flag("registrySkillsEnabled") && flag("experimentalHarness");
            let context = CommandContext::new(every_reference_gate_open)
                .with_clipboard_image_supported(
                    case.get("clipboardSupported")
                        .and_then(Value::as_bool)
                        .expect("every availability case declares clipboardSupported"),
                )
                .with_excluded(excluded.iter().map(String::as_str));
            (case_id(case).to_owned(), context)
        })
        .collect()
}

fn available_keys(context: &CommandContext) -> Vec<&'static str> {
    let mut keys = COMMANDS
        .iter()
        .filter(|command| command_available_in(command, context))
        .map(|command| command.name)
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys
}

fn strings(values: impl IntoIterator<Item = impl Into<String>>) -> Value {
    Value::Array(
        values
            .into_iter()
            .map(|value| Value::String(value.into()))
            .collect(),
    )
}

fn count(value: usize) -> Value {
    Value::Number(value.into())
}

/// The context the capture recorded the help families under: every command
/// available and nothing excluded.
fn port_help_context() -> CommandContext {
    CommandContext::new(true).with_clipboard_image_supported(true)
}

/// The document `/help` writes, split into lines.
fn port_help_document() -> Vec<String> {
    help::document(&port_help_context())
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Each heading's offset, its level, and how many non-blank lines follow it.
/// Derived from the document rather than restated, so a section that moves is
/// measured where it landed.
fn port_help_sections(lines: &[String]) -> Vec<(usize, usize, usize)> {
    let mut sections: Vec<(usize, usize, usize)> = Vec::new();
    for (offset, line) in lines.iter().enumerate() {
        let level = line
            .chars()
            .take_while(|character| *character == '#')
            .count();
        if level > 0 {
            sections.push((offset, level, 0));
        } else if !line.is_empty()
            && let Some(section) = sections.last_mut()
        {
            section.2 = section.2.saturating_add(1);
        }
    }
    sections
}

/// Every command line: the registry key it publishes, the document offset it
/// sits at, and the aliases it lists, read back off the published line rather
/// than rebuilt, so the answer measures what an operator sees.
fn port_help_command_lines(lines: &[String]) -> Vec<(&'static str, usize, Vec<String>)> {
    let context = port_help_context();
    let mut commands = COMMANDS
        .iter()
        .filter(|command| command_available_in(command, &context))
        .collect::<Vec<_>>();
    commands.sort_unstable_by_key(|command| command.name);
    let start = lines.len().saturating_sub(commands.len());
    commands
        .into_iter()
        .enumerate()
        .map(|(index, command)| {
            let offset = start.saturating_add(index);
            let aliases = lines
                .get(offset)
                .and_then(|line| line.strip_prefix("- "))
                .and_then(|rest| rest.split_once(": "))
                .map(|(spans, _)| {
                    spans
                        .split(", ")
                        .map(|span| span.trim_matches('`').to_owned())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (command.name, offset, aliases)
        })
        .collect()
}

/// What this build answers for one case of `family`, or [`None`] when the case
/// names something this build has no answer for, which the replay records as a
/// divergence rather than a skip.
fn port_answer(
    contexts: &BTreeMap<String, CommandContext>,
    family: &str,
    field: &str,
    case: &Map<String, Value>,
) -> Option<Value> {
    let id = case.get("id")?.as_str()?;
    match family {
        "counts" => {
            let aliases = COMMANDS
                .iter()
                .flat_map(|command| command.aliases.iter().copied())
                .collect::<Vec<_>>();
            let value = match id {
                "keys" => COMMANDS.len(),
                "aliases" => aliases.len(),
                "slashAliases" => aliases.iter().filter(|a| a.starts_with('/')).count(),
                "bareAliases" => aliases.iter().filter(|a| !a.starts_with('/')).count(),
                _ => return None,
            };
            Some(count(value))
        }
        "inventory" => {
            let command = COMMANDS.iter().find(|command| command.name == id)?;
            let mut aliases = command.aliases.to_vec();
            aliases.sort_unstable();
            Some(strings(aliases))
        }
        "availability" => {
            let context = contexts.get(id)?;
            let keys = available_keys(context);
            match field {
                "keys" => Some(strings(keys)),
                "count" => Some(count(keys.len())),
                _ => None,
            }
        }
        "parse" => {
            let context = contexts.get(case.get("context")?.as_str()?)?;
            let input = case.get("input")?.as_str()?;
            let Some(parsed) = parse_command_in(input, context) else {
                // The reference records a refusal as three nulls, so this port
                // answers the same shape rather than an absent value: a
                // refusal both sides agree on is a conformance, not a hole.
                return Some(Value::Null);
            };
            let command = COMMANDS.iter().find(|command| command.id == parsed.id)?;
            match field {
                "key" => Some(Value::String(command.name.to_owned())),
                // The reference records the alias-map entry the head word was
                // looked up under, which is the declared alias itself because
                // every declared alias is lowercase. Deriving it here rather
                // than reading a field off `ParsedCommand` is deliberate: it
                // fails when this port resolves through a fold the reference
                // does not perform.
                "alias" => {
                    let lowered = parsed.alias.to_lowercase();
                    command
                        .aliases
                        .iter()
                        .find(|alias| **alias == lowered.as_str())
                        .map(|alias| Value::String((*alias).to_owned()))
                }
                "arguments" => Some(Value::String(parsed.arguments.to_owned())),
                _ => None,
            }
        }
        "helpDocument" => {
            let lines = port_help_document();
            let value = match id {
                "lineCount" => lines.len(),
                "blankLineCount" => lines.iter().filter(|line| line.is_empty()).count(),
                "sectionCount" => port_help_sections(&lines).len(),
                "commandLineCount" => port_help_command_lines(&lines).len(),
                _ => return None,
            };
            Some(count(value))
        }
        "helpSections" => {
            let lines = port_help_document();
            let index = match id {
                "keyboardShortcuts" => 0,
                "specialFeatures" => 1,
                "commands" => 2,
                _ => return None,
            };
            let (heading_line, level, line_count) = *port_help_sections(&lines).get(index)?;
            match field {
                "index" => Some(count(index)),
                "headingLine" => Some(count(heading_line)),
                "level" => Some(count(level)),
                "lineCount" => Some(count(line_count)),
                _ => None,
            }
        }
        "helpCommands" => {
            let lines = port_help_document();
            let commands = port_help_command_lines(&lines);
            let index = commands.iter().position(|(name, _, _)| *name == id)?;
            let (_, line, aliases) = commands.get(index)?;
            match field {
                "index" => Some(count(index)),
                "line" => Some(count(*line)),
                "aliases" => Some(strings(aliases.clone())),
                _ => None,
            }
        }
        "helpProse" => {
            let offset = id.strip_prefix("line-")?.parse::<usize>().ok()?;
            let line = port_help_document().get(offset)?.clone();
            match field {
                "length" => Some(count(line.len())),
                "digest" => Some(Value::String(hex_digest(&line))),
                _ => None,
            }
        }
        _ => None,
    }
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

fn run_family(
    corpus: &Map<String, Value>,
    contexts: &BTreeMap<String, CommandContext>,
    family: &Family,
    report: &mut Report,
) {
    let declared = family
        .inputs
        .iter()
        .chain(family.answers.iter())
        .copied()
        .collect::<BTreeSet<_>>();
    for case in cases(corpus, family.name) {
        let object = case
            .as_object()
            .unwrap_or_else(|| panic!("{} cases are objects", family.name));
        let carried = object
            .keys()
            .map(String::as_str)
            .filter(|key| *key != "id")
            .collect::<BTreeSet<_>>();
        assert!(
            carried.iter().all(|key| declared.contains(key)),
            "the {} case {} carries fields this replay does not read: {:?}; declare them as an \
             input or as an answer rather than leaving them unread",
            family.name,
            case_id(case),
            carried.difference(&declared).collect::<Vec<_>>()
        );
        let identifier = case_id(case);
        for field in family.answers {
            let Some(expected) = object.get(*field) else {
                continue;
            };
            let actual = port_answer(contexts, family.name, field, object);
            report.check(family.name, field, identifier, expected, actual.as_ref());
        }
    }
}

#[test]
fn every_corpus_key_is_a_family_this_replay_reads() {
    let corpus = corpus();
    let declared = FAMILIES
        .iter()
        .map(|family| family.name)
        .chain(METADATA)
        .collect::<BTreeSet<_>>();
    let carried = corpus.keys().map(String::as_str).collect::<BTreeSet<_>>();
    assert_eq!(
        carried, declared,
        "the corpus and this replay disagree on which families exist; regenerate with \
         {CAPTURE_SCRIPT} or declare the family here"
    );
}

#[test]
fn every_ledger_entry_names_a_declared_family() {
    let declared = FAMILIES
        .iter()
        .map(|family| family.name)
        .collect::<BTreeSet<_>>();
    let orphans = DIVERGENCES
        .iter()
        .map(|(entry, _)| *entry)
        .filter(|entry| {
            entry
                .split('/')
                .next()
                .is_none_or(|family| !declared.contains(family))
        })
        .collect::<Vec<_>>();
    assert!(
        orphans.is_empty(),
        "these ledger entries name a family the corpus does not carry: {orphans:?}"
    );
}

fn hex_digest(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn reference_help_digests(corpus: &Map<String, Value>) -> BTreeMap<String, String> {
    let digests = cases(corpus, "helpProse")
        .iter()
        .map(|case| {
            (
                case_id(case).to_owned(),
                case.get("digest")
                    .and_then(Value::as_str)
                    .expect("every help prose case carries a digest")
                    .to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert!(
        !digests.is_empty(),
        "the corpus records no reference help prose to stay unequal to"
    );
    digests
}

/// `NOTICE` forbids shipping a reference-authored line, and a digest is what
/// makes that enforceable without carrying the text: every help line this port
/// ships is compared against every reference digest and has to differ from each.
/// A blank line would trivially differ, so it fails too.
#[test]
fn this_ports_help_prose_never_matches_a_reference_digest() {
    let corpus = corpus();
    let reference = reference_help_digests(&corpus);
    for line in port_help_lines() {
        assert!(
            !line.trim().is_empty(),
            "this port's help carries a blank line as prose, which is not a line of its own"
        );
        let digest = hex_digest(&line);
        for (name, expected) in &reference {
            assert_ne!(
                &digest, expected,
                "this port's help reproduces the reference's {name}, which `NOTICE` forbids"
            );
        }
    }
}

/// The other half of the same boundary: the corpus reduced the reference's lines
/// to digests, and nothing may put one back. Every string the corpus carries is
/// hashed and held unequal to every digest it records, so a line pasted into a
/// note, an input or an identifier fails the suite rather than shipping.
#[test]
fn the_corpus_carries_no_reference_help_line_in_cleartext() {
    let corpus = corpus();
    let reference = reference_help_digests(&corpus)
        .into_values()
        .collect::<BTreeSet<_>>();
    let mut offenders = Vec::new();
    collect_strings(&Value::Object(corpus.clone()), &mut |text| {
        if reference.contains(&hex_digest(text)) {
            offenders.push(text.to_owned());
        }
    });
    assert!(
        offenders.is_empty(),
        "{CORPUS_RELATIVE} carries {} reference-authored line(s) in cleartext; the corpus records \
         them as a length and a digest and never as text",
        offenders.len()
    );
}

fn collect_strings(value: &Value, visit: &mut impl FnMut(&str)) {
    match value {
        Value::String(text) => visit(text),
        Value::Array(items) => {
            for item in items {
                collect_strings(item, visit);
            }
        }
        Value::Object(entries) => {
            for item in entries.values() {
                collect_strings(item, visit);
            }
        }
        _ => {}
    }
}

/// The two failure modes the replay exists for, proven on a report the test
/// builds rather than on the corpus: a divergence the ledger does not name has
/// to be reported with its family, its case and both values, and a ledger entry
/// whose divergence stopped reproducing has to be reported as stale.
#[test]
fn the_ledger_reports_an_unrecorded_divergence_and_a_stale_entry() {
    let ledger = [("parse/key/named", "recorded")];
    let mut report = Report::default();
    report.check(
        "parse",
        "key",
        "unnamed",
        &Value::String("exit".to_owned()),
        Some(&Value::Null),
    );
    let (unrecorded, stale) = audit(&report, "parse", &ledger);
    assert_eq!(unrecorded.len(), 1, "the unnamed divergence is reported");
    let reported = &unrecorded[0];
    assert!(reported.starts_with("parse/key/unnamed:"), "{reported}");
    assert!(reported.contains("reference \"exit\""), "{reported}");
    assert!(reported.contains("port null"), "{reported}");
    assert_eq!(
        stale,
        vec!["parse/key/named".to_owned()],
        "an entry whose case stopped diverging is stale"
    );
}

/// The parse family is only an oracle for the branches it reaches, and the
/// criteria that motivated it name them one by one. This holds the corpus to
/// carrying each rather than to a probe count alone.
#[test]
fn the_parse_family_reaches_every_branch_it_was_built_for() {
    let corpus = corpus();
    let probes = cases(&corpus, "parse");
    assert!(
        probes.len() >= 40,
        "the parse family carries {} probes, below the 40 the corpus commits to",
        probes.len()
    );
    let inputs = probes
        .iter()
        .map(|case| {
            case.get("input")
                .and_then(Value::as_str)
                .expect("every parse case carries its input")
        })
        .collect::<Vec<_>>();
    let has = |predicate: &dyn Fn(&str) -> bool| inputs.iter().any(|input| predicate(input));
    assert!(has(&|input| input.starts_with(' ')), "leading whitespace");
    assert!(has(&|input| input.ends_with(' ')), "trailing whitespace");
    assert!(
        has(&|input| input.contains("  ") && input.trim().contains("  ")),
        "an interior whitespace run"
    );
    assert!(has(&|input| input == "exit"), "a bare alias alone");
    assert!(
        has(&|input| input.starts_with("exit ")),
        "a bare alias followed by arguments"
    );
    assert!(
        has(&|input| input.starts_with("/mcp ")),
        "a slash alias followed by arguments"
    );
    assert!(has(&|input| input.is_empty()), "empty input");
    assert!(
        has(&|input| !input.is_empty() && input.trim().is_empty()),
        "whitespace-only input"
    );
    assert!(has(&|input| input == "/nope"), "an unknown alias");
    assert!(has(&|input| input == "/HELP"), "an alias in uppercase");
    assert!(
        has(&|input| !input.is_ascii() && input.to_lowercase().is_ascii()),
        "an alias spelled with a non-ASCII character whose Unicode lowercase is ASCII"
    );
}

#[test]
fn the_committed_corpus_replays_against_this_port() {
    let corpus = corpus();
    let contexts = port_contexts(&corpus);
    println!(
        "commands: divergence ledger ({} entries)",
        DIVERGENCES.len()
    );
    for (case, reason) in DIVERGENCES {
        println!("  {case}: {reason}");
    }
    let mut comparisons = 0;
    for family in FAMILIES {
        let mut report = Report::default();
        run_family(&corpus, &contexts, family, &mut report);
        comparisons += settle(&report, family.name);
    }
    println!(
        "commands: {comparisons} comparisons across {} families replayed at {}",
        FAMILIES.len(),
        &REFERENCE_COMMIT[..12],
    );
    assert!(
        comparisons >= MINIMUM_COMPARISONS,
        "the corpus replays {comparisons} comparisons, below the {MINIMUM_COMPARISONS} floor; \
         regenerate it with {CAPTURE_SCRIPT}"
    );
}

/// The corpus is only an oracle for as long as it still describes the pinned
/// reference. This probe recaptures it where the checkout is present and on the
/// pin, and skips everywhere else naming the pin and the way back.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "commands") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let script = repository.join(CAPTURE_SCRIPT);
    let recaptured = repository.join("target/commands-corpus.json");
    let output = Command::new("python3")
        .arg(&script)
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the commands capture script runs");
    assert!(
        output.status.success(),
        "the commands capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fresh = fs::read_to_string(&recaptured).expect("the recaptured corpus is readable");
    let committed =
        fs::read_to_string(repository.join(CORPUS_RELATIVE)).expect("the corpus is readable");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate it \
         with `{CAPTURE_SCRIPT}`"
    );
}
