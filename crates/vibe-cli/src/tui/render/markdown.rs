use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{RenderLimits, sanitize_terminal, wrapped_terminal_lines};
use crate::tui::setup::ResolvedTheme;

pub(super) fn markdown_lines(
    input: &str,
    width: usize,
    theme: ResolvedTheme,
) -> Vec<Line<'static>> {
    let sanitized = sanitize_terminal(input, RenderLimits::default());
    let mut output = Vec::new();
    let mut fenced = false;
    for raw in sanitized.lines() {
        let trimmed = raw.trim_start();
        if let Some(language) = trimmed.strip_prefix("```") {
            if fenced {
                output.push(Line::styled("  └─", theme.muted()));
            } else {
                let label = language.trim();
                output.push(Line::styled(
                    if label.is_empty() {
                        "  ┌─ code".to_owned()
                    } else {
                        format!("  ┌─ {label}")
                    },
                    theme.muted(),
                ));
            }
            fenced = !fenced;
            continue;
        }
        if fenced {
            push_markdown_wrapped(
                &mut output,
                raw,
                "  │ ",
                width,
                Style::default(),
                theme,
                false,
            );
            continue;
        }
        if raw.trim().is_empty() {
            output.push(Line::default());
            continue;
        }
        if is_markdown_table_separator(trimmed) {
            continue;
        }
        if let Some(heading) = markdown_heading(trimmed) {
            push_markdown_wrapped(
                &mut output,
                heading,
                "  ",
                width,
                theme.secondary().add_modifier(Modifier::BOLD),
                theme,
                true,
            );
            continue;
        }
        if let Some(item) = markdown_bullet(trimmed) {
            push_markdown_wrapped(&mut output, item, "  • ", width, theme.base(), theme, true);
            continue;
        }
        if let Some(quote) = trimmed.strip_prefix('>') {
            push_markdown_wrapped(
                &mut output,
                quote.trim_start(),
                "  │ ",
                width,
                theme.muted().add_modifier(Modifier::ITALIC),
                theme,
                true,
            );
            continue;
        }
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            let cells = trimmed
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" │ ");
            push_markdown_wrapped(&mut output, &cells, "  ", width, theme.base(), theme, true);
            continue;
        }
        if trimmed
            .chars()
            .all(|character| matches!(character, '-' | '_' | '*'))
            && trimmed.len() >= 3
        {
            output.push(Line::styled(
                format!("  {}", "─".repeat(width.saturating_sub(2))),
                theme.muted(),
            ));
            continue;
        }
        push_markdown_wrapped(&mut output, raw, "  ", width, theme.base(), theme, true);
    }
    if fenced {
        output.push(Line::styled("  └─", theme.muted()));
    }
    if output.is_empty() {
        output.push(Line::raw("  "));
    }
    output
}

fn push_markdown_wrapped(
    output: &mut Vec<Line<'static>>,
    content: &str,
    first_prefix: &str,
    width: usize,
    style: Style,
    theme: ResolvedTheme,
    parse_inline: bool,
) {
    let prefix_width = UnicodeWidthStr::width(first_prefix);
    let content_width = width.saturating_sub(prefix_width).max(1);
    let wrapped_lines = if parse_inline {
        wrap_inline_spans(inline_markdown_spans(content, style, theme), content_width)
    } else {
        wrapped_terminal_lines(content, content_width)
            .into_iter()
            .map(|line| vec![Span::styled(line, style)])
            .collect()
    };
    for (index, content_spans) in wrapped_lines.into_iter().enumerate() {
        let prefix = if index == 0 {
            first_prefix.to_owned()
        } else {
            " ".repeat(prefix_width)
        };
        let mut spans = vec![Span::styled(prefix, style)];
        spans.extend(content_spans);
        output.push(Line::from(spans));
    }
}

fn wrap_inline_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Vec<Span<'static>>> {
    let mut output = Vec::new();
    let mut current = Vec::<(String, Style)>::new();
    let mut current_width = 0usize;
    for span in spans {
        let style = span.style;
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
            if current_width > 0 && current_width.saturating_add(grapheme_width) > width {
                output.push(styled_line(std::mem::take(&mut current)));
                current_width = 0;
            }
            if let Some((content, current_style)) = current.last_mut()
                && *current_style == style
            {
                content.push_str(grapheme);
            } else {
                current.push((grapheme.to_owned(), style));
            }
            current_width = current_width.saturating_add(grapheme_width);
        }
    }
    if !current.is_empty() {
        output.push(styled_line(current));
    }
    if output.is_empty() {
        output.push(Vec::new());
    }
    output
}

