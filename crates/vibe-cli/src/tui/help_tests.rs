//! What `/help` publishes, proved from the document builder, from the key
//! router that [`super::SHORTCUTS`] is the table of, and from
//! [`crate::tui::workflow::dispatch_command`], which is the entry point a
//! submitted `/help` reaches.

use super::{FEATURES, HEADINGS, SHORTCUTS, Shortcut, document};
use crate::Arguments;
use crate::tui::chat_input::ChatInputState;
use crate::tui::commands::{COMMANDS, CommandContext};
use crate::tui::completion::active_token;
use crate::tui::input::PromptEditor;
use crate::tui::shortcuts::{Chord, chord_of};
use crate::tui::state::{EntrySource, EntryStatus, TranscriptEntry, TranscriptKind, TuiState};
use crate::tui::submission::{Availability, Submission, classify};
use crate::tui::workflow::{CommandAction, dispatch_command};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeSet;
use std::path::Path;

/// The context `crates/vibe-cli/tests/commands/corpus.json` recorded the help
/// families under: every command available, nothing excluded.
fn full_context() -> CommandContext {
    CommandContext::new(true).with_clipboard_image_supported(true)
}

fn heading_level(line: &str) -> usize {
    line.chars()
        .take_while(|character| *character == '#')
        .count()
}

/// The section bodies, in order: the lines between one heading and the next.
fn sections(lines: &[&str]) -> Vec<Vec<String>> {
    let mut sections: Vec<Vec<String>> = Vec::new();
    for line in lines {
        if heading_level(line) > 0 {
            sections.push(Vec::new());
        } else if !line.is_empty()
            && let Some(current) = sections.last_mut()
        {
            current.push((*line).to_owned());
        }
    }
    sections
}

#[test]
fn the_document_carries_the_three_sections_the_corpus_recorded() {
    let text = document(&full_context());
    let lines = text.lines().collect::<Vec<_>>();

    assert_eq!(lines.len(), 46, "the corpus recorded 46 lines");
    assert_eq!(
        lines.iter().filter(|line| line.is_empty()).count(),
        5,
        "the corpus recorded 5 blank lines"
    );

    let headings = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| heading_level(line) > 0)
        .collect::<Vec<_>>();
    assert_eq!(
        headings.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
        vec![0, 11, 16],
        "the corpus recorded the headings at these offsets"
    );
    for (_, line) in &headings {
        assert_eq!(heading_level(line), 3, "{line} is not a level-3 heading");
    }
    assert_eq!(
        headings.iter().map(|(_, line)| **line).collect::<Vec<_>>(),
        HEADINGS.to_vec()
    );

    assert_eq!(
        sections(&lines).iter().map(Vec::len).collect::<Vec<_>>(),
        vec![SHORTCUTS.len(), FEATURES.len(), COMMANDS.len()],
        "the corpus recorded 8 shortcut lines, 2 prefix lines and one line per command"
    );
}

