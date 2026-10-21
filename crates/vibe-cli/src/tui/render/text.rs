//! Terminal-safe text: sanitizing untrusted output, wrapping it to the
//! viewport, and truncating a single line to a display width.
//!
//! Every string the transcript renders passes through here, so control
//! sequences a tool emitted can never reach the terminal and a pathological
//! payload can never grow the frame without bound.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const MAX_RENDER_CHARS: usize = 64 * 1024;
pub const MAX_RENDER_LINE_CHARS: usize = 4 * 1024;
pub(super) const MAX_RENDER_LINES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderLimits {
    pub max_chars: usize,
    pub max_line_chars: usize,
}

impl Default for RenderLimits {
    fn default() -> Self {
        Self {
            max_chars: MAX_RENDER_CHARS,
            max_line_chars: MAX_RENDER_LINE_CHARS,
        }
    }
}

pub(super) fn wrapped_terminal_lines(input: &str, width: usize) -> Vec<String> {
    let sanitized = sanitize_terminal(input, RenderLimits::default());
    let mut wrapped = Vec::new();
    for logical_line in sanitized.split('\n') {
        wrap_visual_line(logical_line, width.max(1), &mut wrapped);
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

pub(super) fn wrap_visual_line(line: &str, width: usize, output: &mut Vec<String>) {
    if line.is_empty() {
        output.push(String::new());
        return;
    }
    let mut current = String::new();
    let mut current_width = 0usize;
    for grapheme in line.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        if current_width > 0 && current_width.saturating_add(grapheme_width) > width {
            output.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push_str(grapheme);
        current_width = current_width.saturating_add(grapheme_width);
    }
    if !current.is_empty() {
        output.push(current);
    }
}

#[must_use]
pub fn sanitize_terminal(input: &str, limits: RenderLimits) -> String {
    let mut output = String::new();
    let mut total = 0usize;
    let mut line = 0usize;
    let mut truncated = false;
    for character in input.chars() {
        if total >= limits.max_chars {
            truncated = true;
            break;
        }
        if character == '\n' {
            output.push('\n');
            total += 1;
            line = 0;
            continue;
        }
        if line >= limits.max_line_chars {
            if !output.ends_with('…') {
                output.push('…');
                total += 1;
            }
            continue;
        }
        match character {
            '\t' => {
                output.push_str("    ");
                total = total.saturating_add(4);
                line = line.saturating_add(4);
            }
            '\u{1b}' => {
                output.push('␛');
                total += 1;
                line += 1;
            }
            character if character.is_control() => {
                output.push('�');
                total += 1;
                line += 1;
            }
            character => {
                output.push(character);
                total += 1;
                line += 1;
            }
        }
    }
    if truncated {
        output.push_str("\n… content truncated …");
    }
    output
}

pub(super) fn sanitize_inline(input: &str) -> String {
    sanitize_terminal(
        input,
        RenderLimits {
            max_chars: MAX_RENDER_LINE_CHARS,
            max_line_chars: MAX_RENDER_LINE_CHARS,
        },
    )
    .replace('\n', " ")
}

pub(super) fn truncate_width(input: &str, width: usize) -> String {
    if UnicodeWidthStr::width(input) <= width {
        return input.to_owned();
    }
    let mut output = String::new();
    for grapheme in input.graphemes(true) {
        let next = UnicodeWidthStr::width(output.as_str())
            .saturating_add(UnicodeWidthStr::width(grapheme));
        if next.saturating_add(1) > width {
            break;
        }
        output.push_str(grapheme);
    }
    if width > 0 {
        output.push('…');
    }
    output
}

/// Reference `clean_output` (`vibe/cli/textual_ui/widgets/tool_widgets.py`):
/// each line keeps only what its last carriage return redrew, loses its escape
/// sequences, and loses every control character but the tab. Both the manual
/// shell's captured output and every tool result body are cleaned this way.
#[must_use]
pub(in crate::tui) fn clean_output(content: &str) -> String {
    content
        .replace("\r\n", "\n")
        .split('\n')
        .map(|line| {
            let written = line.trim_end_matches('\r');
            let redrawn = written.rsplit('\r').next().unwrap_or(written);
            strip_escapes(redrawn)
                .chars()
                .filter(
                    |character| !matches!(u32::from(*character), 0x00..=0x08 | 0x0b..=0x1f | 0x7f),
                )
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reference `_ANSI_ESCAPE`: an OSC ended by BEL or ST, a CSI with its final
/// byte, or a two-byte escape. An escape that is none of those is left for the
/// control filter, which drops the ESC alone.
fn strip_escapes(line: &str) -> String {
    let characters = line.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(line.len());
    let mut index = 0;
    while index < characters.len() {
        if characters[index] != '\u{1b}' {
            output.push(characters[index]);
            index += 1;
            continue;
        }
        let end = escape_end(&characters, index);
        match end {
            Some(end) => index = end,
            None => {
                output.push(characters[index]);
                index += 1;
            }
        }
    }
    output
}

/// Where the escape starting at `start` ends, exclusive, when it matches.
fn escape_end(characters: &[char], start: usize) -> Option<usize> {
    let introducer = *characters.get(start + 1)?;
    if introducer == ']' {
        let mut cursor = start + 2;
        while let Some(&character) = characters.get(cursor) {
            match character {
                '\u{7}' => return Some(cursor + 1),
                '\u{1b}' => {
                    if characters.get(cursor + 1) == Some(&'\\') {
                        return Some(cursor + 2);
                    }
                    break;
                }
                _ => cursor += 1,
            }
        }
        // An unterminated OSC still matches as the two-byte `ESC ]`.
        return Some(start + 2);
    }
    if introducer == '[' {
        let mut cursor = start + 2;
        while characters
            .get(cursor)
            .is_some_and(|character| ('0'..='?').contains(character))
        {
            cursor += 1;
        }
        while characters
            .get(cursor)
            .is_some_and(|character| (' '..='/').contains(character))
        {
            cursor += 1;
        }
        return characters
            .get(cursor)
            .filter(|character| ('@'..='~').contains(*character))
            .map(|_| cursor + 1);
    }
    (('@'..='Z').contains(&introducer) || ('\\'..='_').contains(&introducer)).then_some(start + 2)
}
