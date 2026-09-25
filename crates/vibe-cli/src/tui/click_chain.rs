//! Multi-click selection shared by the composer and the transcript.
//!
//! Reference `WordSelectScreen` (`vibe/cli/textual_ui/word_selection.py`) and
//! `ChatTextArea._update_click_chain`
//! (`vibe/cli/textual_ui/widgets/chat_input/text_area.py`): presses on the same
//! spot within half a second climb from a character caret to the word under
//! the pointer, then to its whole line, and a fourth press starts over. A drag
//! that grew a word or line selection ends the chain.

/// Reference `CLICK_CHAIN_TIME_THRESHOLD`.
const CLICK_CHAIN_MS: u64 = 500;

/// What a press selects, and the unit a drag after it extends by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Granularity {
    #[default]
    Character,
    Word,
    Line,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClickChain<K> {
    count: u8,
    last: Option<(K, u64)>,
    granularity: Granularity,
    dragged: bool,
}

impl<K: PartialEq + Clone> ClickChain<K> {
    /// Records a press at `at` and answers what it selects.
    pub fn press(&mut self, at: K, now_ms: u64) -> Granularity {
        let chained = !std::mem::take(&mut self.dragged)
            && self.last.as_ref().is_some_and(|(spot, time)| {
                *spot == at && now_ms.saturating_sub(*time) <= CLICK_CHAIN_MS
            });
        self.count = if chained && self.count < 3 {
            self.count + 1
        } else {
            1
        };
        self.last = Some((at, now_ms));
        self.granularity = match self.count {
            2 => Granularity::Word,
            3 => Granularity::Line,
            _ => Granularity::Character,
        };
        self.granularity
    }

    /// Where the last press landed, which anchors a word or line drag.
    #[must_use]
    pub fn anchor(&self) -> Option<&K> {
        self.last.as_ref().map(|(spot, _)| spot)
    }

    #[must_use]
    pub const fn granularity(&self) -> Granularity {
        self.granularity
    }

    /// A word or line drag changed the selection, so the next press starts a
    /// fresh chain.
    pub fn mark_dragged(&mut self) {
        self.dragged = true;
    }
}

/// The word around position `at` in a line of `len` cells: the word cells
/// ending at `at` plus those starting there. Reference `_boundary_around`,
/// whose `\w` is a letter, a digit or an underscore.
#[must_use]
pub fn word_span(len: usize, at: usize, is_word: impl Fn(usize) -> bool) -> Option<(usize, usize)> {
    let at = at.min(len);
    let start = (0..at).rev().take_while(|&index| is_word(index)).count();
    let end = (at..len).take_while(|&index| is_word(index)).count();
    (start + end > 0).then_some((at - start, at + end))
}

#[must_use]
pub fn is_word_character(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

#[cfg(test)]
mod click_chain_tests;
