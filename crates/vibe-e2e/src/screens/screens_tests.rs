use super::*;

fn masks() -> Masks {
    let mut masks = Masks::default();
    masks.tokens.push(("/tmp/w-1/work".to_owned(), "<work>"));
    masks
}

#[test]
fn a_row_keeps_its_text_only_when_every_letter_on_it_is_authored() {
    let screen = Screen::parse(40, 3, b"> hello there\r\nsomething the subject wrote\r\n");
    let view = normalized(&screen, &[], &["hello there"], &masks(), "");
    assert_eq!(view["rows"][0], json!({"text": "> hello there"}));
    assert_eq!(view["rows"][1]["length"], json!(27));
    assert_eq!(view["rows"][1]["sha256"].as_str().map(str::len), Some(64));
    assert!(view["rows"][1].get("text").is_none());
    assert_eq!(view["rows"][2], json!({"text": ""}));
    assert_eq!(view["size"], json!([40, 3]));
}

#[test]
fn braille_cells_and_the_drawn_cursor_are_masked() {
    let first = Screen::parse(
        20,
        2,
        "\u{2860}\u{28e2} x\r\n\x1b[7;48;5;0m \x1b[m\x1b[2;1H".as_bytes(),
    );
    let second = Screen::parse(20, 2, "\u{2814}\u{2884} x\r\n \x1b[2;1H".as_bytes());
    let left = normalized(&first, &[], &[], &masks(), "");
    let right = normalized(&second, &[], &[], &masks(), "");
    assert_eq!(left, right, "{left:#}\n{right:#}");
    assert_eq!(left["cursor"], json!({"row": 1, "col": 0, "visible": true}));
    let hidden = Screen::parse(20, 2, b"x\x1b[?25l");
    assert_eq!(
        normalized(&hidden, &[], &[], &masks(), "")["cursor"],
        json!({"visible": false})
    );
}

#[test]
fn attribute_runs_reduce_to_classes() {
    let screen = Screen::parse(
        20,
        2,
        b"\x1b[1;38;5;3mab\x1b[38;2;1;2;3mcd\x1b[m e\x1b[2;5H",
    );
    let view = normalized(&screen, &[], &[], &masks(), "");
    assert_eq!(
        view["attributes"],
        json!([{"row": 0, "runs": [[0, 2, "fg:palette bold"], [2, 2, "fg:rgb bold"]]}])
    );
}

#[test]
fn world_paths_become_tokens_with_one_separator_spelling() {
    let mut masks = masks();
    masks.tokens.push((r"C:\tmp\w-1\work".to_owned(), "<work>"));
    let screen = Screen::parse(40, 2, br"C:\tmp\w-1\work\src x");
    let view = normalized(&screen, &[], &["src", "x"], &masks, "");
    assert_eq!(view["rows"][0], json!({"text": "<work>/src x"}));
}

#[test]
fn titles_drop_the_program_and_read_as_a_set() {
    let screen = Screen::parse(10, 1, b"");
    let titles = ["/bin/vibe", "Vibe", "Vibe", ">> Vibe", "Vibe"].map(str::to_owned);
    let view = normalized(&screen, &titles, &[], &masks(), "/bin/vibe");
    assert_eq!(view["titles"], json!([">> Vibe", "Vibe"]));
}

#[test]
fn an_animation_that_moves_its_blanks_reads_the_same() {
    let first = Screen::parse(
        30,
        1,
        " \u{2820}\u{2822}    \u{2860}\u{2840}   Name v1".as_bytes(),
    );
    let second = Screen::parse(
        30,
        1,
        "  \u{2860}\u{28d2}\u{2804}  \u{2814}\u{2844}   Name v1".as_bytes(),
    );
    let row = |screen: &Screen| masked_row(screen, 0, &masks()).0;
    assert_eq!(row(&first), row(&second));
    assert_eq!(row(&first), format!("{}Name v1", "\u{2591}".repeat(12)));
}

#[test]
fn the_first_difference_names_the_row_and_the_column() {
    let left = normalized(
        &Screen::parse(20, 2, b"\r\n> abc"),
        &[],
        &["abc", "abd"],
        &masks(),
        "",
    );
    let right = normalized(
        &Screen::parse(20, 2, b"\r\n> abd"),
        &[],
        &["abc", "abd"],
        &masks(),
        "",
    );
    let found = first_difference(&left, &right).unwrap();
    assert!(found.starts_with("row 1, column 4"), "{found}");
    assert_eq!(first_difference(&left, &left), None);
    assert!(render(&left).contains("01|> abc"));
}

#[test]
fn a_script_describes_its_steps_for_the_corpus() {
    let script = Script {
        name: "family/name",
        size: (120, 40),
        arguments: vec![],
        authored: vec!["stand-in"],
        steps: vec![
            Step::WaitFor("/help"),
            Step::Type("hi"),
            Step::Submit,
            Step::Keys(vec![Key::Ctrl('c'), Key::Up]),
            Step::Snapshot("after"),
        ],
    };
    assert_eq!(
        script.describe()["steps"],
        json!([{"waitFor": "/help"}, {"type": "hi"}, "submit", {"keys": ["ctrl+c", "up"]}, {"snapshot": "after"}])
    );
    assert_eq!(script.authored(), vec!["stand-in", "hi"]);
}
