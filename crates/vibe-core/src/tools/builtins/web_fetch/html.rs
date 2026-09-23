//! The document tree `web_fetch` converts, built the way the reference builds
//! it.
//!
//! Reference `_html_to_markdown` hands the page to `markdownify`, which parses
//! it with Beautiful Soup over the standard library's `html.parser`. Neither is
//! an HTML5 tree builder: there are no implied end tags and no foster
//! parenting, an end tag closes the most recent open element of its name and
//! everything opened after it, and an end tag nothing opened is dropped. What
//! reaches the converter is therefore a shape of its own, and reproducing that
//! shape is what makes the converted text agree. This module is written from
//! the behavior of CPython 3.12 `html.parser.HTMLParser` (tokenization, with
//! `convert_charrefs` off as Beautiful Soup sets it) and Beautiful Soup 4.14's
//! `HTMLParserTreeBuilder` (tree construction and string merging).

use super::entities::{INVALID_CHARREFS, INVALID_CODEPOINTS, NAMED, WINDOWS_1252};

/// One node of the parsed document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NodeKind {
    /// The Beautiful Soup object itself, named `[document]`.
    Document,
    Element {
        name: String,
        attributes: Vec<(String, String)>,
    },
    /// A string the converter reads: text, and the CDATA sections, other
    /// declarations and processing instructions Beautiful Soup keeps as
    /// strings.
    Text(String),
    /// A comment or a doctype, which the converter always skips but whose
    /// emptiness still decides whether a neighbor counts as having a sibling.
    Skipped(String),
}

#[derive(Debug, Clone)]
pub(super) struct Node {
    pub(super) kind: NodeKind,
    pub(super) parent: Option<usize>,
    pub(super) children: Vec<usize>,
    /// The position among the parent's children.
    pub(super) position: usize,
}

/// A parsed document: node 0 is the document.
#[derive(Debug, Clone)]
pub(super) struct Document {
    pub(super) nodes: Vec<Node>,
}

impl Document {
    pub(super) fn name(&self, node: usize) -> Option<&str> {
        match &self.nodes.get(node)?.kind {
            NodeKind::Document => Some("[document]"),
            NodeKind::Element { name, .. } => Some(name),
            _ => None,
        }
    }

    pub(super) fn is_element(&self, node: usize) -> bool {
        matches!(
            self.nodes.get(node).map(|node| &node.kind),
            Some(NodeKind::Element { .. } | NodeKind::Document)
        )
    }

    pub(super) fn attribute(&self, node: usize, name: &str) -> Option<&str> {
        match &self.nodes.get(node)?.kind {
            NodeKind::Element { attributes, .. } => attributes
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str()),
            _ => None,
        }
    }

    pub(super) fn parent(&self, node: usize) -> Option<usize> {
        self.nodes.get(node)?.parent
    }

    pub(super) fn children(&self, node: usize) -> &[usize] {
        self.nodes
            .get(node)
            .map_or(&[], |node| node.children.as_slice())
    }

    pub(super) fn previous_sibling(&self, node: usize) -> Option<usize> {
        let position = self.nodes.get(node)?.position;
        let parent = self.parent(node)?;
        position
            .checked_sub(1)
            .and_then(|index| self.children(parent).get(index).copied())
    }

    pub(super) fn next_sibling(&self, node: usize) -> Option<usize> {
        let position = self.nodes.get(node)?.position;
        let parent = self.parent(node)?;
        self.children(parent).get(position + 1).copied()
    }

    /// Every descendant in document order, the node itself excluded.
    pub(super) fn descendants(&self, node: usize) -> Vec<usize> {
        let mut found = Vec::new();
        let mut pending = self
            .children(node)
            .iter()
            .rev()
            .copied()
            .collect::<Vec<_>>();
        while let Some(next) = pending.pop() {
            found.push(next);
            pending.extend(self.children(next).iter().rev().copied());
        }
        found
    }

    /// The node and its ancestors, innermost first.
    pub(super) fn lineage(&self, node: usize) -> impl Iterator<Item = usize> + '_ {
        std::iter::successors(Some(node), |current| self.parent(*current))
    }
}

