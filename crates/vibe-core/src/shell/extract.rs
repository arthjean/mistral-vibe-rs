//! Shell command extraction, on the grammar the reference parses with.
//!
//! Reference `analyze_shell_command` in
//! `vibe/core/tools/builtins/_shell_permission_analysis.py` walks the
//! `tree-sitter-bash` parse tree and answers three things: the words each
//! `command` node is made of, the syntax that keeps the call from being granted
//! without a prompt, and whether that syntax reaches the command's identity, in
//! which case the extracted words stop describing what runs and only the text
//! as written can be approved. A tokenizer cannot answer any of the three: it
//! has no notion of a heredoc, so `python3 <<'EOF' ... EOF` reads as a bare
//! standalone `python3`.
//!
//! The extraction is deliberately partial in the same places the reference is:
//! an assignment prefix, a loop header and a redirect target are not `command`
//! nodes, so none of the three reaches the extracted words. What the words never
//! show is what the approval reasons account for instead.
//!
//! The reasons are this port's own wording. Only whether there is one decides
//! anything; the wording surfaces in the label of the whole-command
//! requirement, where the reference prints its own.

use std::cell::RefCell;
use std::collections::BTreeSet;

use tree_sitter::{Node, Parser};

use super::command_policy::has_option_guardrails;
use super::lexer::whitespace_words;
use crate::policy::arity::known_session_pattern_arity;

/// The node kinds whose text composes a command's words.
const SEGMENT_KINDS: [&str; 6] = [
    "command_name",
    "number",
    "word",
    "string",
    "raw_string",
    "concatenation",
];

/// The word appended to a command whose node sits under a redirect.
///
/// Reference `_read_command` appends it so the command stops being a single
/// word, which is the only thing the standalone denylist looks at.
pub const REDIRECT_MARKER: &str = "<redirect>";

/// The node kind wrapping a command whose output or input is redirected.
const REDIRECTED_STATEMENT: &str = "redirected_statement";

/// The children of a `file_redirect` that name its target.
const REDIRECT_VALUE_KINDS: [&str; 5] = ["word", "number", "string", "raw_string", "concatenation"];

/// The commands whose argument is a program or script to run, so the arity
/// table's boundary for them says nothing about where their identity stops.
const OPAQUE_ARGUMENT_COMMANDS: [&str; 3] = [".", "env", "source"];

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

/// The command parts `command` runs, in the order the grammar meets them.
///
/// Returns an empty vector when the text holds no command node, which the
/// reference answers by resolving no permission of its own: a caller that
/// cannot see what runs asks rather than grants.
#[must_use]
pub fn extract_commands(command: &str) -> Vec<String> {
    analyze_text(command).parts
}

/// What the grammar made of a command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TextAnalysis {
    /// The words of every `command` node, one string per node.
    pub(crate) parts: Vec<String>,
    /// Why the text may not be granted without a prompt, in this port's words.
    pub(crate) reasons: BTreeSet<String>,
    /// Whether [`Self::parts`] stopped describing what the shell will run, so
    /// nothing narrower than the text itself is honest to record.
    pub(crate) invalidates_scope: bool,
}

impl TextAnalysis {
    pub(crate) fn requires_approval(&self) -> bool {
        !self.reasons.is_empty()
    }

    /// The label of the requirement that approves the text as written.
    pub(crate) fn approval_label(&self) -> String {
        format!(
            "shell text that needs approval as written ({})",
            self.reasons.iter().cloned().collect::<Vec<_>>().join(", ")
        )
    }
}

/// Parses `command` and walks the tree the way the reference does.
pub(crate) fn analyze_text(command: &str) -> TextAnalysis {
    let mut analysis = TextAnalysis::default();
    // A POSIX shell removes a backslash-newline pair before it tokenizes, which
    // the grammar does not do consistently, so the parse can describe another
    // command than the one that runs.
    if command.contains("\\\n") {
        analysis.reasons.insert("a line continuation".to_owned());
        analysis.invalidates_scope = true;
    }
    PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut parser = Parser::new();
            if parser
                .set_language(&tree_sitter_bash::LANGUAGE.into())
                .is_err()
            {
                return;
            }
            *slot = Some(parser);
        }
        let Some(parser) = slot.as_mut() else {
            return;
        };
        let Some(tree) = parser.parse(command, None) else {
            return;
        };
        let source = command.as_bytes();
        if tree.root_node().has_error() {
            analysis.reasons.insert("a parse error".to_owned());
            analysis.invalidates_scope = true;
        }
        // Pre-order, children left to right, which is the order the reference
        // recursion appends in.
        let mut stack = vec![(tree.root_node(), 0_usize)];
        while let Some((node, depth)) = stack.pop() {
            visit(node, source, &mut analysis);
            if depth >= MAX_DEPTH {
                continue;
            }
            for index in (0..node.child_count()).rev() {
                if let Some(child) = node.child(index) {
                    stack.push((child, depth + 1));
                }
            }
        }
    });
    analysis
}

