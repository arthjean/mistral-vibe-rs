//! The document `/help` writes into the transcript, and the chord table it is
//! built from.
//!
//! Reference `CommandRegistry.get_help_text` (`vibe/cli/commands.py:268`) builds
//! three `###` sections in one fixed order: the keyboard shortcuts, the input
//! prefixes, then one line per command sorted by registry key, each line listing
//! every alias of that command as a code span with the canonical `/name` first.
//! `_show_help` (`vibe/cli/textual_ui/app.py:2452`) mounts the result as a
//! Markdown message rather than opening a modal, which is what
//! [`TranscriptKind::Document`](crate::tui::state::TranscriptKind) reproduces.
//!
//! The structure is the contract; the prose is not. Every heading, shortcut line
//! and prefix line upstream is authored prose `NOTICE` forbids reproducing, so
//! the lines below are this port's own, covering the same directives.
//! `crates/vibe-cli/tests/commands/corpus.json` records the reference's as a
//! byte length and a SHA-256, and
//! `this_ports_help_prose_never_matches_a_reference_digest` holds the two
//! permanently apart.
//!
//! [`SHORTCUTS`] is not documentation about the key handler: it *is* the key
//! handler's binding table, since [`super::shortcuts::chord_of`] resolves every
//! global chord by looking a key event up in it. A help line therefore cannot
//! advertise a key this binary ignores, because the line's own key strokes are
//! what routes that key.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::commands::{COMMANDS, CommandContext, CommandDefinition, command_available_in};
use super::shortcuts::Chord;

/// One key event a help line advertises, and the name that line gives it.
pub(super) struct KeyStroke {
    code: KeyCode,
    modifiers: KeyModifiers,
    name: &'static str,
}

impl KeyStroke {
    const fn new(code: KeyCode, modifiers: KeyModifiers, name: &'static str) -> Self {
        Self {
            code,
            modifiers,
            name,
        }
    }

    /// Whether this stroke is the key that arrived.
    ///
    /// Only `Ctrl` and `Shift` are compared, because they are the two modifiers
    /// that tell two slots of this table apart: `Ctrl+J` from `j`, `Shift+Enter`
    /// from `Enter`, `Shift+Tab` from `Tab`. Every other bit a terminal reports
    /// rides along without unbinding the key, which is what keeps `Alt+Enter`
    /// submitting the way it did before this table became the router.
    fn binds(&self, key: KeyEvent) -> bool {
        const SIGNIFICANT: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::SHIFT);
        self.code == key.code && self.modifiers == key.modifiers.intersection(SIGNIFICANT)
    }

    /// The event this stroke names, which is what a test presses.
    #[cfg(test)]
    pub(super) fn event(&self) -> KeyEvent {
        KeyEvent::new(self.code, self.modifiers)
    }
}

/// One advertised shortcut: the keys it names, the chord they route to, and
/// what the line says they do.
pub(super) struct Shortcut {
    pub keys: &'static [KeyStroke],
    pub chord: Chord,
    text: &'static str,
}

impl Shortcut {
    /// The published line: every distinct key name as a code span, then the
    /// directive.
    pub(super) fn line(&self) -> String {
        let mut names: Vec<&str> = Vec::new();
        for stroke in self.keys {
            if !names.contains(&stroke.name) {
                names.push(stroke.name);
            }
        }
        let spans = names
            .into_iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(" / ");
        format!("- {spans} {}", self.text)
    }

    fn binds(&self, key: KeyEvent) -> bool {
        self.keys.iter().any(|stroke| stroke.binds(key))
    }
}

const CONTROL: KeyModifiers = KeyModifiers::CONTROL;
const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
const NONE: KeyModifiers = KeyModifiers::NONE;

