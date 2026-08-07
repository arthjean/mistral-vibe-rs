//! Shell command extraction, on the grammar the reference parses with.
//!
//! Reference `vibe/core/tools/builtins/bash.py::_extract_commands` walks the
//! `tree-sitter-bash` parse tree and rebuilds, per `command` node, the words it
//! is made of. A tokenizer cannot answer the same question: it has no notion of
//! a heredoc, so `python3 <<'EOF' ... EOF` reads as a bare standalone `python3`
//! and is refused by a list that was never meant to cover it. The grammar
//! answers instead, and the redirect it sees becomes an explicit marker on the
//! segment.
//!
//! The extraction is deliberately partial in the same places the reference is:
//! an assignment prefix, a loop header and a redirect target are not `command`
//! nodes, so none of the three reaches the policy. What the policy never sees is
//! what an approval never covers, which is why an empty extraction defers to the
//! configured permission rather than granting.

use std::cell::RefCell;

use tree_sitter::{Node, Parser};

/// The node kinds whose text composes a segment.
///
/// Reference `_extract_commands` collects exactly these five kinds among a
/// `command` node's direct children, which is what keeps a redirect target and
/// an assignment prefix out of the segment text.
const SEGMENT_KINDS: [&str; 5] = [
    "command_name",
    "word",
    "string",
    "raw_string",
    "concatenation",
];

/// The word appended to a segment whose command node sits under a redirect.
///
/// Reference `_extract_commands` appends it so the segment stops being a single
/// word, which is the only thing the standalone denylist looks at.
pub const REDIRECT_MARKER: &str = "<redirect>";

/// The node kind wrapping a command whose output or input is redirected.
const REDIRECTED_STATEMENT: &str = "redirected_statement";

/// A guard against a pathological tree: no real command nests this deep, and a
/// bounded walk cannot exhaust the stack on a hostile input.
const MAX_DEPTH: usize = 512;

thread_local! {
    /// One parser per thread, built on first use.
    ///
    /// Reference `_get_parser` caches a single parser for the process. A
    /// `tree_sitter::Parser` is neither `Sync` nor cheap to build, so the cache
    /// is per thread here, which is the same amortization without a lock on the
    /// path a turn runs through.
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

/// The command segments `command` runs, in the order the grammar meets them.
///
/// Returns an empty vector when the text holds no command node, which the
/// reference answers by resolving no permission of its own: a caller that
/// cannot see what runs asks rather than grants.
#[must_use]
pub fn extract_commands(command: &str) -> Vec<String> {
    PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut parser = Parser::new();
            if parser
                .set_language(&tree_sitter_bash::LANGUAGE.into())
                .is_err()
            {
                return Vec::new();
            }
            *slot = Some(parser);
        }
        let Some(parser) = slot.as_mut() else {
            return Vec::new();
        };
        let Some(tree) = parser.parse(command, None) else {
            return Vec::new();
        };
        let source = command.as_bytes();
        let mut segments = Vec::new();
        // Pre-order, children left to right, which is the order the reference
        // recursion appends in.
        let mut stack = vec![(tree.root_node(), 0_usize)];
        while let Some((node, depth)) = stack.pop() {
            if let Some(segment) = segment_of(node, source) {
                segments.push(segment);
            }
            if depth >= MAX_DEPTH {
                continue;
            }
            for index in (0..node.child_count()).rev() {
                if let Some(child) = node.child(index) {
                    stack.push((child, depth + 1));
                }
            }
        }
        segments
    })
}

/// The segment `node` contributes, or [`None`] when it is not a command.
fn segment_of(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() != "command" {
        return None;
    }
    let mut parts = Vec::new();
    for index in 0..node.child_count() {
        let Some(child) = node.child(index) else {
            continue;
        };
        if !SEGMENT_KINDS.contains(&child.kind()) {
            continue;
        }
        // Text that is not valid UTF-8 is skipped rather than lossily decoded:
        // a segment the policy cannot read exactly is a segment it must not
        // match a pattern against.
        if let Ok(text) = child.utf8_text(source) {
            parts.push(text.to_owned());
        }
    }
    if parts.is_empty() {
        return None;
    }
    if node
        .parent()
        .is_some_and(|parent| parent.kind() == REDIRECTED_STATEMENT)
    {
        parts.push(REDIRECT_MARKER.to_owned());
    }
    Some(parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// US-109: the five node kinds compose the segment, joined by one space.
    #[test]
    fn a_segment_is_its_words_joined_by_single_spaces() {
        assert_eq!(extract_commands("ls    -la"), vec!["ls -la".to_owned()]);
        assert_eq!(
            extract_commands("echo 'hello world'"),
            vec!["echo 'hello world'".to_owned()]
        );
        assert_eq!(
            extract_commands("git config user.name"),
            vec!["git config user.name".to_owned()]
        );
    }

    /// US-109: a redirected command carries the marker, so it is no longer the
    /// single word the standalone denylist refuses.
    #[test]
    fn a_redirected_command_carries_the_marker() {
        assert_eq!(
            extract_commands("python3 <<'EOF'\nprint(1)\nEOF"),
            vec![format!("python3 {REDIRECT_MARKER}")]
        );
        assert_eq!(
            extract_commands("cat file.txt > out.txt"),
            vec![format!("cat file.txt {REDIRECT_MARKER}")]
        );
        assert_eq!(
            extract_commands("wc -l < input.txt"),
            vec![format!("wc -l {REDIRECT_MARKER}")]
        );
    }

    /// US-109: each segment of a chain is extracted on its own, so approving
    /// one never approves the next.
    #[test]
    fn every_chain_segment_is_extracted_separately() {
        for chain in [
            "cat README.md && rm -rf build",
            "cat README.md || rm -rf build",
            "cat README.md ; rm -rf build",
            "cat README.md | rm -rf build",
        ] {
            assert_eq!(
                extract_commands(chain),
                vec!["cat README.md".to_owned(), "rm -rf build".to_owned()],
                "`{chain}` is not split per segment"
            );
        }
    }

    /// US-109: text holding no command node extracts to nothing, which is what
    /// makes the caller ask rather than allow.
    #[test]
    fn text_without_a_command_extracts_to_nothing() {
        assert!(extract_commands("").is_empty());
        assert!(extract_commands("   ").is_empty());
        assert!(extract_commands("# only a comment").is_empty());
    }

    /// US-109: the parser is reused and a 64 KB command stays well under the
    /// budget the PRD sets.
    #[test]
    fn a_64_kb_command_parses_within_the_budget() {
        let command = format!("ls {}", "a".repeat(64 * 1024));
        // The first call builds the parser; the measured one reuses it, which
        // is what the budget is stated against.
        let _ = extract_commands("ls");
        let started = std::time::Instant::now();
        let segments = extract_commands(&command);
        let elapsed = started.elapsed();
        assert_eq!(segments.len(), 1);
        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "parsing 64 KB took {elapsed:?}"
        );
    }
}