#[test]
fn the_command_section_is_sorted_by_registry_key_with_the_canonical_alias_first() {
    let text = document(&full_context());
    let lines = text.lines().collect::<Vec<_>>();
    let commands = lines[18..].to_vec();

    let mut expected = COMMANDS.iter().collect::<Vec<_>>();
    expected.sort_unstable_by_key(|command| command.name);
    assert_eq!(
        commands.len(),
        expected.len(),
        "every available command gets a line"
    );

    for (line, command) in commands.iter().zip(expected) {
        let (spans, description) = line
            .strip_prefix("- ")
            .and_then(|rest| rest.split_once(": "))
            .unwrap_or_else(|| panic!("{line} is not a command line"));
        assert_eq!(description, command.description);
        let aliases = spans
            .split(", ")
            .map(|span| {
                span.strip_prefix('`')
                    .and_then(|span| span.strip_suffix('`'))
                    .unwrap_or_else(|| panic!("{span} is not a code span"))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            aliases[0],
            format!("/{}", command.name),
            "{line} does not lead with the canonical alias"
        );
        let mut tail = aliases[1..].to_vec();
        tail.sort_unstable();
        assert_eq!(
            aliases[1..],
            tail[..],
            "{line} does not sort the remaining aliases"
        );
        assert_eq!(
            aliases.iter().copied().collect::<BTreeSet<_>>(),
            command.aliases.iter().copied().collect::<BTreeSet<_>>(),
            "{line} does not publish exactly this command's aliases"
        );
    }
}

#[test]
fn an_unavailable_command_loses_its_line_while_the_headings_stand() {
    let bare = document(&CommandContext::new(false).with_clipboard_image_supported(false));
    assert!(
        !bare.contains("`/paste-image`"),
        "no clipboard images means no /paste-image line"
    );
    assert!(
        !bare.contains("`/teleport`"),
        "no Vibe Code means no /teleport line"
    );

    let excluded = COMMANDS
        .iter()
        .map(|command| command.name)
        .collect::<Vec<_>>();
    let empty = document(&full_context().with_excluded(excluded));
    let lines = empty.lines().collect::<Vec<_>>();
    assert_eq!(
        lines
            .iter()
            .filter(|line| heading_level(line) > 0)
            .copied()
            .collect::<Vec<_>>(),
        HEADINGS.to_vec(),
        "a context with no command available still publishes the three headings"
    );
    assert!(
        sections(&lines)[2].is_empty(),
        "the command section is the only one that shrinks"
    );
    assert_eq!(sections(&lines)[0].len(), SHORTCUTS.len());
    assert_eq!(sections(&lines)[1].len(), FEATURES.len());
}

/// Every key a shortcut line advertises reaches the key router and comes back
/// as the chord that line names. A line naming a chord nothing binds fails
/// here, because the router resolves by this very table.
#[test]
fn every_advertised_key_routes_to_the_chord_its_line_names() {
    for shortcut in SHORTCUTS {
        for stroke in shortcut.keys {
            assert_eq!(
                chord_of(stroke.event()),
                Some(shortcut.chord),
                "{} advertises a key the router does not resolve to {:?}",
                shortcut.line(),
                shortcut.chord
            );
        }
    }
}

#[test]
fn a_key_no_line_advertises_names_no_chord() {
    assert_eq!(
        chord_of(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(
        chord_of(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        None
    );
}

/// A modifier the terminal reports but no chord distinguishes must not unbind
/// an advertised key. `Alt+Enter` is the case that matters: a terminal sends it
/// as an escape-prefixed `\r`, which crossterm reports as `Enter` carrying
/// `Alt`, and it submitted before this table became the router.
#[test]
fn an_advertised_key_still_routes_under_an_insignificant_modifier() {
    for modifier in [
        KeyModifiers::META,
        KeyModifiers::ALT,
        KeyModifiers::SUPER,
        KeyModifiers::HYPER,
    ] {
        let mut key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        key.modifiers |= modifier;
        assert_eq!(
            chord_of(key),
            Some(Chord::Submit),
            "{modifier:?} unbinds the submit key"
        );
    }
    assert_eq!(
        chord_of(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT | KeyModifiers::ALT
        )),
        Some(Chord::Newline),
        "Alt does not turn a shifted Enter back into a submit"
    );
}

/// The two modifiers that do tell one slot from another still do.
#[test]
fn the_two_distinguishing_modifiers_still_separate_the_slots() {
    assert_eq!(
        chord_of(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
        Some(Chord::Newline)
    );
    assert_eq!(
        chord_of(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(
        chord_of(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        None,
        "an unshifted Tab belongs to the composer"
    );
}

/// The two divergences the shortcut section carries, both rows of the
/// accepted-divergence table in `docs/parity.md`.
#[test]
fn the_shortcut_lines_state_what_this_binary_actually_binds() {
    let quit = SHORTCUTS
        .iter()
        .find(|shortcut| shortcut.chord == Chord::Quit)
        .map(Shortcut::line)
        .expect("the quit slot is declared");
    assert!(
        quit.contains("`Ctrl+D`"),
        "{quit} hides the Ctrl+D binding this port adds"
    );
    assert_eq!(
        chord_of(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
        Some(Chord::Quit)
    );

    let rewind = SHORTCUTS
        .last()
        .map(Shortcut::line)
        .expect("the rewind slot is declared");
    assert!(
        rewind.contains("empty prompt"),
        "{rewind} omits the condition this port's escape handler applies"
    );
}

/// The prefix lines name what the submission path and the completer accept, not
/// what the help wishes they accepted.
#[test]
fn the_prefix_lines_name_what_the_input_path_accepts() {
    let shell = FEATURES
        .iter()
        .find(|line| line.contains("shell"))
        .expect("a prefix line names the shell escape");
    assert!(shell.contains("`!<command>`"));
    assert_eq!(classify("!ls", None), Submission::Shell);

    let path = FEATURES
        .iter()
        .find(|line| line.contains("path"))
        .expect("a prefix line names the path completion");
    assert!(path.contains("`@path/to/file`"));
    let mut editor = PromptEditor::default();
    editor.set_text("@src");
    assert_eq!(
        active_token(&editor).map(|(_, query)| query),
        Some("@src".to_owned())
    );
}

#[tokio::test]
async fn submitting_help_writes_the_document_into_the_transcript_without_an_overlay() {
    let arguments =
        <Arguments as clap::Parser>::try_parse_from(["vibe"]).expect("interactive arguments");
    let mut state = TuiState::new("session");
    let mut composer = ChatInputState::new();
    composer.set_command_context(full_context());
    let mut runtime = None;

    let action = dispatch_command(
        "/help",
        &arguments,
        Path::new("/workspace"),
        &mut runtime,
        &mut state,
        &mut composer,
        Availability::Idle,
    )
    .await;

    assert!(matches!(action, CommandAction::Handled));
    assert!(
        state.overlay.is_none(),
        "the reference mounts a message, not a modal"
    );
    assert_eq!(
        state.entries.len(),
        2,
        "the submitted line is echoed above the document"
    );
    assert_eq!(state.entries[0].kind, TranscriptKind::Command);
    assert_eq!(state.entries[0].text, "help");
    let entry = &state.entries[1];
    assert_eq!(entry.kind, TranscriptKind::Document);
    assert_eq!(entry.text, document(&full_context()));
}

#[tokio::test]
async fn the_help_document_survives_a_canonical_resync() {
    let arguments =
        <Arguments as clap::Parser>::try_parse_from(["vibe"]).expect("interactive arguments");
    let mut state = TuiState::new("session");
    let mut composer = ChatInputState::new();
    composer.set_command_context(full_context());
    let mut runtime = None;
    dispatch_command(
        "/help",
        &arguments,
        Path::new("/workspace"),
        &mut runtime,
        &mut state,
        &mut composer,
        Availability::Idle,
    )
    .await;

    let mut replacement = TuiState::new("session");
    replacement.entries.push(TranscriptEntry {
        id: "canonical".to_owned(),
        revision: 1,
        kind: TranscriptKind::UserMessage,
        text: "hello".to_owned(),
        status: EntryStatus::Completed,
        source: EntrySource::Restored,
    });
    state
        .replace_projection_preserving_diagnostics(replacement)
        .expect("same-session replacement");

    let documents = state
        .entries
        .iter()
        .filter(|entry| entry.kind == TranscriptKind::Document)
        .count();
    assert_eq!(
        documents, 1,
        "the document is neither dropped nor duplicated"
    );
}