/// Beautiful Soup `HTMLTreeBuilder.DEFAULT_EMPTY_ELEMENT_TAGS`.
const VOID_ELEMENTS: [&str; 24] = [
    "area", "base", "basefont", "bgsound", "br", "col", "command", "embed", "frame", "hr", "image",
    "img", "input", "isindex", "keygen", "link", "menuitem", "meta", "nextid", "param", "source",
    "spacer", "track", "wbr",
];

/// Beautiful Soup `HTMLTreeBuilder.DEFAULT_PRESERVE_WHITESPACE_TAGS`.
const PRESERVE_WHITESPACE: [&str; 2] = ["pre", "textarea"];

/// `html.parser.HTMLParser.CDATA_CONTENT_ELEMENTS`, read as raw text.
const RAW_TEXT_ELEMENTS: [&str; 6] = ["script", "style", "xmp", "iframe", "noembed", "noframes"];

/// `html.parser.HTMLParser.RCDATA_CONTENT_ELEMENTS`, read as text whose only
/// markup is character references.
const ESCAPABLE_TEXT_ELEMENTS: [&str; 2] = ["textarea", "title"];

/// `BeautifulSoup.ASCII_SPACES`.
fn is_ascii_space(character: char) -> bool {
    matches!(character, ' ' | '\n' | '\t' | '\u{c}' | '\r')
}

/// The whitespace the HTML tokenizer separates on, `[\t\n\r\f ]`.
fn is_tag_space(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\r' | '\u{c}' | ' ')
}

/// Parses `markup` into the tree Beautiful Soup's `html.parser` builder
/// produces.
pub(super) fn parse(markup: &str) -> Document {
    let mut builder = TreeBuilder::new();
    Tokenizer::new(markup).run(&mut builder);
    builder.finish()
}

// ---------------------------------------------------------------------------
// Tree construction (Beautiful Soup)
// ---------------------------------------------------------------------------

/// How a finished run of data enters the tree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StringKind {
    Text,
    Skipped,
}

struct TreeBuilder {
    nodes: Vec<Node>,
    stack: Vec<usize>,
    data: Vec<String>,
    /// `already_closed_empty_element`: void elements the builder closed itself,
    /// whose explicit end tag is then struck off rather than applied.
    closed_void: Vec<String>,
}

impl TreeBuilder {
    fn new() -> Self {
        Self {
            nodes: vec![Node {
                kind: NodeKind::Document,
                parent: None,
                children: Vec::new(),
                position: 0,
            }],
            stack: vec![0],
            data: Vec::new(),
            closed_void: Vec::new(),
        }
    }

    fn current(&self) -> usize {
        self.stack.last().copied().unwrap_or(0)
    }

    fn append(&mut self, kind: NodeKind) -> usize {
        let parent = self.current();
        let index = self.nodes.len();
        let position = self.nodes[parent].children.len();
        self.nodes.push(Node {
            kind,
            parent: Some(parent),
            children: Vec::new(),
            position,
        });
        self.nodes[parent].children.push(index);
        index
    }

    fn preserving_whitespace(&self) -> bool {
        self.stack.iter().any(|node| {
            matches!(&self.nodes[*node].kind, NodeKind::Element { name, .. }
                if PRESERVE_WHITESPACE.contains(&name.as_str()))
        })
    }

