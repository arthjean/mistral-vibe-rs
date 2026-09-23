//! An HTML page as the Markdown the reference hands its model.
//!
//! Reference `_html_to_markdown` (`vibe/core/tools/builtins/web_fetch.py`)
//! runs `markdownify` 1.2 with ATX headings and `-` bullets, through a
//! converter that also empties `script`, `style`, `noscript`, `iframe`,
//! `object` and `embed`. This module reproduces that library's behavior over
//! the tree [`super::html`] builds: whitespace is normalized per text node and
//! dropped at block boundaries, each element converts the text its children
//! produced, and runs of newlines between siblings collapse to at most one
//! blank line. Every other option stays at its default: no wrapping,
//! asterisks and underscores escaped, `**` and `*` for strong and emphasis,
//! and two trailing spaces for a line break.
//!
//! The library also fails on some pages, and the reference reports those
//! failures as the tool's error, so [`ConversionError`] carries the message
//! the reference interpreter raises in each case.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use super::html::{self, Document, NodeKind, is_python_space};
use super::numeric::{DECIMAL, DIGIT, NUMERIC};

/// How deep an element may nest before the conversion gives up.
///
/// The reference converts recursively, two interpreter frames per level,
/// under Python's default limit of 1000 frames: called directly, 494 nested
/// elements convert and 495 raise. A caller that is itself deeper lowers that
/// bound in the reference, so it is a measurement rather than a contract.
const MAX_DEPTH: usize = 494;

/// Python's default `sys.get_int_max_str_digits()`.
const MAX_INT_DIGITS: usize = 4300;

/// The conversion failed where the reference's does, with the message the
/// reference interpreter raises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ConversionError(pub(super) String);

/// The Markdown `markdownify` produces for `markup` under the reference's
/// options.
pub(super) fn html_to_markdown(markup: &str) -> Result<String, ConversionError> {
    let document = html::parse(markup);
    Converter {
        document: &document,
        looked_up: RefCell::default(),
    }
    .process_tag(0, &BTreeSet::new(), 0)
}

type Tags = BTreeSet<String>;

struct Converter<'a> {
    document: &'a Document,
    /// `convert_fn_cache`: every tag name whose conversion was looked up, and
    /// whether that lookup resolved to the cache itself, which only a tag
    /// whose method name reads `fn_cache` can do.
    looked_up: RefCell<BTreeMap<String, bool>>,
}

/// The converter method a tag name resolves to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Method {
    Named,
    Heading(usize),
    None,
}

fn range_order(ranges: &[(u32, u32)], character: char) -> Option<usize> {
    let code = u32::from(character);
    ranges
        .binary_search_by(|(start, end)| {
            if code < *start {
                std::cmp::Ordering::Greater
            } else if code > *end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .ok()
}

/// The value `int()` reads from one decimal digit of any script.
fn decimal_value(character: char) -> Option<u32> {
    let index = range_order(DECIMAL, character)?;
    Some((u32::from(character) - DECIMAL[index].0) % 10)
}

/// `h` followed by digits at the start of a tag name, as `re_html_heading`
/// matches it, with the level those digits name held to 1 through 6.
fn heading_level(name: &str) -> Option<usize> {
    let digits = name.strip_prefix('h')?;
    let values = digits.chars().map_while(decimal_value).collect::<Vec<_>>();
    if values.is_empty() {
        return None;
    }
    let level = values
        .iter()
        .try_fold(0_usize, |value, digit| {
            value.checked_mul(10)?.checked_add(*digit as usize)
        })
        .unwrap_or(usize::MAX);
    Some(level.clamp(1, 6))
}

/// Python `int(text)` for a string `str.isdigit` or `str.isnumeric` accepted,
/// as a canonical ASCII decimal.
fn python_int(text: &str) -> Result<String, ConversionError> {
    let digits = text
        .chars()
        .map(decimal_value)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            ConversionError(format!("invalid literal for int() with base 10: '{text}'"))
        })?;
    if digits.len() > MAX_INT_DIGITS {
        return Err(ConversionError(format!(
            "Exceeds the limit ({MAX_INT_DIGITS} digits) for integer string conversion: value \
             has {} digits; use sys.set_int_max_str_digits() to increase the limit",
            digits.len()
        )));
    }
    let canonical = digits
        .iter()
        .skip_while(|digit| **digit == 0)
        .filter_map(|digit| char::from_digit(*digit, 10))
        .collect::<String>();
    Ok(if canonical.is_empty() {
        "0".to_owned()
    } else {
        canonical
    })
}

