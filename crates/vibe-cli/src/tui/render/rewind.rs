use super::overlays::{draw_panel, panel_area, panel_width};
use super::*;

pub(super) fn draw_rewind(frame: &mut Frame<'_>, rewind: &RewindState, theme: ResolvedTheme) {
    let (width, inner_width) = panel_width(frame.area(), 86);
    let target = rewind.target();
    let (position, count) = rewind.target_position();
    let mut lines = Vec::new();
    lines.push(Line::styled(
        format!("Message {position} of {count}"),
        theme.muted(),
    ));
    lines.push(Line::styled(
        truncate_width(
            &sanitize_inline(target.message.lines().next().unwrap_or_default()),
            inner_width,
        ),
        theme.base().add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::default());
    for (index, action) in rewind.actions().iter().enumerate() {
        let selected = *action == rewind.selected_action();
        let label = match action {
            RewindChoice::EditAndRestore => "Edit & restore files to this point",
            RewindChoice::EditOnly if target.has_file_changes => "Edit without restoring files",
            RewindChoice::EditOnly => "Edit message from here",
            RewindChoice::InPlace => "Stay in this session",
            RewindChoice::Fork => "Fork into a new session",
        };
        let style = if selected {
            theme.secondary().add_modifier(Modifier::BOLD)
        } else {
            theme.base()
        };
        lines.push(Line::styled(
            format!(
                "{} {}. {label}",
                if selected { "›" } else { " " },
                index + 1
            ),
            style,
        ));
    }
    if !target.has_file_changes && !rewind.choosing_persistence() {
        lines.push(Line::styled(
            "No file changes need restoration at this point.",
            theme.muted(),
        ));
    }
    if let Some(error) = rewind.error() {
        lines.push(Line::default());
        lines.push(Line::styled(error.to_owned(), theme.warning()));
    }
    lines.push(Line::default());
    lines.push(Line::styled(
        if rewind.choosing_persistence() {
            "↑↓/jk choose  Enter confirm  Esc back  q cancel"
        } else {
            "←/Esc previous  → next  Shift+↑↓ scroll  ↑↓/jk choose  Enter accept  q cancel"
        },
        theme.muted(),
    ));
    let visual_rows = lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(inner_width.max(1)))
        .sum::<usize>();
    let area = panel_area(
        frame.area(),
        width,
        u16::try_from(visual_rows)
            .unwrap_or(u16::MAX)
            .saturating_add(2),
    );
    draw_panel(frame, area, "Edit an earlier message", lines, theme);
}
