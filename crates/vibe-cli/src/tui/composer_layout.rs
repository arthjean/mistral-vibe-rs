use std::sync::Arc;

use super::input::{PromptEditor, VisualLayout, composer_content_width};

pub(crate) const CHROME_HEIGHT: u16 = 2;
pub(crate) const PROMPT_WIDTH: u16 = 2;
const MIN_BODY_HEIGHT: u16 = 3;
const SCROLLBAR_WIDTH: usize = 2;

/// Canonical geometry shared by reducer observations, rendering and mouse input.
pub(crate) struct ComposerLayout {
    width: usize,
    input_height: u16,
    body_height: u16,
    visual: Arc<VisualLayout>,
    cursor_row: usize,
    cursor_column: usize,
    scroll: usize,
}

impl ComposerLayout {
    #[must_use]
    pub(crate) fn for_viewport(
        editor: &PromptEditor,
        viewport_width: u16,
        viewport_height: u16,
        prefix: usize,
    ) -> Self {
        Self::for_content_width(
            editor,
            composer_content_width(viewport_width),
            viewport_height,
            prefix,
        )
    }

    #[must_use]
    pub(crate) fn for_content_width(
        editor: &PromptEditor,
        content_width: usize,
        viewport_height: u16,
        prefix: usize,
    ) -> Self {
        let content_width = content_width.max(1);
        let available_body = viewport_height.saturating_sub(CHROME_HEIGHT).max(1);
        let maximum_body = viewport_height
            .saturating_div(2)
            .max(MIN_BODY_HEIGHT)
            .min(available_body);
        let base = editor.visual_layout(content_width, prefix);
        let width = if base.lines().len() > usize::from(maximum_body) {
            content_width.saturating_sub(SCROLLBAR_WIDTH).max(1)
        } else {
            content_width
        };
        let visual = if width == content_width {
            base
        } else {
            editor.visual_layout(width, prefix)
        };
        let desired_body = u16::try_from(visual.lines().len())
            .unwrap_or(u16::MAX)
            .max(MIN_BODY_HEIGHT.min(available_body))
            .min(maximum_body);
        let input_height = desired_body
            .saturating_add(CHROME_HEIGHT)
            .min(viewport_height.max(1));
        let body_height = input_height.saturating_sub(CHROME_HEIGHT);
        let (cursor_row, cursor_column) = visual.cursor_cell(editor.text(), editor.cursor());
        let scroll = cursor_row.saturating_sub(usize::from(body_height.saturating_sub(1)));
        Self {
            width,
            input_height,
            body_height,
            visual,
            cursor_row,
            cursor_column,
            scroll,
        }
    }

    #[must_use]
    pub(crate) const fn width(&self) -> usize {
        self.width
    }

    #[must_use]
    pub(crate) const fn input_height(&self) -> u16 {
        self.input_height
    }

    #[must_use]
    pub(crate) const fn body_height(&self) -> u16 {
        self.body_height
    }

    #[must_use]
    pub(crate) fn visual(&self) -> &VisualLayout {
        &self.visual
    }

    #[must_use]
    pub(crate) const fn cursor_row(&self) -> usize {
        self.cursor_row
    }

    #[must_use]
    pub(crate) const fn cursor_column(&self) -> usize {
        self.cursor_column
    }

    #[must_use]
    pub(crate) const fn scroll(&self) -> usize {
        self.scroll
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn textual_wrap_and_terminal_cells_share_the_canonical_layout() {
        let mut editor = PromptEditor::default();
        editor.set_text("a bbbbb\tcc");
        let layout = ComposerLayout::for_content_width(&editor, 5, 6, 0);

        assert_eq!(layout.width(), 3);
        assert_eq!(layout.cursor_row(), layout.visual().lines().len() - 1);
        assert_eq!(
            layout.cursor_column(),
            layout.visual().cell_width(
                editor.text(),
                layout.visual().lines()[layout.cursor_row()].start..editor.cursor(),
            )
        );

        let mut tab = PromptEditor::default();
        tab.set_text("a\t");
        let tab_layout = ComposerLayout::for_content_width(&tab, 4, 6, 0);
        assert_eq!(tab_layout.visual().lines(), &[0..1, 1..2]);
        assert_eq!(
            [tab_layout.cursor_row(), tab_layout.cursor_column()],
            [1, 4]
        );
    }

    #[test]
    fn incremental_tail_layout_matches_a_full_unicode_rebuild() {
        let mut editor = PromptEditor::default();
        editor.set_text("alpha beta");
        for width in [3, 5, 9] {
            let _ = editor.visual_layout(width, 0);
        }

        for fragment in ["x", "\u{301}", "\nnext\t", "👩\u{200d}💻"] {
            editor.insert(fragment);
            let mut rebuilt = PromptEditor::default();
            rebuilt.set_text(editor.text());
            assert_eq!(editor.cursor(), rebuilt.cursor());
            for width in [3, 5, 9] {
                let incremental = editor.visual_layout(width, 0);
                let full = rebuilt.visual_layout(width, 0);
                assert_eq!(incremental.lines(), full.lines(), "width {width}");
                assert_eq!(
                    incremental.text_lines(editor.text()),
                    full.text_lines(rebuilt.text()),
                    "width {width}"
                );
            }
        }
    }
}
