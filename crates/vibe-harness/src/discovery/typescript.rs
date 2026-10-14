//! The TypeScript declaration `search_tool_functions` shows for one function,
//! rendered from its JSON Schemas the way reference
//! `features/tool_discovery/declaration.rs` lays them out: one
//! `declare namespace tools.<group>` block holding the shared aliases, the
//! argument and result types, and the function itself.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

/// Deeper than this, a schema renders as `unknown`.
const DEPTH_LIMIT: usize = 64;
/// The constraints a JSDoc block states as `@keyword value` lines.
const STATED_LIMITS: [&str; 8] = [
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minLength",
    "maxLength",
    "minProperties",
    "maxProperties",
];

/// `name` with every character an identifier cannot hold turned into `_`,
/// and a leading `_` when it would start with a digit.
pub fn safe_identifier(name: &str) -> String {
    let mut safe: String = name
        .chars()
        .map(|character| {
            if character == '_' || character == '$' || character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect();
    if !safe.starts_with(|character: char| {
        character == '_' || character == '$' || character.is_ascii_alphabetic()
    }) {
        safe.insert(0, '_');
    }
    safe
}

/// The declaration block for `namespace.name`.
pub fn declaration(
    namespace: &str,
    name: &str,
    description: &str,
    input: &Value,
    output: Option<&Value>,
) -> String {
    let base = pascal_case(name);
    let mut arguments = Renderer::new(input, "Input");
    let argument_type = arguments.finish(input);
    let mut returned = output.map(|schema| Renderer::new(schema, "Output"));
    let returned_type = match (&mut returned, output) {
        (Some(renderer), Some(schema)) => renderer.finish(schema),
        _ => "unknown".to_owned(),
    };
    let mut blocks = arguments.aliases();
    if let Some(renderer) = returned {
        blocks.extend(renderer.aliases());
    }
    blocks.push(typed(&format!("{base}Arg"), input, &argument_type));
    blocks.push(typed(
        &format!("{base}Returned"),
        output.unwrap_or(&Value::Null),
        &returned_type,
    ));
    if let Some(doc) = jsdoc(description.lines().map(escaped).collect()) {
        blocks.push(doc);
    }
    blocks.push(format!(
        "function {name}(arg: {base}Arg): Promise<{base}Returned>;"
    ));
    let body: Vec<String> = blocks.iter().map(|block| indented(block, 2)).collect();
    format!(
        "declare namespace tools.{namespace} {{\n{}\n}}",
        body.join("\n")
    )
}

/// Renders one schema, collecting the aliases its `$ref`s name.
struct Renderer<'a> {
    root: &'a Value,
    prefix: &'static str,
    /// Alias name to its JSDoc and its type.
    declared: BTreeMap<String, (Option<String>, String)>,
    /// Reference to the alias it was given.
    named: BTreeMap<String, String>,
    taken: BTreeSet<String>,
    gave_up: bool,
}

impl<'a> Renderer<'a> {
    fn new(root: &'a Value, prefix: &'static str) -> Self {
        Self {
            root,
            prefix,
            declared: BTreeMap::new(),
            named: BTreeMap::new(),
            taken: BTreeSet::new(),
            gave_up: false,
        }
    }

    /// The type of `schema`, or `unknown` with no aliases when any part of
    /// it could not be rendered.
    fn finish(&mut self, schema: &Value) -> String {
        let rendered = self.render(schema, 0);
        if self.gave_up {
            self.declared.clear();
            return "unknown".to_owned();
        }
        rendered
    }

    fn aliases(self) -> Vec<String> {
        self.declared
            .into_iter()
            .map(|(name, (doc, value))| match doc {
                Some(doc) => format!("{doc}\ntype {name} = {value};"),
                None => format!("type {name} = {value};"),
            })
            .collect()
    }

    fn render(&mut self, schema: &Value, depth: usize) -> String {
        if depth >= DEPTH_LIMIT {
            self.gave_up = true;
            return "unknown".to_owned();
        }
        let rendered = self.render_plain(schema, depth);
        if schema["nullable"] == Value::Bool(true) {
            return one_of(vec![rendered, "null".to_owned()]);
        }
        rendered
    }

    fn render_plain(&mut self, schema: &Value, depth: usize) -> String {
        if let Some(reference) = schema["$ref"].as_str() {
            return self.alias_for(reference, depth);
        }
        if let Some(values) = schema["enum"].as_array() {
            return one_of(values.iter().map(literal).collect());
        }
        if let Some(value) = schema.get("const") {
            return literal(value);
        }
        if let Some(parts) = schema["allOf"].as_array() {
            let parts = parts
                .iter()
                .map(|part| self.render(part, depth + 1))
                .collect();
            return joined(parts, " & ");
        }
        if let Some(parts) = schema["oneOf"]
            .as_array()
            .or_else(|| schema["anyOf"].as_array())
        {
            let parts = parts
                .iter()
                .map(|part| self.render(part, depth + 1))
                .collect();
            return one_of(parts);
        }
        if let Some(kinds) = schema["type"].as_array() {
            let parts = kinds
                .iter()
                .map(|kind| {
                    let mut single = schema.clone();
                    single["type"] = kind.clone();
                    self.render_plain(&single, depth + 1)
                })
                .collect();
            return one_of(parts);
        }
        match schema["type"].as_str() {
            Some("string") => "string".to_owned(),
            Some("integer" | "number") => "number".to_owned(),
            Some("boolean") => "boolean".to_owned(),
            Some("null") => "null".to_owned(),
            Some("array") => self.array(schema, depth),
            Some("object") => self.object(schema, depth),
            _ if schema.get("properties").is_some()
                || schema.get("additionalProperties").is_some() =>
            {
                self.object(schema, depth)
            }
            _ if schema.get("items").is_some() || schema.get("prefixItems").is_some() => {
                self.array(schema, depth)
            }
            _ => "unknown".to_owned(),
        }
    }

    fn alias_for(&mut self, reference: &str, depth: usize) -> String {
        let target = reference
            .strip_prefix('#')
            .and_then(|pointer| self.root.pointer(pointer));
        let Some(target) = target else {
            self.gave_up = true;
            return "unknown".to_owned();
        };
        if let Some(name) = self.named.get(reference) {
            return name.clone();
        }
        let last = reference
            .rsplit('/')
            .next()
            .unwrap_or("Value")
            .replace("~1", "/")
            .replace("~0", "~");
        let base = format!("{}{}", self.prefix, pascal_case(&safe_identifier(&last)));
        let mut name = base.clone();
        let mut suffix = 2;
        while !self.taken.insert(name.clone()) {
            name = format!("{base}{suffix}");
            suffix += 1;
        }
        self.named.insert(reference.to_owned(), name.clone());
        let value = self.render(target, depth + 1);
        self.declared
            .insert(name.clone(), (schema_doc(target), value));
        name
    }

    fn array(&mut self, schema: &Value, depth: usize) -> String {
        let tuple = schema["prefixItems"]
            .as_array()
            .or_else(|| schema["items"].as_array());
        if let Some(items) = tuple {
            let items: Vec<String> = items
                .iter()
                .map(|item| self.render(item, depth + 1))
                .collect();
            return format!("[{}]", items.join(", "));
        }
        let item = match schema.get("items") {
            Some(item) if item.is_object() => self.render(item, depth + 1),
            _ => "unknown".to_owned(),
        };
        format!("Array<{item}>")
    }

    fn object(&mut self, schema: &Value, depth: usize) -> String {
        let properties = schema["properties"]
            .as_object()
            .filter(|properties| !properties.is_empty());
        let Some(properties) = properties else {
            return match schema.get("additionalProperties") {
                Some(Value::Bool(false)) => "Record<string, never>".to_owned(),
                Some(value) if value.is_object() => {
                    format!("Record<string, {}>", self.render(value, depth + 1))
                }
                _ => "Record<string, unknown>".to_owned(),
            };
        };
        let required: BTreeSet<&str> = schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let mut lines = vec!["{".to_owned()];
        for (name, property) in properties {
            if let Some(doc) = schema_doc(property) {
                lines.push(indented(&doc, 2));
            }
            let optional = if required.contains(name.as_str()) {
                ""
            } else {
                "?"
            };
            let rendered = self.render(property, depth + 1).replace('\n', "\n  ");
            lines.push(format!("  {}{optional}: {rendered};", property_name(name)));
        }
        lines.push("}".to_owned());
        lines.join("\n")
    }
}

/// `values` deduplicated, in sorted order, as a union.
fn one_of(values: Vec<String>) -> String {
    joined(values, " | ")
}

fn joined(values: Vec<String>, separator: &str) -> String {
    let distinct: BTreeSet<String> = values.into_iter().collect();
    if distinct.is_empty() {
        return "unknown".to_owned();
    }
    distinct.into_iter().collect::<Vec<_>>().join(separator)
}

fn literal(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "unknown".to_owned())
}