    /// `BeautifulSoup.endData`: the pending data becomes one string, and a
    /// string of ASCII spaces alone collapses to one newline or one space
    /// unless a `pre` or `textarea` is open.
    fn end_data(&mut self, kind: StringKind) {
        if self.data.is_empty() {
            return;
        }
        let mut joined = self.data.concat();
        self.data.clear();
        if !self.preserving_whitespace() && joined.chars().all(is_ascii_space) {
            joined = if joined.contains('\n') { "\n" } else { " " }.to_owned();
        }
        self.append(match kind {
            StringKind::Text => NodeKind::Text(joined),
            StringKind::Skipped => NodeKind::Skipped(joined),
        });
    }

    fn data(&mut self, text: &str) {
        self.data.push(text.to_owned());
    }

    fn string(&mut self, text: &str, kind: StringKind) {
        self.end_data(StringKind::Text);
        self.data(text);
        self.end_data(kind);
    }

    fn open_count(&self, name: &str) -> usize {
        self.stack
            .iter()
            .skip(1)
            .filter(|node| {
                matches!(&self.nodes[**node].kind,
                NodeKind::Element { name: open, .. } if open == name)
            })
            .count()
    }

    /// `BeautifulSoup._popToTag`: pops up to and including the most recent
    /// open element named `name`, and nothing when none is open.
    fn pop_to(&mut self, name: &str) {
        if self.open_count(name) == 0 {
            return;
        }
        while self.stack.len() > 1 {
            let Some(top) = self.stack.pop() else {
                return;
            };
            if matches!(&self.nodes[top].kind, NodeKind::Element { name: open, .. } if open == name)
            {
                return;
            }
        }
    }

    fn start_tag(&mut self, name: &str, attributes: Vec<(String, Option<String>)>, closes: bool) {
        self.end_data(StringKind::Text);
        // A repeated attribute keeps its first position and its last value.
        let mut kept: Vec<(String, String)> = Vec::new();
        for (key, value) in attributes {
            let value = value.unwrap_or_default();
            match kept.iter_mut().find(|(existing, _)| *existing == key) {
                Some(entry) => entry.1 = value,
                None => kept.push((key, value)),
            }
        }
        let node = self.append(NodeKind::Element {
            name: name.to_owned(),
            attributes: kept,
        });
        self.stack.push(node);
        if closes && VOID_ELEMENTS.contains(&name) {
            self.end_data(StringKind::Text);
            self.pop_to(name);
            self.closed_void.push(name.to_owned());
        }
    }

    fn end_tag(&mut self, name: &str, check_closed: bool) {
        if check_closed
            && let Some(index) = self.closed_void.iter().position(|closed| closed == name)
        {
            self.closed_void.remove(index);
            return;
        }
        self.end_data(StringKind::Text);
        self.pop_to(name);
    }

    fn finish(mut self) -> Document {
        self.end_data(StringKind::Text);
        Document { nodes: self.nodes }
    }
}

// ---------------------------------------------------------------------------
// Tokenization (html.parser)
// ---------------------------------------------------------------------------

struct Tokenizer<'a> {
    raw: &'a str,
    /// The element whose content is read as raw or escapable text.
    text_element: Option<(String, bool)>,
}

impl<'a> Tokenizer<'a> {
    fn new(raw: &'a str) -> Self {
        Self {
            raw,
            text_element: None,
        }
    }

    fn at(&self, index: usize) -> Option<char> {
        self.raw.get(index..).and_then(|rest| rest.chars().next())
    }

    fn starts(&self, index: usize, prefix: &str) -> bool {
        self.raw
            .get(index..)
            .is_some_and(|rest| rest.starts_with(prefix))
    }

    fn starts_ignoring_case(&self, index: usize, prefix: &str) -> bool {
        self.raw
            .get(index..index + prefix.len())
            .is_some_and(|slice| slice.eq_ignore_ascii_case(prefix))
    }

    fn find(&self, from: usize, needle: &str) -> Option<usize> {
        self.raw
            .get(from..)
            .and_then(|rest| rest.find(needle))
            .map(|offset| from + offset)
    }

