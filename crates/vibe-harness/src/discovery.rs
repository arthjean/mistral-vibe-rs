//! `search_tool_functions`: what a model learns of the functions
//! `run_typescript` can call.
//!
//! Reference `harness/core/src/core/features/tool_discovery/`: every callable
//! function is one document, searched with BM25 over its names, description
//! and declaration (`best_match`), listed per connector
//! (`all_connector_capabilities`), or described in full by exact name
//! (`details`). The answer is Markdown under a YAML front matter; the counts
//! it states also feed the discovery entry the history shows.

mod typescript;

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::Deserialize;
use serde_json::Value;

pub use typescript::safe_identifier;

const MAX_FUNCTIONS: usize = 10;
const MAX_CONNECTORS: usize = 10;
/// How many ranked functions a `best_match` answer names.
const MAX_CANDIDATES: usize = 20;

/// The arguments the model sends. Unknown fields are ignored, as the
/// reference's request type does.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchRequest {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub functions: Vec<String>,
    #[serde(default, rename = "functionNames")]
    pub function_names: Vec<String>,
    #[serde(default)]
    pub connectors: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    BestMatch,
    Details,
    AllConnectorCapabilities,
}

impl Mode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BestMatch => "best_match",
            Self::Details => "details",
            Self::AllConnectorCapabilities => "all_connector_capabilities",
        }
    }
}

impl SearchRequest {
    /// The request the arguments describe.
    ///
    /// # Errors
    ///
    /// The arguments do not deserialize, or a list is too long or holds an
    /// empty name; the message is what the model reads.
    pub fn parse(arguments: &Value) -> Result<Self, String> {
        let invalid = |reason: String| format!("invalid search_tool_functions arguments: {reason}");
        let request: Self = serde_json::from_value(arguments.clone())
            .map_err(|error| invalid(error.to_string()))?;
        for (field, values, limit) in [
            ("functions", &request.functions, MAX_FUNCTIONS),
            ("functionNames", &request.function_names, MAX_FUNCTIONS),
            ("connectors", &request.connectors, MAX_CONNECTORS),
        ] {
            if values.len() > limit {
                return Err(invalid(format!(
                    "{field} must contain at most {limit} items"
                )));
            }
            if values.iter().any(String::is_empty) {
                return Err(invalid(format!("{field} values must not be empty")));
            }
        }
        Ok(request)
    }
}

/// One function a program can call.
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    /// The connector or group, as written.
    pub connector: String,
    pub connector_guidance: String,
    /// The name the function was declared under.
    pub original: String,
    /// `original` as an identifier.
    pub name: String,
    /// `group.name`.
    pub lookup: String,
    pub description: String,
    pub declaration: String,
}

impl Function {
    #[must_use]
    pub fn new(
        connector: &str,
        connector_guidance: &str,
        name: &str,
        description: &str,
        input: &Value,
        output: Option<&Value>,
    ) -> Self {
        let group = safe_identifier(connector);
        let function = safe_identifier(name);
        Self {
            connector: connector.to_owned(),
            connector_guidance: connector_guidance.to_owned(),
            original: name.to_owned(),
            lookup: format!("{group}.{function}"),
            declaration: typescript::declaration(&group, &function, description, input, output),
            name: function,
            description: description.to_owned(),
        }
    }

    fn reference(&self) -> String {
        format!("tools.{}", self.lookup)
    }
}

/// What one search answered: the text the model reads and the counts the
/// discovery entry shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub content: String,
    pub mode: Mode,
    pub tool_count: usize,
    pub connector_notice_count: usize,
}

/// Runs `request` against `functions`.
#[must_use]
pub fn search(functions: &[Function], request: SearchRequest) -> Answer {
    let named: Vec<String> = request
        .functions
        .into_iter()
        .chain(request.function_names)
        .filter(|name| !name.trim().is_empty())
        .take(MAX_FUNCTIONS)
        .collect();
    let connectors: Vec<String> = request
        .connectors
        .into_iter()
        .filter(|name| !name.trim().is_empty())
        .take(MAX_CONNECTORS)
        .collect();
    if request.mode == Mode::Details || !named.is_empty() {
        return details(functions, &named);
    }
    match request.mode {
        Mode::AllConnectorCapabilities => capabilities(functions, &connectors),
        _ => best_match(functions, &connectors, request.query.as_deref()),
    }
}