/// A canonical ASCII decimal plus `addend`, formatted as `'%s' % int` formats
/// it.
fn add_decimal(decimal: &str, addend: usize) -> Result<String, ConversionError> {
    let mut digits = decimal.bytes().rev().map(|byte| usize::from(byte - b'0'));
    let mut carry = addend;
    let mut sum = Vec::new();
    loop {
        let digit = digits.next();
        if digit.is_none() && carry == 0 {
            break;
        }
        let total = digit.unwrap_or(0) + carry;
        sum.push(b'0' + u8::try_from(total % 10).unwrap_or(0));
        carry = total / 10;
    }
    if sum.is_empty() {
        sum.push(b'0');
    }
    if sum.len() > MAX_INT_DIGITS {
        return Err(ConversionError(format!(
            "Exceeds the limit ({MAX_INT_DIGITS} digits) for integer string conversion; use \
             sys.set_int_max_str_digits() to increase the limit"
        )));
    }
    sum.reverse();
    Ok(String::from_utf8_lossy(&sum).into_owned())
}

/// A `colspan` held to 1 through 1000 when `str.isdigit` holds for it, and 1
/// otherwise.
fn colspan_of(value: Option<&str>) -> Result<usize, ConversionError> {
    let Some(value) = value.filter(|value| {
        !value.is_empty() && value.chars().all(|c| range_order(DIGIT, c).is_some())
    }) else {
        return Ok(1);
    };
    let canonical = python_int(value)?;
    Ok(if canonical.len() > 4 {
        1000
    } else {
        canonical.parse::<usize>().unwrap_or(1000).clamp(1, 1000)
    })
}

/// Python `str.strip()`.
fn strip(text: &str) -> &str {
    text.trim_matches(is_python_space)
}

/// Python `str.strip(' \t\r\n')`.
fn strip_blank(text: &str) -> &str {
    text.trim_matches([' ', '\t', '\r', '\n'])
}

/// `re_all_whitespace.sub(' ', text)`: every run of `[\t \r\n]` becomes one
/// space.
fn collapse_all_whitespace(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut in_run = false;
    for character in text.chars() {
        if matches!(character, '\t' | ' ' | '\r' | '\n') {
            if !in_run {
                output.push(' ');
            }
            in_run = true;
        } else {
            output.push(character);
            in_run = false;
        }
    }
    output
}

/// `re_newline_whitespace` then `re_whitespace`: a run of `[\t \r\n]` holding
/// a line break becomes one newline, and any other run one space.
fn normalize_whitespace(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, output: &mut String| {
        if !run.is_empty() {
            output.push(if run.contains(['\r', '\n']) {
                '\n'
            } else {
                ' '
            });
            run.clear();
        }
    };
    for character in text.chars() {
        if matches!(character, '\t' | ' ' | '\r' | '\n') {
            run.push(character);
        } else {
            flush(&mut run, &mut output);
            output.push(character);
        }
    }
    flush(&mut run, &mut output);
    output
}

/// `chomp`: the text stripped, and a space kept on either side where the text
/// began or ended with one.
fn chomp(text: &str) -> (&'static str, &'static str, &str) {
    let prefix = if text.starts_with(' ') { " " } else { "" };
    let suffix = if text.ends_with(' ') { " " } else { "" };
    (prefix, suffix, strip(text))
}