    /// The next position `goahead` stops at: `&` or `<` in ordinary content,
    /// and the matching end tag (plus `&` for escapable text) inside a raw
    /// text element. [`None`] inside a text element that is never closed.
    fn next_interesting(&self, from: usize) -> Option<usize> {
        let Some((element, escapable)) = &self.text_element else {
            return Some(
                self.raw[from..]
                    .find(['&', '<'])
                    .map_or(self.raw.len(), |offset| from + offset),
            );
        };
        // `plaintext` is never closed: everything after it is its text.
        if element == "plaintext" {
            return Some(self.raw.len());
        }
        let closing = format!("</{element}");
        let mut index = from;
        while index < self.raw.len() {
            let rest = &self.raw[index..];
            let candidate = rest.find(['&', '<'])? + index;
            if *escapable && self.starts(candidate, "&") {
                return Some(candidate);
            }
            if self.starts_ignoring_case(candidate, &closing)
                && self
                    .at(candidate + closing.len())
                    .is_some_and(|next| is_tag_space(next) || next == '/' || next == '>')
            {
                return Some(candidate);
            }
            index = candidate + 1;
        }
        None
    }

    fn run(&mut self, tree: &mut TreeBuilder) {
        let raw = self.raw;
        let length = raw.len();
        let mut index = 0;
        while index < length {
            let Some(stop) = self.next_interesting(index) else {
                break;
            };
            if index < stop {
                tree.data(&raw[index..stop]);
            }
            index = stop;
            if index == length {
                break;
            }
            if self.starts(index, "<") {
                let next = if self.at(index + 1).is_some_and(|c| c.is_ascii_alphabetic()) {
                    self.start_tag(index, tree)
                } else if self.starts(index, "</") {
                    self.end_tag(index, tree)
                } else if self.starts(index, "<!--") {
                    self.comment(index, tree)
                } else if self.starts(index, "<?") {
                    self.processing_instruction(index, tree)
                } else if self.starts(index, "<!") {
                    self.declaration(index, tree)
                } else {
                    tree.data("<");
                    Some(index + 1)
                };
                index = match next {
                    Some(next) => next,
                    None => {
                        self.unterminated(index, tree);
                        length
                    }
                };
            } else if self.starts(index, "&#") {
                match self.character_reference(index) {
                    Some((value, next)) => {
                        tree.data(&value);
                        index = next;
                    }
                    None => {
                        // A malformed numeric reference stops the scan, and
                        // everything after it is data, markup included.
                        if raw[index..].contains(';') {
                            tree.data("&#");
                            index += 2;
                        }
                        break;
                    }
                }
            } else {
                match self.entity_reference(index) {
                    Some((value, next)) => {
                        tree.data(&value);
                        index = next;
                    }
                    None => {
                        let incomplete = self
                            .at(index + 1)
                            .is_some_and(|c| c.is_ascii_alphabetic() || c == '#');
                        if incomplete {
                            if raw[index..].chars().count() == 2 {
                                index += 1;
                            }
                            break;
                        }
                        if index + 1 < length {
                            tree.data("&");
                            index += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
        }
        if index < length {
            tree.data(&raw[index..]);
        }
    }

    /// What `goahead` does at end of input with a construct that never
    /// closed.
    fn unterminated(&self, index: usize, tree: &mut TreeBuilder) {
        let raw = self.raw;
        if self.at(index + 1).is_some_and(|c| c.is_ascii_alphabetic()) {
            // An unterminated start tag swallows the rest of the document.
        } else if self.starts(index, "</") {
            if index + 2 == raw.len() {
                tree.data("</");
            } else if self.at(index + 2).is_some_and(|c| c.is_ascii_alphabetic()) {
            } else {
                tree.string(&raw[index + 2..], StringKind::Skipped);
            }
        } else if self.starts(index, "<!--") {
            let body = &raw[index + 4..];
            let body = ["--!", "--", "-"]
                .iter()
                .find_map(|suffix| body.strip_suffix(suffix))
                .unwrap_or(body);
            tree.string(body, StringKind::Skipped);
        } else if self.starts(index, "<![CDATA[") {
            tree.string(&raw[index + 9..], StringKind::Text);
        } else if self.starts_ignoring_case(index, "<!doctype") {
            tree.string("", StringKind::Skipped);
        } else if self.starts(index, "<!") {
            tree.string(&raw[index + 2..], StringKind::Skipped);
        } else if self.starts(index, "<?") {
            tree.string(&raw[index + 2..], StringKind::Text);
        }
    }

    /// Where `locatetagend` stops, from the first letter of a tag name.
    fn tag_end(&self, from: usize) -> usize {
        let raw = self.raw;
        let mut index = from;
        let advance_while = |mut index: usize, keep: &dyn Fn(char) -> bool| {
            while let Some(c) = raw[index..].chars().next() {
                if !keep(c) {
                    break;
                }
                index += c.len_utf8();
            }
            index
        };
        index = advance_while(index, &|c| !(is_tag_space(c) || c == '/' || c == '>'));
        index = advance_while(index, &|c| is_tag_space(c) || c == '/');
        loop {
            let previous = raw[..index].chars().next_back();
            let Some(first) = raw[index..].chars().next() else {
                break;
            };
            let after_separator =
                previous.is_some_and(|c| matches!(c, '\'' | '"' | '/') || is_tag_space(c));
            if !after_separator || is_tag_space(first) || first == '/' || first == '>' {
                break;
            }
            index += first.len_utf8();
            index = advance_while(index, &|c| {
                !(is_tag_space(c) || c == '/' || c == '=' || c == '>')
            });
            if let Some(after_value) = self.attribute_value(index) {
                index = after_value.1;
            }
            index = advance_while(index, &|c| is_tag_space(c) || c == '/');
        }
        if self.starts(index, ">") {
            index += 1;
        }
        index
    }

    /// The optional `= value` after an attribute name: the value as written
    /// and where it ends, or [`None`] when no value follows.
    fn attribute_value(&self, from: usize) -> Option<(String, usize)> {
        let raw = self.raw;
        let skip_space = |mut index: usize| {
            while raw[index..].chars().next().is_some_and(is_tag_space) {
                index += 1;
            }
            index
        };
        let mut index = skip_space(from);
        if !self.starts(index, "=") {
            return None;
        }
        index = skip_space(index + 1);
        match self.at(index) {
            Some(quote @ ('\'' | '"')) => {
                let close = raw[index + 1..].find(quote)? + index + 1;
                Some((raw[index..=close].to_owned(), close + 1))
            }
            _ => {
                let end = raw[index..]
                    .find(|c: char| c == '>' || is_tag_space(c))
                    .map_or(raw.len(), |offset| index + offset);
                Some((raw[index..end].to_owned(), end))
            }
        }
    }

    /// `tagfind_tolerant`: the tag name and where the attributes start.
    fn tag_name(&self, from: usize) -> (String, usize) {
        let raw = self.raw;
        let end = raw[from..]
            .find(|c: char| is_tag_space(c) || c == '/' || c == '>')
            .map_or(raw.len(), |offset| from + offset);
        let name = raw[from..end].to_lowercase();
        (name, self.skip_attribute_separators(end))
    }

    /// `(?:[\t\n\r\f ]|/(?!>))*`.
    fn skip_attribute_separators(&self, mut index: usize) -> usize {
        loop {
            match self.at(index) {
                Some(c) if is_tag_space(c) => index += 1,
                Some('/') if !self.starts(index + 1, ">") => index += 1,
                _ => return index,
            }
        }
    }

    fn start_tag(&mut self, index: usize, tree: &mut TreeBuilder) -> Option<usize> {
        let raw = self.raw;
        let end = self.tag_end(index + 1);
        if !raw[..end].ends_with('>') {
            return None;
        }
        let (name, mut cursor) = self.tag_name(index + 1);
        let mut attributes = Vec::new();
        while cursor < end {
            let previous = raw[..cursor].chars().next_back();
            let Some(first) = self.at(cursor) else {
                break;
            };
            let after_separator =
                previous.is_some_and(|c| matches!(c, '\'' | '"' | '/') || is_tag_space(c));
            if !after_separator || is_tag_space(first) || first == '/' || first == '>' {
                break;
            }
            let name_end = raw[cursor + first.len_utf8()..]
                .find(|c: char| is_tag_space(c) || c == '/' || c == '=' || c == '>')
                .map_or(raw.len(), |offset| cursor + first.len_utf8() + offset);
            let key = raw[cursor..name_end].to_lowercase();
            let (value, after) = match self.attribute_value(name_end) {
                Some((written, after)) => {
                    let unquoted = if written.len() >= 2
                        && ((written.starts_with('\'') && written.ends_with('\''))
                            || (written.starts_with('"') && written.ends_with('"')))
                    {
                        written[1..written.len() - 1].to_owned()
                    } else {
                        written
                    };
                    let value = if unquoted.is_empty() {
                        unquoted
                    } else {
                        unescape(&unquoted)
                    };
                    (Some(value), after)
                }
                None => (None, name_end),
            };
            attributes.push((key, value));
            cursor = self.skip_attribute_separators(after);
        }
        let rest = py_strip(&raw[cursor..end]);
        if rest != ">" && rest != "/>" {
            tree.data(&raw[index..end]);
            return Some(end);
        }
        if rest.ends_with("/>") {
            tree.start_tag(&name, attributes, false);
            tree.end_tag(&name, true);
        } else {
            tree.start_tag(&name, attributes, true);
            if RAW_TEXT_ELEMENTS.contains(&name.as_str()) || name == "plaintext" {
                self.text_element = Some((name, false));
            } else if ESCAPABLE_TEXT_ELEMENTS.contains(&name.as_str()) {
                self.text_element = Some((name, true));
            }
        }
        Some(end)
    }

    fn end_tag(&mut self, index: usize, tree: &mut TreeBuilder) -> Option<usize> {
        let raw = self.raw;
        self.find(index + 2, ">")?;
        if !self.at(index + 2).is_some_and(|c| c.is_ascii_alphabetic()) {
            if self.starts(index + 2, ">") {
                return Some(index + 3);
            }
            return self.bogus_comment(index, tree);
        }
        let end = self.tag_end(index + 2);
        if !raw[..end].ends_with('>') {
            return None;
        }
        let (name, _) = self.tag_name(index + 2);
        tree.end_tag(&name, true);
        self.text_element = None;
        Some(end)
    }

    fn bogus_comment(&self, index: usize, tree: &mut TreeBuilder) -> Option<usize> {
        let close = self.find(index + 2, ">")?;
        tree.string(&self.raw[index + 2..close], StringKind::Skipped);
        Some(close + 1)
    }

    fn comment(&self, index: usize, tree: &mut TreeBuilder) -> Option<usize> {
        let raw = self.raw;
        let body = index + 4;
        let closed = raw[body..].match_indices("--").find_map(|(offset, _)| {
            let start = body + offset;
            if self.starts(start + 2, ">") {
                Some((start, start + 3))
            } else if self.starts(start + 2, "!>") {
                Some((start, start + 4))
            } else {
                None
            }
        });
        let (start, next) = match closed {
            Some(found) => found,
            None if self.starts(body, ">") => (body, body + 1),
            None if self.starts(body, "->") => (body, body + 2),
            None => return None,
        };
        tree.string(&raw[body..start], StringKind::Skipped);
        Some(next)
    }

    fn processing_instruction(&self, index: usize, tree: &mut TreeBuilder) -> Option<usize> {
        let close = self.find(index + 2, ">")?;
        tree.string(&self.raw[index + 2..close], StringKind::Text);
        Some(close + 1)
    }

    fn declaration(&self, index: usize, tree: &mut TreeBuilder) -> Option<usize> {
        let raw = self.raw;
        if self.starts(index, "<![CDATA[") {
            let close = self.find(index + 9, "]]>")?;
            tree.string(&raw[index + 9..close], StringKind::Text);
            return Some(close + 3);
        }
        if self.starts_ignoring_case(index, "<!doctype") {
            let close = self.find(index + 9, ">")?;
            tree.string("", StringKind::Skipped);
            return Some(close + 1);
        }
        if self.starts(index, "<![") {
            let close = self.find(index + 3, ">")?;
            if raw[..close].ends_with(']') {
                // `unknown_decl`: a CDATA section and any other declaration
                // both become strings the converter reads.
                let data = &raw[index + 3..close - 1];
                let text = if data.to_uppercase().starts_with("CDATA[") {
                    &data[6..]
                } else {
                    data
                };
                tree.string(text, StringKind::Text);
            } else {
                tree.string(&raw[index + 2..close], StringKind::Skipped);
            }
            return Some(close + 1);
        }
        self.bogus_comment(index, tree)
    }

    /// `&#…`: the character Beautiful Soup substitutes and where scanning
    /// resumes, or [`None`] when the reference is malformed.
    fn character_reference(&self, index: usize) -> Option<(String, usize)> {
        let raw = self.raw;
        let body = index + 2;
        let hexadecimal = matches!(self.at(body), Some('x' | 'X'));
        let digits_start = if hexadecimal { body + 1 } else { body };
        let digits_end = raw[digits_start..]
            .find(|c: char| {
                if hexadecimal {
                    !c.is_ascii_hexdigit()
                } else {
                    !c.is_ascii_digit()
                }
            })
            .map_or(raw.len(), |offset| digits_start + offset);
        // The terminator must be a character that is not a hexadecimal digit,
        // so a decimal run followed by `a` to `f` is no reference at all.
        let terminator = self.at(digits_end)?;
        if digits_end == digits_start || terminator.is_ascii_hexdigit() {
            return None;
        }
        let digits = &raw[digits_start..digits_end];
        let number =
            u32::from_str_radix(digits, if hexadecimal { 16 } else { 10 }).unwrap_or(u32::MAX);
        let next = if terminator == ';' {
            digits_end + 1
        } else {
            digits_end
        };
        Some((numeric_reference(number), next))
    }

    /// `&name`: what Beautiful Soup substitutes and where scanning resumes, or
    /// [`None`] when `entityref` does not match.
    fn entity_reference(&self, index: usize) -> Option<(String, usize)> {
        let raw = self.raw;
        let start = index + 1;
        if !self.at(start).is_some_and(|c| c.is_ascii_alphabetic()) {
            return None;
        }
        let run_end = raw[start..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '.'))
            .map_or(raw.len(), |offset| start + offset);
        let (name_end, terminator) = match self.at(run_end) {
            Some(terminator) => (run_end, terminator),
            None => {
                // At the very end the pattern backs off to a `-` or `.` inside
                // the run, which is then the terminator.
                let split = raw[start + 1..run_end].rfind(['-', '.'])? + start + 1;
                (split, raw[split..].chars().next()?)
            }
        };
        let name = &raw[start..name_end];
        let value = match lookup_named(&format!("{name};")) {
            Some(value) => value.to_owned(),
            None => format!("&{name}"),
        };
        let next = if terminator == ';' {
            name_end + 1
        } else {
            name_end
        };
        Some((value, next))
    }
}

/// Python `str.strip()`.
fn py_strip(text: &str) -> &str {
    text.trim_matches(is_python_space)
}

/// Python `str.isspace` for one character: Unicode whitespace plus the four
/// information separators Python counts and Rust does not.
pub(super) fn is_python_space(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

fn lookup_named(name: &str) -> Option<&'static str> {
    NAMED
        .binary_search_by(|(key, _)| (*key).cmp(name))
        .ok()
        .map(|index| NAMED[index].1)
}

/// `UnicodeDammit.numeric_character_reference`.
fn numeric_reference(number: u32) -> String {
    if number == 0 || number > 0x10_ffff || (0xd800..=0xdfff).contains(&number) {
        return "\u{fffd}".to_owned();
    }
    if (0x80..=0x9f).contains(&number)
        && let Some((_, value)) = WINDOWS_1252.iter().find(|(key, _)| *key == number)
    {
        return (*value).to_owned();
    }
    char::from_u32(number).map_or_else(|| "\u{fffd}".to_owned(), String::from)
}

/// Python `html.unescape`, which the tokenizer applies to attribute values.
pub(super) fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find('&') {
        output.push_str(&rest[..position]);
        let after = &rest[position + 1..];
        match unescape_one(after) {
            Some((value, consumed)) => {
                output.push_str(&value);
                rest = &after[consumed..];
            }
            None => {
                output.push('&');
                rest = after;
            }
        }
    }
    output.push_str(rest);
    output
}

/// One reference after `&`: `#[0-9]+;?`, `#[xX][0-9a-fA-F]+;?` or
/// `[^\t\n\f <&#;]{1,32};?`, with what it becomes and how much it consumed.
fn unescape_one(after: &str) -> Option<(String, usize)> {
    if let Some(numeric) = after.strip_prefix('#') {
        let (hexadecimal, digits_from) = match numeric.chars().next() {
            Some('x' | 'X') => (true, 1),
            _ => (false, 0),
        };
        let digits = &numeric[digits_from..];
        let count = digits
            .find(|c: char| {
                if hexadecimal {
                    !c.is_ascii_hexdigit()
                } else {
                    !c.is_ascii_digit()
                }
            })
            .unwrap_or(digits.len());
        if count == 0 {
            return None;
        }
        let mut consumed = 1 + digits_from + count;
        if numeric[digits_from + count..].starts_with(';') {
            consumed += 1;
        }
        let number = u32::from_str_radix(&digits[..count], if hexadecimal { 16 } else { 10 })
            .unwrap_or(u32::MAX);
        let value =
            if let Some((_, value)) = INVALID_CHARREFS.iter().find(|(key, _)| *key == number) {
                (*value).to_owned()
            } else if (0xd800..=0xdfff).contains(&number) || number > 0x10_ffff {
                "\u{fffd}".to_owned()
            } else if INVALID_CODEPOINTS.contains(&number) {
                String::new()
            } else {
                char::from_u32(number).map_or_else(|| "\u{fffd}".to_owned(), String::from)
            };
        return Some((value, consumed));
    }
    let mut end = 0;
    for (count, (offset, character)) in after.char_indices().enumerate() {
        if count == 32
            || matches!(
                character,
                '\t' | '\n' | '\u{c}' | ' ' | '<' | '&' | '#' | ';'
            )
        {
            break;
        }
        end = offset + character.len_utf8();
    }
    if end == 0 {
        return None;
    }
    let with_semicolon = after[end..].starts_with(';');
    let reference = if with_semicolon {
        &after[..=end]
    } else {
        &after[..end]
    };
    if let Some(value) = lookup_named(reference) {
        return Some((value.to_owned(), reference.len()));
    }
    // The longest legacy name the reference starts with, the rest kept.
    let mut cut = reference.len();
    while let Some((boundary, _)) = reference[..cut].char_indices().next_back() {
        cut = boundary;
        if cut < 2 {
            break;
        }
        if let Some(value) = lookup_named(&reference[..cut]) {
            return Some((format!("{value}{}", &reference[cut..]), reference.len()));
        }
    }
    Some((format!("&{reference}"), reference.len()))
}