fn empty(mode: Mode, message: &str) -> Answer {
    Answer {
        content: format!(
            "{}\n\n{message}",
            front_matter(mode, 0, 0, 0, Some(0), None, &[])
        ),
        mode,
        tool_count: 0,
        connector_notice_count: 0,
    }
}

const NOTHING_CALLABLE: &str = "No tool functions are available in run_typescript.";

fn best_match(functions: &[Function], connectors: &[String], query: Option<&str>) -> Answer {
    let mode = Mode::BestMatch;
    if functions.is_empty() {
        return empty(mode, NOTHING_CALLABLE);
    }
    let query = query.unwrap_or_default().trim();
    if query.is_empty() {
        return empty(
            mode,
            "No query provided. Describe the capability you need for best_match mode.",
        );
    }
    let candidates = of_connectors(functions, connectors);
    if !connectors.is_empty() && candidates.is_empty() {
        return empty(mode, &unknown_connectors(connectors));
    }
    let ranked = rank(&candidates, query);
    if ranked.is_empty() {
        let message = if connectors.is_empty() {
            format!(
                "No matching tool functions found for query \"{query}\". Try a broader search query."
            )
        } else {
            format!(
                "No matching tool functions found for query \"{query}\" in connectors: {}. Try a broader search query or omit the connector filter.",
                connectors.join(", ")
            )
        };
        return empty(mode, &message);
    }
    let related = ranked.len();
    let shown: Vec<&(f64, &Function)> = ranked.iter().take(MAX_CANDIDATES).collect();
    let listed: Vec<&Function> = shown.iter().map(|(_, function)| *function).collect();
    let guidance = guidance_of(&listed);
    let mut lines = vec![
        front_matter(
            mode,
            distinct_connectors(&listed),
            related,
            shown.len(),
            Some(guidance.len()),
            None,
            &[],
        ),
        String::new(),
        "# Best Matching Tool Candidates".to_owned(),
        String::new(),
    ];
    if related > MAX_CANDIDATES {
        lines.extend(["moreCandidatesAvailable: true".to_owned(), String::new()]);
    }
    lines.push(
        shown
            .iter()
            .enumerate()
            .map(|(index, (score, function))| {
                format!("{}. {} - {score:.1}", index + 1, function.lookup)
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );
    if !guidance.is_empty() {
        lines.extend([String::new(), guidance_section(&guidance)]);
    }
    Answer {
        content: lines.join("\n"),
        mode,
        tool_count: shown.len(),
        connector_notice_count: 0,
    }
}

fn capabilities(functions: &[Function], connectors: &[String]) -> Answer {
    let mode = Mode::AllConnectorCapabilities;
    if functions.is_empty() {
        return empty(mode, NOTHING_CALLABLE);
    }
    if connectors.is_empty() {
        return empty(
            mode,
            "All_connector_capabilities mode requires at least one exact connector name.",
        );
    }
    let listed = of_connectors(functions, connectors);
    if listed.is_empty() {
        return empty(mode, &unknown_connectors(connectors));
    }
    let mut names: Vec<&str> = listed
        .iter()
        .map(|function| function.lookup.as_str())
        .collect();
    names.sort_unstable();
    let body = names
        .iter()
        .enumerate()
        .map(|(index, name)| format!("{}. {name}", index + 1))
        .collect::<Vec<_>>();
    Answer {
        content: [
            front_matter(
                mode,
                distinct_connectors(&listed),
                names.len(),
                names.len(),
                Some(0),
                None,
                &[],
            ),
            String::new(),
            "# All Connector Capabilities".to_owned(),
            String::new(),
            body.join("\n"),
        ]
        .join("\n"),
        mode,
        tool_count: listed.len(),
        connector_notice_count: 0,
    }
}

fn details(functions: &[Function], named: &[String]) -> Answer {
    let mode = Mode::Details;
    if functions.is_empty() {
        return empty(mode, NOTHING_CALLABLE);
    }
    let mut loaded: Vec<&Function> = Vec::new();
    let mut missing: Vec<Option<&str>> = Vec::new();
    if named.is_empty() {
        missing.push(None);
    }
    for name in named {
        match find(functions, name) {
            Some(function) => {
                if !loaded.iter().any(|seen| seen.lookup == function.lookup) {
                    loaded.push(function);
                }
            }
            None => missing.push(Some(name)),
        }
    }
    let body = if loaded.is_empty() {
        "No exact tool function details loaded. Call best_match/all_connector_capabilities first, then request exact returned function names.".to_owned()
    } else {
        loaded
            .iter()
            .enumerate()
            .map(|(index, function)| described(function, index))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let mut lines = vec![
        front_matter(
            mode,
            distinct_connectors(&loaded),
            loaded.len(),
            loaded.len(),
            Some(0),
            Some((named.len(), loaded.len())),
            &loaded,
        ),
        String::new(),
        "# Tool Function Details".to_owned(),
        String::new(),
        body,
    ];
    if !missing.is_empty() {
        let reasons = missing
            .iter()
            .map(|name| match name {
                Some(name) => format!("- No exact tool function found for: {name}"),
                None => "- Details mode requires at least one exact function name returned by best_match or all_connector_capabilities mode.".to_owned(),
            })
            .collect::<Vec<_>>();
        lines.extend([
            String::new(),
            "# Missing Exact Names".to_owned(),
            String::new(),
            reasons.join("\n"),
        ]);
    }
    Answer {
        content: lines.join("\n"),
        mode,
        tool_count: loaded.len(),
        connector_notice_count: 0,
    }
}

fn unknown_connectors(connectors: &[String]) -> String {
    format!(
        "No exact connectors found for: {}. Try a connector name returned by best_match mode.",
        connectors.join(", ")
    )
}

/// The functions of the named connectors, matched exactly or as
/// identifiers, in connector order; every function when none is named.
fn of_connectors<'a>(functions: &'a [Function], connectors: &[String]) -> Vec<&'a Function> {
    if connectors.is_empty() {
        return functions.iter().collect();
    }
    let mut seen = HashSet::new();
    let mut matched = Vec::new();
    for connector in connectors {
        let exact = connector.trim();
        let safe = safe_identifier(exact);
        for function in functions {
            if (function.connector == exact || safe_identifier(&function.connector) == safe)
                && seen.insert(function.lookup.clone())
            {
                matched.push(function);
            }
        }
    }
    matched
}

fn find<'a>(functions: &'a [Function], name: &str) -> Option<&'a Function> {
    let exact = name.trim();
    let lookup = exact.strip_prefix("tools.").unwrap_or(exact);
    let safe = safe_identifier(exact);
    functions.iter().find(|function| {
        function.lookup == lookup
            || function.reference() == exact
            || function.original == exact
            || function.name == exact
            || function.name == safe
    })
}