/// Records what one node contributes.
fn visit(node: Node<'_>, source: &[u8], analysis: &mut TextAnalysis) {
    if let Some(reason) = node_reason(node, source) {
        analysis.reasons.insert(reason);
    }
    match node.kind() {
        // `HOME=/x git status` extracts as `git status`, so a pattern would
        // cover any environment, including one that redirects what git runs.
        "variable_assignment" | "function_definition" => analysis.invalidates_scope = true,
        // The body is what runs, the script behind `python <<EOF`, and none
        // of it reaches the words.
        "heredoc_redirect" | "herestring_redirect" => analysis.invalidates_scope = true,
        // The target becomes the redirect marker, which a trailing `*` would
        // cover: the pattern would grant every target.
        "file_redirect" if file_redirect_reason(node, source).is_some() => {
            analysis.invalidates_scope = true;
        }
        "command" => {
            let read = read_command(node, source);
            analysis.reasons.extend(read.reasons);
            analysis.invalidates_scope |= read.invalidates_scope;
            if !read.text.is_empty() {
                analysis.parts.push(read.text);
            }
        }
        _ => {}
    }
}

fn text_of<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or_default()
}

/// Why a node withholds the grant, or [`None`].
fn node_reason(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() == "file_redirect" {
        return file_redirect_reason(node, source);
    }
    if let Some(reason) = fixed_reason(node.kind()) {
        return Some(reason.to_owned());
    }
    if let Some(reason) = zsh_node_reason(node, source) {
        return Some(reason.to_owned());
    }
    let brace = node.kind() == "concatenation"
        && children(node)
            .any(|child| child.kind() == "word" && matches!(text_of(child, source), "{" | "}"));
    brace.then(|| "a brace expansion".to_owned())
}

/// The reasons that depend on the node kind alone.
fn fixed_reason(kind: &str) -> Option<&'static str> {
    dynamic_reason(kind).or(match kind {
        "heredoc_redirect" | "herestring_redirect" => Some("a redirect"),
        "case_statement" => Some("a case statement"),
        "compound_statement" => Some("a command group"),
        "c_style_for_statement" | "for_statement" | "while_statement" => Some("a loop"),
        "function_definition" => Some("a function definition"),
        "if_statement" => Some("a conditional"),
        "subshell" => Some("a subshell"),
        "test_command" => Some("a test expression"),
        "&" => Some("a background job"),
        _ => None,
    })
}

/// The reason a node the shell expands at run time carries, which also marks
/// the node as unreadable.
fn dynamic_reason(kind: &str) -> Option<&'static str> {
    match kind {
        "ansi_c_string" => Some("ANSI-C quoting"),
        "arithmetic_expansion" => Some("an arithmetic expansion"),
        "brace_expression" => Some("a brace expansion"),
        "command_substitution" => Some("a command substitution"),
        "expansion" => Some("a parameter expansion"),
        "process_substitution" => Some("a process substitution"),
        "simple_expansion" => Some("a variable reference"),
        "variable_assignment" => Some("an environment assignment"),
        _ => None,
    }
}

/// Whether a redirect can reach a file, and so why it withholds the grant.
///
/// Renumbering a descriptor (`2>&1`) and discarding into `/dev/null` reach no
/// file; every other redirect names a path the command writes or reads.
fn file_redirect_reason(node: Node<'_>, source: &[u8]) -> Option<String> {
    let reason = || Some("a redirect".to_owned());
    let Some(target) = children(node)
        .filter(|child| REDIRECT_VALUE_KINDS.contains(&child.kind()))
        .last()
    else {
        return reason();
    };
    // A duplication target must be a bare number: `>&file` is shorthand for
    // sending both streams into that file.
    let only_duplication = children(node)
        .filter(|child| {
            !REDIRECT_VALUE_KINDS.contains(&child.kind()) && child.kind() != "file_descriptor"
        })
        .all(|child| matches!(child.kind(), ">&" | "<&"));
    if only_duplication && target.kind() == "number" {
        return None;
    }
    if text_of(target, source) == "/dev/null" {
        return None;
    }
    reason()
}