fn is_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first == '$' || first.is_ascii_alphabetic())
        && characters.all(|character| {
            character == '_' || character == '$' || character.is_ascii_alphanumeric()
        })
}

fn property_name(name: &str) -> String {
    if is_identifier(name) {
        name.to_owned()
    } else {
        literal(&Value::String(name.to_owned()))
    }
}

/// `snake_case` or a plain word as `PascalCase`.
fn pascal_case(identifier: &str) -> String {
    let joined: String = identifier
        .split('_')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut characters = word.chars();
            characters
                .next()
                .map(|first| format!("{}{}", first.to_ascii_uppercase(), characters.as_str()))
                .unwrap_or_default()
        })
        .collect();
    if joined.is_empty() {
        return "_".to_owned();
    }
    if joined.starts_with(|character: char| {
        character == '_' || character == '$' || character.is_ascii_alphabetic()
    }) {
        joined
    } else {
        format!("_{joined}")
    }
}

fn typed(name: &str, schema: &Value, value: &str) -> String {
    match schema_doc(schema) {
        Some(doc) => format!("{doc}\ntype {name} = {value};"),
        None => format!("type {name} = {value};"),
    }
}

/// A schema's description and stated limits as a JSDoc block.
fn schema_doc(schema: &Value) -> Option<String> {
    let mut lines: Vec<String> = schema["description"]
        .as_str()
        .into_iter()
        .flat_map(str::lines)
        .map(escaped)
        .collect();
    for keyword in STATED_LIMITS {
        if let Some(limit) = schema.get(keyword).filter(|limit| limit.is_number()) {
            lines.push(format!("@{keyword} {limit}"));
        }
    }
    jsdoc(lines)
}

fn jsdoc(lines: Vec<String>) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let mut block = vec!["/**".to_owned()];
    block.extend(lines.into_iter().map(|line| {
        if line.is_empty() {
            " *".to_owned()
        } else {
            format!(" * {line}")
        }
    }));
    block.push(" */".to_owned());
    Some(block.join("\n"))
}

fn escaped(line: &str) -> String {
    line.replace("*/", "*\\/")
}

fn indented(block: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    block
        .lines()
        .map(|line| format!("{pad}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}
