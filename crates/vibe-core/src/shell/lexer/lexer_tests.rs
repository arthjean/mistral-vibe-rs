use super::*;

fn posix(text: &str) -> Option<Vec<String>> {
    split_words(text, WordSplit::Posix)
}

fn windows(text: &str) -> Option<Vec<String>> {
    split_words(text, WordSplit::LiteralBackslash)
}

fn words(list: &[&str]) -> Option<Vec<String>> {
    Some(list.iter().map(|word| (*word).to_owned()).collect())
}

#[test]
fn quotes_group_and_escapes_apply_on_posix() {
    assert_eq!(
        posix("grep 'a b' \"c d\" e\\ f"),
        words(&["grep", "a b", "c d", "e f"])
    );
    assert_eq!(
        posix(r#"echo "a\"b" "a\nb""#),
        words(&["echo", "a\"b", "a\\nb"])
    );
    assert_eq!(posix("echo '' x"), words(&["echo", "", "x"]));
    assert_eq!(posix("echo #x"), words(&["echo", "#x"]));
}

#[test]
fn an_unclosed_quote_or_escape_answers_none() {
    assert_eq!(posix("cat 'unterminated"), None);
    assert_eq!(posix("cat trailing\\"), None);
}

#[test]
fn the_windows_lexer_keeps_backslashes_and_reads_comments() {
    assert_eq!(
        windows(r"type C:\Users\me\notes.txt"),
        words(&["type", r"C:\Users\me\notes.txt"])
    );
    assert_eq!(windows("dir a#b c"), words(&["dir", "a"]));
    assert_eq!(windows("dir # c\nd"), words(&["dir", "d"]));
}