/// `re_line_with_content.sub`: every line, the empty ones included, through
/// `indent`.
fn map_lines(text: &str, indent: impl Fn(&str) -> String) -> String {
    text.split('\n').map(indent).collect::<Vec<_>>().join("\n")
}

/// Every non-empty line of `text` behind `indent`.
fn indent_lines(text: &str, indent: &str) -> String {
    map_lines(text, |line| {
        if line.is_empty() {
            String::new()
        } else {
            format!("{indent}{line}")
        }
    })
}

/// `re_extract_newlines`: the leading newlines, the content, and the trailing
/// newlines of one converted child.
fn split_newlines(text: &str) -> (&str, &str, &str) {
    let leading = text.len() - text.trim_start_matches('\n').len();
    let rest = &text[leading..];
    let content = rest.trim_end_matches('\n');
    (&text[..leading], content, &rest[content.len()..])
}

/// `abstract_inline_conversion`: the chomped text inside `markup`.
fn wrap_inline(text: &str, markup: &str, noformat: bool) -> String {
    if noformat {
        return text.to_owned();
    }
    let (prefix, suffix, text) = chomp(text);
    if text.is_empty() {
        return String::new();
    }
    format!("{prefix}{markup}{text}{markup}{suffix}")
}

/// `convert_code`: a span delimited by one more backtick than the longest run
/// inside it.
fn convert_code(text: &str, noformat: bool) -> String {
    if noformat {
        return text.to_owned();
    }
    let (prefix, suffix, text) = chomp(text);
    if text.is_empty() {
        return String::new();
    }
    let longest = text
        .split(|character: char| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let delimiter = "`".repeat(longest + 1);
    let text = if longest > 0 {
        format!(" {text} ")
    } else {
        text.to_owned()
    };
    format!("{prefix}{delimiter}{text}{delimiter}{suffix}")
}

/// `strip_pre`: the leading run of spaces and newlines up to its last
/// newline, and every trailing space and newline.
fn strip_pre(text: &str) -> &str {
    let leading = text.len() - text.trim_start_matches([' ', '\n']).len();
    let start = text[..leading].rfind('\n').map_or(0, |index| index + 1);
    text[start..].trim_end_matches([' ', '\n'])
}

/// The method name the library derives from a tag name.
fn method_name(tag: &str) -> String {
    tag.to_lowercase().replace(['[', ']', ':', '-'], "_")
}

fn is_named_converter(method: &str) -> bool {
    matches!(
        method,
        "_document_"
            | "a"
            | "article"
            | "b"
            | "blockquote"
            | "br"
            | "caption"
            | "code"
            | "dd"
            | "del"
            | "div"
            | "dl"
            | "dt"
            | "em"
            | "embed"
            | "figcaption"
            | "hr"
            | "i"
            | "iframe"
            | "img"
            | "kbd"
            | "li"
            | "list"
            | "noscript"
            | "object"
            | "ol"
            | "p"
            | "pre"
            | "q"
            | "s"
            | "samp"
            | "script"
            | "section"
            | "strong"
            | "style"
            | "sub"
            | "sup"
            | "table"
            | "td"
            | "th"
            | "tr"
            | "ul"
            | "video"
    )
}

impl Converter<'_> {
    fn name(&self, node: Option<usize>) -> Option<&str> {
        node.and_then(|node| self.document.name(node))
    }

    /// Whether a sibling slot holds something Python finds truthy: an
    /// element, or a string that is not empty.
    fn present(&self, node: Option<usize>) -> bool {
        node.and_then(|node| self.document.nodes.get(node))
            .is_some_and(|node| match &node.kind {
                NodeKind::Text(text) | NodeKind::Skipped(text) => !text.is_empty(),
                NodeKind::Document | NodeKind::Element { .. } => true,
            })
    }

    /// `should_remove_whitespace_inside`.
    fn remove_inside(&self, node: Option<usize>) -> bool {
        let Some(name) = self.name(node) else {
            return false;
        };
        heading_level(name).is_some()
            || matches!(
                name,
                "p" | "blockquote"
                    | "article"
                    | "div"
                    | "section"
                    | "ol"
                    | "ul"
                    | "li"
                    | "dl"
                    | "dt"
                    | "dd"
                    | "table"
                    | "thead"
                    | "tbody"
                    | "tfoot"
                    | "tr"
                    | "td"
                    | "th"
            )
    }

    /// `should_remove_whitespace_outside`.
    fn remove_outside(&self, node: Option<usize>) -> bool {
        self.remove_inside(node) || self.name(node) == Some("pre")
    }

    fn text(&self, node: usize) -> Option<&str> {
        match &self.document.nodes.get(node)?.kind {
            NodeKind::Text(text) => Some(text),
            _ => None,
        }
    }

    /// `_is_block_content_element`.
    fn is_block_content(&self, node: usize) -> bool {
        match &self.document.nodes[node].kind {
            NodeKind::Element { .. } | NodeKind::Document => true,
            NodeKind::Text(text) => !strip(text).is_empty(),
            NodeKind::Skipped(_) => false,
        }
    }

    fn next_block_content_sibling(&self, node: usize) -> Option<usize> {
        let mut current = self.document.next_sibling(node);
        while let Some(sibling) = current {
            if self.is_block_content(sibling) {
                return Some(sibling);
            }
            current = self.document.next_sibling(sibling);
        }
        None
    }

    /// `find_previous_sibling()`: the nearest earlier sibling that is an
    /// element.
    fn previous_element_sibling(&self, node: usize) -> Option<usize> {
        let mut current = self.document.previous_sibling(node);
        while let Some(sibling) = current {
            if self.document.is_element(sibling) {
                return Some(sibling);
            }
            current = self.document.previous_sibling(sibling);
        }
        None
    }

    fn descendants_named(&self, node: usize, names: &[&str]) -> Vec<usize> {
        self.document
            .descendants(node)
            .into_iter()
            .filter(|descendant| {
                self.document
                    .name(*descendant)
                    .is_some_and(|name| names.contains(&name))
            })
            .collect()
    }

    /// `_can_ignore` inside `process_tag`.
    fn can_ignore(&self, node: usize, remove_inside: bool) -> bool {
        match &self.document.nodes[node].kind {
            NodeKind::Element { .. } | NodeKind::Document => false,
            NodeKind::Skipped(_) => true,
            NodeKind::Text(text) => {
                if !strip(text).is_empty() {
                    return false;
                }
                let previous = self.document.previous_sibling(node);
                let next = self.document.next_sibling(node);
                (remove_inside && (!self.present(previous) || !self.present(next)))
                    || self.remove_outside(previous)
                    || self.remove_outside(next)
            }
        }
    }

    /// `get_conv_fn_cached`: the method a tag name resolves to, the first
    /// lookup of each name deciding it for the rest of the page.
    fn method(&self, tag: &str) -> Result<Method, ConversionError> {
        let method = method_name(tag);
        let mut looked_up = self.looked_up.borrow_mut();
        let resolves_to_cache = match looked_up.get(tag) {
            Some(resolved) => *resolved,
            None => {
                // `getattr(self, "convert_fn_cache")` is the cache itself,
                // truthy once any earlier name was looked up.
                let resolved = method == "fn_cache" && !looked_up.is_empty();
                looked_up.insert(tag.to_owned(), resolved);
                resolved
            }
        };
        if resolves_to_cache {
            return Err(ConversionError("'dict' object is not callable".to_owned()));
        }
        if method == "soup" {
            return Err(ConversionError(
                "MarkdownConverter.convert_soup() got an unexpected keyword argument \
                 'parent_tags'"
                    .to_owned(),
            ));
        }
        if is_named_converter(&method) {
            return Ok(Method::Named);
        }
        Ok(heading_level(&tag.to_lowercase()).map_or(Method::None, Method::Heading))
    }

    fn process_tag(
        &self,
        node: usize,
        parent_tags: &Tags,
        depth: usize,
    ) -> Result<String, ConversionError> {
        if depth > MAX_DEPTH {
            return Err(ConversionError(
                "maximum recursion depth exceeded".to_owned(),
            ));
        }
        let name = self.document.name(node).unwrap_or_default().to_owned();
        let remove_inside = self.remove_inside(Some(node));
        let mut child_tags = parent_tags.clone();
        child_tags.insert(name.clone());
        if heading_level(&name).is_some() || name == "td" || name == "th" {
            child_tags.insert("_inline".to_owned());
        }
        if matches!(name.as_str(), "pre" | "code" | "kbd" | "samp") {
            child_tags.insert("_noformat".to_owned());
        }
        let mut child_strings = Vec::new();
        for child in self.document.children(node) {
            if self.can_ignore(*child, remove_inside) {
                continue;
            }
            let converted = if self.text(*child).is_some() {
                self.process_text(*child, &child_tags)
            } else {
                self.process_tag(*child, &child_tags, depth + 1)?
            };
            if !converted.is_empty() {
                child_strings.push(converted);
            }
        }
        let inside_pre = name == "pre"
            || self
                .document
                .lineage(node)
                .skip(1)
                .any(|ancestor| self.document.name(ancestor) == Some("pre"));
        let text = if inside_pre {
            child_strings.concat()
        } else {
            let mut pieces: Vec<String> = vec![String::new()];
            for child in &child_strings {
                let (leading, content, trailing) = split_newlines(child);
                let mut leading = leading.to_owned();
                if pieces.last().is_some_and(|last| !last.is_empty()) && !leading.is_empty() {
                    let previous = pieces.pop().unwrap_or_default();
                    leading = "\n".repeat(previous.len().max(leading.len()).min(2));
                }
                pieces.push(leading);
                pieces.push(content.to_owned());
                pieces.push(trailing.to_owned());
            }
            pieces.concat()
        };
        match self.method(&name)? {
            Method::None => Ok(text),
            Method::Heading(_) if parent_tags.contains("_inline") => Ok(text),
            Method::Heading(level) => {
                let text = collapse_all_whitespace(strip(&text));
                Ok(format!("\n\n{} {text}\n\n", "#".repeat(level)))
            }
            Method::Named => self.convert(node, &method_name(&name), text, parent_tags),
        }
    }

    fn process_text(&self, node: usize, parent_tags: &Tags) -> String {
        let raw = self.text(node).unwrap_or_default();
        let mut text = if parent_tags.contains("pre") {
            raw.to_owned()
        } else {
            normalize_whitespace(raw)
        };
        if !parent_tags.contains("_noformat") {
            text = text.replace('*', "\\*").replace('_', "\\_");
        }
        let previous = self.document.previous_sibling(node);
        let next = self.document.next_sibling(node);
        let parent = self.document.parent(node);
        if self.remove_outside(previous) || (self.remove_inside(parent) && !self.present(previous))
        {
            text = text.trim_start_matches([' ', '\t', '\r', '\n']).to_owned();
        }
        if self.remove_outside(next) || (self.remove_inside(parent) && !self.present(next)) {
            text = text.trim_end_matches(is_python_space).to_owned();
        }
        text
    }

    fn convert(
        &self,
        node: usize,
        method: &str,
        text: String,
        tags: &Tags,
    ) -> Result<String, ConversionError> {
        let inline = tags.contains("_inline");
        let noformat = tags.contains("_noformat");
        Ok(match method {
            "_document_" => text.trim_matches('\n').to_owned(),
            "a" => self.convert_a(node, &text, noformat),
            "b" | "strong" => wrap_inline(&text, "**", noformat),
            "em" | "i" => wrap_inline(&text, "*", noformat),
            "del" | "s" => wrap_inline(&text, "~~", noformat),
            "sub" | "sup" => wrap_inline(&text, "", noformat),
            "blockquote" => {
                let text = strip_blank(&text);
                if inline {
                    format!(" {text} ")
                } else if text.is_empty() {
                    "\n".to_owned()
                } else {
                    let quoted = map_lines(text, |line| {
                        if line.is_empty() {
                            ">".to_owned()
                        } else {
                            format!("> {line}")
                        }
                    });
                    format!("\n{quoted}\n\n")
                }
            }
            "br" if inline && text.is_empty() => " ".to_owned(),
            "br" if inline => format!("{text} "),
            "br" => format!("  \n{text}"),
            "code" | "kbd" | "samp" => convert_code(&text, noformat),
            "div" | "article" | "section" | "dl" => {
                let text = strip(&text);
                if inline {
                    format!(" {text} ")
                } else if text.is_empty() {
                    String::new()
                } else {
                    format!("\n\n{text}\n\n")
                }
            }
            "dd" => {
                let text = strip(&text);
                if inline {
                    format!(" {text} ")
                } else if text.is_empty() {
                    "\n".to_owned()
                } else {
                    let indented = indent_lines(text, "    ");
                    format!(":{}\n", indented.chars().skip(1).collect::<String>())
                }
            }
            "dt" => {
                let text = collapse_all_whitespace(strip(&text));
                if inline {
                    format!(" {text} ")
                } else if text.is_empty() {
                    "\n".to_owned()
                } else {
                    format!("\n\n{text}\n")
                }
            }
            "hr" => "\n\n---\n\n".to_owned(),
            "img" => self.convert_img(node, inline),
            "video" => self.convert_video(node, &text, inline),
            "ul" | "ol" | "list" => {
                let before_paragraph = self
                    .next_block_content_sibling(node)
                    .is_some_and(|next| !matches!(self.document.name(next), Some("ul" | "ol")));
                if tags.contains("li") {
                    format!("\n{}", text.trim_end_matches(is_python_space))
                } else {
                    format!("\n\n{text}{}", if before_paragraph { "\n" } else { "" })
                }
            }
            "li" => self.convert_li(node, &text)?,
            "p" => {
                let text = strip_blank(&text);
                if inline {
                    format!(" {text} ")
                } else if text.is_empty() {
                    String::new()
                } else {
                    format!("\n\n{text}\n\n")
                }
            }
            "pre" if text.is_empty() => String::new(),
            "pre" => format!("\n\n```\n{}\n```\n\n", strip_pre(&text)),
            "q" => format!("\"{text}\""),
            "table" => format!("\n\n{}\n\n", strip(&text)),
            "caption" => format!("{}\n\n", strip(&text)),
            "figcaption" => format!("\n\n{}\n\n", strip(&text)),
            "td" | "th" => format!(
                " {}{}",
                strip(&text).replace('\n', " "),
                " |".repeat(colspan_of(self.document.attribute(node, "colspan"))?)
            ),
            "tr" => self.convert_tr(node, &text)?,
            // `script` and `style`, and the elements the reference's converter
            // subclass empties.
            _ => String::new(),
        })
    }

    fn convert_a(&self, node: usize, text: &str, noformat: bool) -> String {
        if noformat {
            return text.to_owned();
        }
        let (prefix, suffix, text) = chomp(text);
        if text.is_empty() {
            return String::new();
        }
        let href = self.document.attribute(node, "href");
        let title = self
            .document
            .attribute(node, "title")
            .filter(|title| !title.is_empty());
        if let Some(href) = href
            && title.is_none()
            && text.replace("\\_", "_") == href
        {
            return format!("<{href}>");
        }
        let title_part = title
            .map(|title| format!(" \"{}\"", title.replace('"', "\\\"")))
            .unwrap_or_default();
        match href.filter(|href| !href.is_empty()) {
            Some(href) => format!("{prefix}[{text}]({href}{title_part}){suffix}"),
            None => text.to_owned(),
        }
    }

    fn convert_img(&self, node: usize, inline: bool) -> String {
        let attribute = |name: &str| self.document.attribute(node, name).unwrap_or_default();
        let alt = attribute("alt");
        if inline {
            return alt.to_owned();
        }
        let title = attribute("title");
        let title_part = if title.is_empty() {
            String::new()
        } else {
            format!(" \"{}\"", title.replace('"', "\\\""))
        };
        format!("![{alt}]({}{title_part})", attribute("src"))
    }

    fn convert_video(&self, node: usize, text: &str, inline: bool) -> String {
        if inline {
            return text.to_owned();
        }
        let mut src = self.document.attribute(node, "src").unwrap_or_default();
        if src.is_empty()
            && let Some(source) = self
                .descendants_named(node, &["source"])
                .into_iter()
                .find(|source| self.document.attribute(*source, "src").is_some())
        {
            src = self.document.attribute(source, "src").unwrap_or_default();
        }
        let poster = self.document.attribute(node, "poster").unwrap_or_default();
        match (src.is_empty(), poster.is_empty()) {
            (false, false) => format!("[![{text}]({poster})]({src})"),
            (false, true) => format!("[{text}]({src})"),
            (true, false) => format!("![{text}]({poster})"),
            (true, true) => text.to_owned(),
        }
    }

    fn convert_li(&self, node: usize, text: &str) -> Result<String, ConversionError> {
        let text = strip(text);
        if text.is_empty() {
            return Ok("\n".to_owned());
        }
        let parent = self.document.parent(node);
        let bullet = if self.name(parent) == Some("ol") {
            let start = parent
                .and_then(|parent| self.document.attribute(parent, "start"))
                .filter(|start| {
                    !start.is_empty() && start.chars().all(|c| range_order(NUMERIC, c).is_some())
                });
            let start = match start {
                Some(start) => python_int(start)?,
                None => "1".to_owned(),
            };
            let mut earlier = 0;
            let mut current = self.document.previous_sibling(node);
            while let Some(sibling) = current {
                if self.document.name(sibling) == Some("li") {
                    earlier += 1;
                }
                current = self.document.previous_sibling(sibling);
            }
            format!("{}. ", add_decimal(&start, earlier)?)
        } else {
            "- ".to_owned()
        };
        let width = bullet.chars().count();
        let indented = indent_lines(text, &" ".repeat(width));
        let rest = indented.chars().skip(width).collect::<String>();
        Ok(format!("{bullet}{rest}\n"))
    }

    fn convert_tr(&self, node: usize, text: &str) -> Result<String, ConversionError> {
        let cells = self.descendants_named(node, &["td", "th"]);
        let is_first_row = self.previous_element_sibling(node).is_none();
        let parent = self.document.parent(node);
        let parent_name = self.name(parent).unwrap_or_default();
        let is_head_row = cells
            .iter()
            .all(|cell| self.document.name(*cell) == Some("th"))
            || (parent_name == "thead"
                && parent.is_some_and(|parent| self.descendants_named(parent, &["tr"]).len() == 1));
        let head_row_missing = is_first_row
            && (parent_name != "tbody"
                || parent
                    .and_then(|parent| self.document.parent(parent))
                    .is_some_and(|table| self.descendants_named(table, &["thead"]).is_empty()));
        let mut full_colspan = 0;
        for cell in &cells {
            full_colspan += colspan_of(self.document.attribute(*cell, "colspan"))?;
        }
        let rule = |cell: &str| format!("| {} |\n", vec![cell; full_colspan].join(" | "));
        let mut overline = String::new();
        let mut underline = String::new();
        if is_head_row && is_first_row {
            underline = rule("---");
        } else if head_row_missing
            || (is_first_row
                && (parent_name == "table"
                    || (parent_name == "tbody"
                        && parent.is_some_and(|parent| {
                            self.previous_element_sibling(parent).is_none()
                        }))))
        {
            overline = rule("") + &rule("---");
        }
        Ok(format!("{overline}|{text}\n{underline}"))
    }
}

#[cfg(test)]
mod markdown_tests;