/// The eight shortcut slots, in the reference's order.
///
/// Two of them say something the reference's do not, because this binary binds
/// something the reference does not: `Ctrl+D` rides on the quit line rather than
/// adding a ninth slot, and the rewind line states the empty-prompt condition
/// `super::shortcuts::escape` actually applies. Both are rows in the
/// accepted-divergence table of `docs/parity.md`.
pub(super) const SHORTCUTS: &[Shortcut] = &[
    Shortcut {
        keys: &[KeyStroke::new(KeyCode::Enter, NONE, "Enter")],
        chord: Chord::Submit,
        text: "Send the prompt",
    },
    Shortcut {
        keys: &[
            KeyStroke::new(KeyCode::Char('j'), CONTROL, "Ctrl+J"),
            KeyStroke::new(KeyCode::Enter, SHIFT, "Shift+Enter"),
        ],
        chord: Chord::Newline,
        text: "Start a new line",
    },
    Shortcut {
        keys: &[KeyStroke::new(KeyCode::Esc, NONE, "Escape")],
        chord: Chord::Escape,
        text: "Stop the agent or dismiss an overlay",
    },
    Shortcut {
        keys: &[
            KeyStroke::new(KeyCode::Char('c'), CONTROL, "Ctrl+C"),
            KeyStroke::new(KeyCode::Char('d'), CONTROL, "Ctrl+D"),
        ],
        chord: Chord::Quit,
        text: "Quit, or clear a non-empty prompt first",
    },
    Shortcut {
        keys: &[KeyStroke::new(KeyCode::Char('g'), CONTROL, "Ctrl+G")],
        chord: Chord::ExternalEditor,
        text: "Open the prompt in an external editor",
    },
    Shortcut {
        keys: &[KeyStroke::new(KeyCode::Char('o'), CONTROL, "Ctrl+O")],
        chord: Chord::ToggleTools,
        text: "Fold and unfold tool output",
    },
    Shortcut {
        // A terminal reports the chord as `BackTab`, with or without the shift
        // bit, and a few send a shifted `Tab` instead. All three are the one
        // key the line names.
        keys: &[
            KeyStroke::new(KeyCode::BackTab, SHIFT, "Shift+Tab"),
            KeyStroke::new(KeyCode::BackTab, NONE, "Shift+Tab"),
            KeyStroke::new(KeyCode::Tab, SHIFT, "Shift+Tab"),
        ],
        chord: Chord::CycleAgent,
        text: "Switch to the next agent",
    },
    Shortcut {
        keys: &[KeyStroke::new(KeyCode::Esc, NONE, "Esc Esc")],
        chord: Chord::Escape,
        text: "Rewind to an earlier message, on an empty prompt",
    },
];

/// Which chord a key event names, if any. [`super::shortcuts::chord_of`] is the
/// caller, and the running session routes by its answer.
pub(super) fn chord_for(key: KeyEvent) -> Option<Chord> {
    SHORTCUTS
        .iter()
        .find(|shortcut| shortcut.binds(key))
        .map(|shortcut| shortcut.chord)
}

/// The two prefix lines, naming what `super::submission::classify` and the path
/// completer actually accept.
pub(super) const FEATURES: &[&str] = &[
    "- `!<command>` Run a shell command without the agent",
    "- `@path/to/file` Complete a path from the workspace",
];

/// The three section headings, at the heading level the reference publishes.
pub(super) const HEADINGS: [&str; 3] = [
    "### Key Bindings",
    "### Input Prefixes",
    "### Available Commands",
];

/// Every line this port authors, which is the set the licensing guard holds
/// unequal to every reference digest.
///
/// The command lines are deliberately not among them: both halves of a command
/// line are the registry's own contract rather than prose, and `helpCommands` is
/// what compares them.
#[cfg(test)]
pub(super) fn authored_lines() -> Vec<String> {
    HEADINGS
        .iter()
        .copied()
        .map(str::to_owned)
        .chain(SHORTCUTS.iter().map(Shortcut::line))
        .chain(FEATURES.iter().copied().map(str::to_owned))
        .collect()
}

/// One command's line: every alias as a code span, the canonical `/name` first
/// and the rest sorted, then the description the registry publishes.
pub(super) fn command_line(command: &CommandDefinition) -> String {
    let canonical = format!("/{}", command.name);
    let mut aliases = command.aliases.to_vec();
    aliases.sort_unstable_by(|left, right| {
        (*left != canonical.as_str(), *left).cmp(&(*right != canonical.as_str(), *right))
    });
    let spans = aliases
        .into_iter()
        .map(|alias| format!("`{alias}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("- {spans}: {}", command.description)
}

/// The whole document, built once per invocation.
///
/// A context that leaves no command available still publishes the three
/// headings; only the command section shrinks.
pub(super) fn document(context: &CommandContext) -> String {
    let mut commands = COMMANDS
        .iter()
        .filter(|command| command_available_in(command, context))
        .collect::<Vec<_>>();
    commands.sort_unstable_by_key(|command| command.name);

    let mut lines = vec![HEADINGS[0].to_owned(), String::new()];
    lines.extend(SHORTCUTS.iter().map(Shortcut::line));
    lines.push(String::new());
    lines.push(HEADINGS[1].to_owned());
    lines.push(String::new());
    lines.extend(FEATURES.iter().copied().map(str::to_owned));
    lines.push(String::new());
    lines.push(HEADINGS[2].to_owned());
    lines.push(String::new());
    lines.extend(commands.into_iter().map(command_line));
    lines.join("\n")
}

#[cfg(test)]
#[path = "help_tests.rs"]
mod help_tests;