/// The zsh expansion a word triggers that the bash grammar reads as literal.
fn zsh_word_reason(value: &str) -> Option<&'static str> {
    if value.starts_with('=') {
        return Some("a zsh `=` expansion");
    }
    if value.contains("==") {
        return Some("a zsh `==` expansion");
    }
    if value.starts_with('~') && !(value == "~" || value.starts_with("~/")) {
        return Some("a named-directory expansion");
    }
    if value.contains("***") {
        return Some("a zsh `***` glob");
    }
    None
}

fn zsh_node_reason(node: Node<'_>, source: &[u8]) -> Option<&'static str> {
    if !matches!(node.kind(), "command_name" | "word") {
        return None;
    }
    zsh_word_reason(text_of(node, source))
}

fn children<'tree>(node: Node<'tree>) -> impl Iterator<Item = Node<'tree>> {
    (0..node.child_count()).filter_map(move |index| node.child(index))
}

fn contains_dynamic(node: Node<'_>) -> bool {
    dynamic_reason(node.kind()).is_some() || children(node).any(contains_dynamic)
}

/// What one `command` node contributes.
struct CommandRead {
    text: String,
    reasons: BTreeSet<String>,
    invalidates_scope: bool,
}

/// The words of a `command` node, and whether they can be granted as a
/// pattern.
///
/// A token the extraction cannot read is harmless only where the session
/// pattern would have wildcarded it away; any earlier and it was part of the
/// command's identity, so `git $SUB` would leave `git *`.
fn read_command(node: Node<'_>, source: &[u8]) -> CommandRead {
    let mut parts: Vec<String> = Vec::new();
    let mut reasons = BTreeSet::new();
    let mut invalidates_scope = false;
    let mut first_unreadable = None;
    for child in children(node) {
        // A prefix assignment takes no argument position; the walk judges it.
        if child.kind() == "variable_assignment" {
            continue;
        }
        // Taken before the push, so a dropped child still accounts for its slot.
        let index = whitespace_words(&parts.join(" ")).len();
        let unreadable =
            if SEGMENT_KINDS.contains(&child.kind()) && zsh_node_reason(child, source).is_none() {
                parts.push(text_of(child, source).to_owned());
                contains_dynamic(child)
            } else {
                if child.kind() == "ansi_c_string" {
                    // Kept for the guardrails, such as `find`'s execution
                    // predicates, while the dynamic reason still asks.
                    parts.push(text_of(child, source).to_owned());
                } else if dynamic_reason(child.kind()).is_none()
                    && zsh_node_reason(child, source).is_none()
                {
                    // The shell runs the original text, so a child this extraction
                    // omits makes the two views differ.
                    reasons.insert(format!(
                        "syntax the policy does not model ({})",
                        child.kind()
                    ));
                    invalidates_scope = true;
                }
                true
            };
        if unreadable && first_unreadable.is_none() {
            first_unreadable = Some(index);
        }
    }
    if let Some(index) = first_unreadable
        && !identity_survives(&parts, index)
    {
        invalidates_scope = true;
    }
    if !parts.is_empty()
        && node
            .parent()
            .is_some_and(|parent| parent.kind() == REDIRECTED_STATEMENT)
    {
        parts.push(REDIRECT_MARKER.to_owned());
    }
    CommandRead {
        text: parts.join(" "),
        reasons,
        invalidates_scope,
    }
}

/// Whether a pattern over `parts` still names what an unreadable token at
/// `first_unreadable` runs.
///
/// Only a boundary the arity table states counts: `sudo $CMD` would otherwise
/// read as an argument the trailing `*` covers when it is the program `sudo`
/// goes on to run. A guardrailed command is recorded as its own text, which
/// simply loses the token, and the token could be the guarded option.
fn identity_survives(parts: &[String], first_unreadable: usize) -> bool {
    let tokens = whitespace_words(&parts.join(" "));
    let Some(first) = tokens.first() else {
        return false;
    };
    if OPAQUE_ARGUMENT_COMMANDS.contains(&first.as_str()) || has_option_guardrails(&tokens) {
        return false;
    }
    let Some(arity) = known_session_pattern_arity(&tokens) else {
        return false;
    };
    // A quote in the kept prefix means the split landed inside a token.
    if tokens
        .iter()
        .take(arity)
        .any(|token| token.contains(['"', '\'']))
    {
        return false;
    }
    first_unreadable >= arity
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
