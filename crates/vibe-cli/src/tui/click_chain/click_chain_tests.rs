use super::*;

#[test]
fn presses_on_one_spot_climb_to_a_line_and_start_over() {
    let mut chain = ClickChain::default();
    assert_eq!(chain.press((3, 1), 0), Granularity::Character);
    assert_eq!(chain.press((3, 1), 200), Granularity::Word);
    assert_eq!(chain.press((3, 1), 400), Granularity::Line);
    assert_eq!(chain.press((3, 1), 600), Granularity::Character);
    assert_eq!(chain.press((3, 1), 700), Granularity::Word);
}

#[test]
fn a_pause_a_move_or_a_drag_breaks_the_chain() {
    let mut chain = ClickChain::default();
    chain.press((3, 1), 0);
    assert_eq!(chain.press((3, 1), 501), Granularity::Character);
    assert_eq!(chain.press((4, 1), 600), Granularity::Character);
    assert_eq!(chain.press((4, 1), 700), Granularity::Word);
    chain.mark_dragged();
    assert_eq!(chain.press((4, 1), 800), Granularity::Character);
}

#[test]
fn a_word_spans_both_sides_of_the_press() {
    let text: Vec<char> = "say hello_42, now".chars().collect();
    let word = |index: usize| is_word_character(text[index]);
    assert_eq!(word_span(text.len(), 6, word), Some((4, 12)));
    assert_eq!(word_span(text.len(), 12, word), Some((4, 12)));
    assert_eq!(word_span(text.len(), 13, word), None);
    assert_eq!(word_span(text.len(), 99, word), Some((14, 17)));
}