fn distinct_connectors(functions: &[&Function]) -> usize {
    functions
        .iter()
        .map(|function| function.connector.as_str())
        .collect::<BTreeSet<_>>()
        .len()
}

/// The front matter every answer starts with.
fn front_matter(
    mode: Mode,
    connectors: usize,
    related: usize,
    relevant: usize,
    notices: Option<usize>,
    loading: Option<(usize, usize)>,
    loaded: &[&Function],
) -> String {
    let mut lines = vec![
        "---".to_owned(),
        format!("mode: {}", mode.as_str()),
        format!("relevantFunctionCount: {relevant}"),
        format!("relatedFunctionCount: {related}"),
        format!("relatedConnectorCount: {connectors}"),
    ];
    if let Some(notices) = notices {
        lines.push(format!("relevantConnectorNoticeCount: {notices}"));
    }
    if let Some((requested, count)) = loading {
        lines.push(format!("requestedFunctionCount: {requested}"));
        lines.push(format!("loadedFunctionCount: {count}"));
    }
    if !loaded.is_empty() {
        lines.push("loadedFunctions:".to_owned());
        for function in loaded {
            lines.push(format!("  - name: {}", yaml_scalar(&function.lookup)));
            lines.push(format!(
                "    connector: {}",
                yaml_scalar(&function.connector)
            ));
        }
    }
    lines.push("---".to_owned());
    lines.join("\n")
}

