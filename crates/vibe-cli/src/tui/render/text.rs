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