fn styled_line(spans: Vec<(String, Style)>) -> Vec<Span<'static>> {
    spans
        .into_iter()
        .map(|(content, style)| Span::styled(content, style))
        .collect()
}

fn inline_markdown_spans(input: &str, style: Style, theme: ResolvedTheme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        let markers = ["\\", "`", "**", "__", "*", "_", "["];
        let next = markers
            .iter()
            .filter_map(|marker| rest.find(marker).map(|index| (index, *marker)))
            .min_by_key(|(index, _)| *index);
        let Some((index, marker)) = next else {
            spans.push(Span::styled(rest.to_owned(), style));
            break;
        };
        if index > 0 {
            spans.push(Span::styled(rest[..index].to_owned(), style));
            rest = &rest[index..];
            continue;
        }
        if marker == "\\" {
            let mut characters = rest[1..].chars();
            if let Some(escaped) = characters
                .next()
                .filter(|character| "\\`*_[]()".contains(*character))
            {
                spans.push(Span::styled(escaped.to_string(), style));
                rest = &rest[1 + escaped.len_utf8()..];
            } else {
                spans.push(Span::styled("\\".to_owned(), style));
                rest = &rest[1..];
            }
            continue;
        }
        if marker == "["
            && let Some(close) = rest.find("](")
        {
            let after = &rest[close + 2..];
            if let Some(end) = after.find(')') {
                let label = &rest[1..close];
                let target = &after[..end];
                spans.push(Span::styled(
                    label.to_owned(),
                    theme.secondary().add_modifier(Modifier::UNDERLINED),
                ));
                if target != label {
                    spans.push(Span::styled(format!(" ({target})"), theme.muted()));
                }
                rest = &after[end + 1..];
                continue;
            }
        }
        let (delimiter, modifier) = match marker {
            "`" => ("`", Modifier::REVERSED),
            "**" => ("**", Modifier::BOLD),
            "__" => ("__", Modifier::BOLD),
            "*" => ("*", Modifier::ITALIC),
            "_" => ("_", Modifier::ITALIC),
            _ => {
                spans.push(Span::styled(rest[..1].to_owned(), style));
                rest = &rest[1..];
                continue;
            }
        };
        let after = &rest[delimiter.len()..];
        if let Some(end) = find_unescaped(after, delimiter) {
            spans.push(Span::styled(
                after[..end].to_owned(),
                style.add_modifier(modifier),
            ));
            rest = &after[end + delimiter.len()..];
        } else {
            spans.push(Span::styled(delimiter.to_owned(), style));
            rest = after;
        }
    }
    spans
}

fn find_unescaped(input: &str, needle: &str) -> Option<usize> {
    input
        .match_indices(needle)
        .find(|(index, _)| {
            input[..*index]
                .chars()
                .rev()
                .take_while(|character| *character == '\\')
                .count()
                % 2
                == 0
        })
        .map(|(index, _)| index)
}

fn markdown_heading(line: &str) -> Option<&str> {
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    (hashes > 0 && hashes <= 6)
        .then(|| line.get(hashes..)?.strip_prefix(' '))
        .flatten()
}

fn markdown_bullet(line: &str) -> Option<&str> {
    ["- ", "* ", "+ "]
        .into_iter()
        .find_map(|prefix| line.strip_prefix(prefix))
}

fn is_markdown_table_separator(line: &str) -> bool {
    let trimmed = line.trim_matches('|').trim();
    !trimmed.is_empty()
        && trimmed
            .split('|')
            .all(|cell| cell.trim().trim_matches(':').chars().all(|c| c == '-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::setup::Theme;

    #[test]
    fn inline_styles_survive_wrapping() {
        let lines = markdown_lines(
            "**abcdefgh**",
            4,
            ResolvedTheme {
                theme: Theme::Dark,
                colors_enabled: true,
            },
        );

        assert_eq!(lines.len(), 4);
        assert!(lines.iter().all(|line| line.width() <= 4));
        let content = lines
            .iter()
            .flat_map(|line| line.spans.iter().skip(1))
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(content, "abcdefgh");
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter().skip(1))
                .all(|span| span.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn escaped_inline_delimiters_remain_literal() {
        let lines = markdown_lines(
            r"\*literal\* and **bold**",
            80,
            ResolvedTheme {
                theme: Theme::Dark,
                colors_enabled: true,
            },
        );
        let content = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(content, "  *literal* and bold");
        assert!(
            !lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
        );
    }
}