/// `value` as YAML writes it: bare when that reads back as the same string,
/// single-quoted otherwise.
fn yaml_scalar(value: &str) -> String {
    if reads_back_bare(value) {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "''"))
    }
}

fn reads_back_bare(value: &str) -> bool {
    if value.is_empty() || value.trim() != value || value.contains(['\n', '\r', ':']) {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    let reserved = [
        "~", "null", "y", "yes", "n", "no", "true", "false", "on", "off", ".nan", ".inf", "+.inf",
        "-.inf",
    ];
    if reserved.contains(&lower.as_str()) || value.parse::<f64>().is_ok() || looks_like_date(value)
    {
        return false;
    }
    if value.starts_with(|first: char| "-?:,[]{}#&*!|>'\"%@`".contains(first)) {
        return false;
    }
    !value.contains(" #") && !value.contains("\t#")
}

fn looks_like_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    bytes.len() >= 10
        && digits(0..4)
        && bytes[4] == b'-'
        && digits(5..7)
        && bytes[7] == b'-'
        && digits(8..10)
}

fn described(function: &Function, index: usize) -> String {
    let description = function
        .description
        .trim()
        .lines()
        .map(|line| {
            if line.trim().is_empty() {
                "//".to_owned()
            } else {
                format!("// {}", line.trim())
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "```ts\n// Result {}.\n// connector: {}\n// function: {}\n// tool: {}\n// description:\n{description}\n\n{}\n```",
        index + 1,
        function.connector,
        function.lookup,
        function.reference(),
        function.declaration.trim()
    )
}

/// One function per connector that has guidance to give, first seen first.
fn guidance_of<'a>(functions: &[&'a Function]) -> Vec<&'a Function> {
    let mut seen = HashSet::new();
    functions
        .iter()
        .filter(|function| {
            !function.connector_guidance.trim().is_empty()
                && seen.insert(function.connector.clone())
        })
        .copied()
        .collect()
}

fn guidance_section(functions: &[&Function]) -> String {
    let entries: Vec<String> = functions
        .iter()
        .map(|function| {
            let guidance: Vec<&str> = function
                .connector_guidance
                .trim()
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect();
            let mut lines = vec![
                format!("    {}", xml("name", &function.connector)),
                format!("    {}", xml("status", "connected")),
            ];
            if !guidance.is_empty() {
                lines.push(format!("    {}", xml("guidance", &guidance.join("\n"))));
            }
            format!("  <connector>\n{}\n  </connector>", lines.join("\n"))
        })
        .collect();
    format!(
        "# Connector Guidance\n\n{}",
        xml("connector-guidance", &format!("\n{}\n", entries.join("\n")))
    )
}

/// `content` inside `<tag>`, with any tag of the same name it holds escaped
/// so it cannot close the element early.
fn xml(tag: &str, content: &str) -> String {
    let lower = content.to_ascii_lowercase();
    let opening = format!("<{tag}");
    let closing = format!("</{tag}");
    let mut escaped = String::with_capacity(content.len());
    let mut cursor = 0;
    while cursor < content.len() {
        let next = [&opening, &closing]
            .iter()
            .filter_map(|needle| {
                lower[cursor..]
                    .match_indices(needle.as_str())
                    .find_map(|(offset, _)| {
                        let start = cursor + offset;
                        let after = lower[start + needle.len()..].chars().next();
                        after
                            .is_none_or(|character| {
                                character == '>' || character == '/' || character.is_whitespace()
                            })
                            .then_some(start)
                    })
            })
            .min();
        let Some(start) = next else {
            escaped.push_str(&content[cursor..]);
            break;
        };
        escaped.push_str(&content[cursor..start]);
        let Some(length) = content[start..].find('>') else {
            escaped.push_str(&content[start..]);
            break;
        };
        let end = start + length + 1;
        escaped.push_str(
            &content[start..end]
                .replace('<', "&lt;")
                .replace('>', "&gt;"),
        );
        cursor = end;
    }
    format!("<{tag}>{escaped}</{tag}>")
}

/// BM25 over each function's fields, names and description counting double.
fn rank<'a>(functions: &[&'a Function], query: &str) -> Vec<(f64, &'a Function)> {
    let wanted: BTreeSet<String> = tokens(query).into_iter().collect();
    if functions.is_empty() || wanted.is_empty() {
        return Vec::new();
    }
    struct Counts {
        length: usize,
        frequency: HashMap<String, usize>,
        weight: HashMap<String, f64>,
    }
    let counts: Vec<Counts> = functions
        .iter()
        .map(|function| {
            let reference = function.reference();
            let fields: [(&str, f64); 8] = [
                (&function.name, 2.0),
                (&function.lookup, 2.0),
                (&reference, 2.0),
                (&function.original, 2.0),
                (&function.connector, 1.0),
                (&function.description, 2.0),
                (&function.connector_guidance, 1.0),
                (&function.declaration, 1.0),
            ];
            let mut counts = Counts {
                length: 0,
                frequency: HashMap::new(),
                weight: HashMap::new(),
            };
            for (text, weight) in fields {
                let words = tokens(text);
                counts.length += words.len();
                for word in words.into_iter().filter(|word| wanted.contains(word)) {
                    *counts.frequency.entry(word.clone()).or_default() += 1;
                    let kept = counts.weight.entry(word).or_insert(weight);
                    *kept = kept.max(weight);
                }
            }
            counts
        })
        .collect();
    let total = functions.len() as f64;
    let average = counts
        .iter()
        .map(|counts| counts.length as f64)
        .sum::<f64>()
        / total;
    let documents_with: HashMap<&str, f64> = wanted
        .iter()
        .map(|word| {
            let found = counts
                .iter()
                .filter(|counts| counts.frequency.contains_key(word))
                .count();
            (word.as_str(), found as f64)
        })
        .collect();
    let mut ranked: Vec<(f64, &Function)> = functions
        .iter()
        .zip(&counts)
        .filter_map(|(function, counts)| {
            let ratio = if average == 0.0 {
                0.0
            } else {
                counts.length as f64 / average
            };
            let normalization = 1.2 * (0.25 + 0.75 * ratio);
            let score: f64 = wanted
                .iter()
                .filter_map(|word| {
                    let frequency = *counts.frequency.get(word)? as f64;
                    let found = documents_with[word.as_str()];
                    let idf = (1.0 + (total - found + 0.5) / (found + 0.5)).ln();
                    let weight = counts.weight.get(word).copied().unwrap_or(1.0);
                    Some(idf * weight * (frequency * 2.2) / (frequency + normalization))
                })
                .sum();
            (score > 0.0).then_some((score, *function))
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.1.lookup.cmp(&right.1.lookup))
    });
    ranked
}

/// Lowercase ASCII words, camelCase and acronym boundaries split apart.
fn tokens(text: &str) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    let mut spaced = String::with_capacity(text.len());
    for (index, &character) in characters.iter().enumerate() {
        let previous = index.checked_sub(1).map(|before| characters[before]);
        let next = characters.get(index + 1).copied();
        if character.is_ascii_uppercase()
            && (previous
                .is_some_and(|before| before.is_ascii_lowercase() || before.is_ascii_digit())
                || (previous.is_some_and(|before| before.is_ascii_uppercase())
                    && next.is_some_and(|after| after.is_ascii_lowercase())))
        {
            spaced.push(' ');
        }
        spaced.push(character.to_ascii_lowercase());
    }
    spaced
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod discovery_tests;
