use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io::{self, Write};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::engine::{ToolExecutor, ToolFuture, ToolStreamSink};
use crate::matching::NameFilter;
use crate::text::truncate_utf8;

pub mod builtins;
pub mod config;
pub mod reference_text;
pub mod shell;

#[cfg(test)]
mod coercion_tests;

pub const DEFAULT_MAX_TOOL_OUTPUT_BYTES: usize = 1_048_576;
pub(crate) const MAX_TOOL_ERROR_BYTES: usize = 16_384;

pub type ToolHandlerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolExecutionOutput, ToolError>> + Send + 'a>>;
pub type OwnedToolHandlerFuture =
    Pin<Box<dyn Future<Output = Result<ToolExecutionOutput, ToolError>> + Send + 'static>>;

#[derive(Clone)]
pub struct ToolOutputSink {
    stream: Option<ToolStreamSink>,
    streamed_bytes: Arc<AtomicU64>,
    max_output_bytes: usize,
}

impl ToolOutputSink {
    fn new(stream: Option<ToolStreamSink>, max_output_bytes: usize) -> Self {
        Self {
            stream,
            streamed_bytes: Arc::new(AtomicU64::new(0)),
            max_output_bytes,
        }
    }

    #[must_use]
    pub fn discard(max_output_bytes: usize) -> Self {
        Self::new(None, max_output_bytes)
    }

    #[cfg(test)]
    pub(crate) fn test_streaming(stream: ToolStreamSink, max_output_bytes: usize) -> Self {
        Self::new(Some(stream), max_output_bytes)
    }

    pub fn emit(&self, chunk: impl Into<String>) -> Result<(), ToolError> {
        let chunk = chunk.into();
        let chunk_bytes = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        let previous = self
            .streamed_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_add(chunk_bytes))
            })
            .unwrap_or(u64::MAX);
        let actual = previous.saturating_add(chunk_bytes);
        let limit = u64::try_from(self.max_output_bytes).unwrap_or(u64::MAX);
        if actual > limit {
            return Err(ToolError::OutputTooLarge {
                actual: usize::try_from(actual).unwrap_or(usize::MAX),
                limit: self.max_output_bytes,
            });
        }
        if let Some(stream) = &self.stream {
            stream(chunk).map_err(ToolError::Execution)?;
        }
        Ok(())
    }

    fn bytes(&self) -> usize {
        usize::try_from(self.streamed_bytes.load(Ordering::Acquire)).unwrap_or(usize::MAX)
    }

    #[must_use]
    pub fn remaining_bytes(&self) -> usize {
        self.max_output_bytes.saturating_sub(self.bytes())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAvailability {
    Available,
    Disabled,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPresentationKind {
    Generic,
    Read,
    Search,
    Diff,
    Shell,
    Mcp,
    Connector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    BuiltIn,
    Mcp,
    Connector,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub state: Value,
    pub availability: ToolAvailability,
    pub presentation: ToolPresentationKind,
    pub source: ToolSource,
    #[serde(default)]
    pub selection_priority: i32,
}

impl ToolSpec {
    pub fn validate(&self) -> Result<(), ToolError> {
        validate_tool_name(&self.name)?;
        validate_schema_object(&self.input_schema, "input")?;
        if let Some(schema) = &self.output_schema {
            validate_output_schema(schema)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInvocation {
    pub call_id: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionOutput {
    pub typed_result: Value,
    pub model_text: String,
    #[serde(default)]
    pub display: Value,
    #[serde(default)]
    pub chunks: Vec<String>,
}

impl ToolExecutionOutput {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            typed_result: Value::String(text.clone()),
            model_text: text,
            display: Value::Null,
            chunks: Vec::new(),
        }
    }
}

pub trait ToolHandler: Send + Sync {
    fn invoke<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
        output: ToolOutputSink,
    ) -> ToolHandlerFuture<'a>;
}

impl<F> ToolHandler for F
where
    F: Fn(&ToolInvocation, ToolOutputSink) -> OwnedToolHandlerFuture + Send + Sync,
{
    fn invoke<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
        output: ToolOutputSink,
    ) -> ToolHandlerFuture<'a> {
        self(invocation, output)
    }
}

/// A runtime prerequisite re-read every time the surface is published.
///
/// The reference asks each tool class `is_available` on every `available_tools`
/// access, so a prerequisite that disappears mid-session withdraws the tool at
/// the next publication rather than at call time.
pub type ToolCondition = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Clone)]
struct RegisteredTool {
    spec: ToolSpec,
    handler: Arc<dyn ToolHandler>,
    discovery_index: u64,
    /// Who published this name, so a second claim on it can name the first.
    origin: String,
    /// The prerequisite this tool is published under, when it has one.
    condition: Option<ToolCondition>,
}

impl RegisteredTool {
    fn available(&self) -> bool {
        self.spec.availability == ToolAvailability::Available
            && self.condition.as_ref().is_none_or(|condition| condition())
    }
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Arc<RwLock<BTreeMap<String, RegisteredTool>>>,
    next_discovery_index: Arc<AtomicU64>,
    max_output_bytes: usize,
    /// How a slash-invoked skill resolves for this session, installed when the
    /// `skill` tool registers so the synthetic pair and the tool answer from
    /// one catalog and one loaded ledger.
    invoked_skills: Arc<RwLock<Option<Arc<dyn crate::skills::InvokedSkillResolver>>>>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolRegistry")
            .field(
                "tool_count",
                &self.tools.read().map_or(0, |tools| tools.len()),
            )
            .field("max_output_bytes", &self.max_output_bytes)
            .finish_non_exhaustive()
    }
}

impl PartialEq for ToolRegistry {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.tools, &other.tools) && self.max_output_bytes == other.max_output_bytes
    }
}

impl Eq for ToolRegistry {}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_TOOL_OUTPUT_BYTES)
    }
}

impl ToolRegistry {
    #[must_use]
    pub fn new(max_output_bytes: usize) -> Self {
        Self {
            tools: Arc::new(RwLock::new(BTreeMap::new())),
            next_discovery_index: Arc::new(AtomicU64::new(1)),
            max_output_bytes,
            invoked_skills: Arc::new(RwLock::new(None)),
        }
    }

    /// Installs the session's invoked-skill resolver, replacing any earlier
    /// one: a re-registration after an agent switch re-reads the discovery it
    /// was handed then.
    pub fn set_invoked_skills(&self, resolver: Arc<dyn crate::skills::InvokedSkillResolver>) {
        if let Ok(mut slot) = self.invoked_skills.write() {
            *slot = Some(resolver);
        }
    }

    /// The invoked-skill resolver, absent until the `skill` tool registers.
    #[must_use]
    pub fn invoked_skills(&self) -> Option<Arc<dyn crate::skills::InvokedSkillResolver>> {
        self.invoked_skills
            .read()
            .ok()
            .and_then(|slot| slot.clone())
    }

    pub fn register(
        &self,
        spec: ToolSpec,
        handler: Arc<dyn ToolHandler>,
    ) -> Result<RegistrationOutcome, ToolError> {
        let origin = format!("{:?} tool `{}`", spec.source, spec.name);
        self.insert(spec, handler, origin, false, None)
    }

    /// Registers a tool published only while `condition` holds.
    ///
    /// The condition is re-read at publication rather than frozen here, so a
    /// prerequisite that appears or disappears mid-session moves the tool in and
    /// out of the surface the way the reference `is_available` does.
    pub fn register_conditional(
        &self,
        spec: ToolSpec,
        handler: Arc<dyn ToolHandler>,
        condition: ToolCondition,
    ) -> Result<RegistrationOutcome, ToolError> {
        let origin = format!("{:?} tool `{}`", spec.source, spec.name);
        self.insert(spec, handler, origin, false, Some(condition))
    }

    /// Registers a tool that must own its published name outright.
    ///
    /// Remote providers publish `{alias}_{tool}`, a rule two different
    /// providers can land on for the same string. Priority-based replacement
    /// would let the later claim silently shadow the earlier one, so an
    /// exclusive registration that finds the name taken is rejected and both
    /// sources are named.
    pub fn register_exclusive(
        &self,
        spec: ToolSpec,
        handler: Arc<dyn ToolHandler>,
        origin: impl Into<String>,
    ) -> Result<RegistrationOutcome, ToolError> {
        self.insert(spec, handler, origin.into(), true, None)
    }

    fn insert(
        &self,
        spec: ToolSpec,
        handler: Arc<dyn ToolHandler>,
        origin: String,
        exclusive: bool,
        condition: Option<ToolCondition>,
    ) -> Result<RegistrationOutcome, ToolError> {
        spec.validate()?;
        let discovery_index = self.next_discovery_index.fetch_add(1, Ordering::Relaxed);
        let mut tools = self
            .tools
            .write()
            .map_err(|_| ToolError::RegistryPoisoned)?;
        // A provider re-registering its own tool is a refresh, not a second
        // claim on the name, so only a different origin is a duplicate.
        if exclusive
            && let Some(existing) = tools.get(&spec.name)
            && existing.origin != origin
        {
            return Err(ToolError::DuplicateName {
                name: spec.name.clone(),
                existing: existing.origin.clone(),
                incoming: origin,
            });
        }
        let replace = tools.get(&spec.name).is_none_or(|existing| {
            (spec.selection_priority, discovery_index)
                > (existing.spec.selection_priority, existing.discovery_index)
        });
        if replace {
            let outcome = if tools.contains_key(&spec.name) {
                RegistrationOutcome::Replaced
            } else {
                RegistrationOutcome::Inserted
            };
            tools.insert(
                spec.name.clone(),
                RegisteredTool {
                    spec,
                    handler,
                    discovery_index,
                    origin,
                    condition,
                },
            );
            Ok(outcome)
        } else {
            Ok(RegistrationOutcome::IgnoredLowerPriority)
        }
    }

    pub fn list(&self) -> Result<Vec<ToolSpec>, ToolError> {
        let tools = self.tools.read().map_err(|_| ToolError::RegistryPoisoned)?;
        Ok(tools.values().map(|tool| tool.spec.clone()).collect())
    }

    /// The surface a session publishes: every available tool the filters keep.
    ///
    /// Reference `available_tools` narrows with `enabled_tools` only when that
    /// list carries an entry, and applies `disabled_tools` last, so a name both
    /// lists match is withheld.
    pub fn available(
        &self,
        enabled: &NameFilter,
        disabled: &NameFilter,
    ) -> Result<Vec<ToolSpec>, ToolError> {
        let tools = self.tools.read().map_err(|_| ToolError::RegistryPoisoned)?;
        Ok(tools
            .values()
            .filter(|tool| tool.available())
            .map(|tool| tool.spec.clone())
            .filter(|spec| enabled.is_empty() || enabled.matches(&spec.name))
            .filter(|spec| !disabled.matches(&spec.name))
            .collect())
    }

    /// The registered names no publication can carry, because the prerequisite
    /// they were registered under does not hold.
    pub fn withheld(&self) -> Result<Vec<String>, ToolError> {
        let tools = self.tools.read().map_err(|_| ToolError::RegistryPoisoned)?;
        Ok(tools
            .values()
            .filter(|tool| {
                tool.condition
                    .as_ref()
                    .is_some_and(|condition| !condition())
            })
            .map(|tool| tool.spec.name.clone())
            .collect())
    }

    pub fn set_availability(
        &self,
        name: &str,
        source: ToolSource,
        availability: ToolAvailability,
    ) -> Result<bool, ToolError> {
        let mut tools = self
            .tools
            .write()
            .map_err(|_| ToolError::RegistryPoisoned)?;
        let Some(registered) = tools.get_mut(name) else {
            return Ok(false);
        };
        if registered.spec.source != source {
            return Ok(false);
        }
        registered.spec.availability = availability;
        Ok(true)
    }

    pub fn set_availabilities(
        &self,
        source: ToolSource,
        updates: &[(String, ToolAvailability)],
    ) -> Result<bool, ToolError> {
        let mut tools = self
            .tools
            .write()
            .map_err(|_| ToolError::RegistryPoisoned)?;
        if updates.iter().any(|(name, _)| {
            tools
                .get(name)
                .is_none_or(|registered| registered.spec.source != source)
        }) {
            return Ok(false);
        }
        for (name, availability) in updates {
            if let Some(registered) = tools.get_mut(name) {
                registered.spec.availability = *availability;
            }
        }
        Ok(true)
    }

    pub async fn invoke(
        &self,
        name: &str,
        invocation: ToolInvocation,
    ) -> Result<ToolExecutionOutput, ToolError> {
        self.invoke_stream(name, invocation, None).await
    }

    pub async fn invoke_stream(
        &self,
        name: &str,
        mut invocation: ToolInvocation,
        stream: Option<ToolStreamSink>,
    ) -> Result<ToolExecutionOutput, ToolError> {
        let registered = {
            let tools = self.tools.read().map_err(|_| ToolError::RegistryPoisoned)?;
            tools
                .get(name)
                .cloned()
                .ok_or_else(|| ToolError::Unavailable(name.to_owned()))?
        };
        if !registered.available() {
            return Err(ToolError::Unavailable(name.to_owned()));
        }
        coerce_and_validate(&mut invocation.arguments, &registered.spec.input_schema)?;
        apply_defaults(&mut invocation.arguments, &registered.spec.input_schema);
        let output = ToolOutputSink::new(stream, self.max_output_bytes);
        let mut result =
            match AssertUnwindSafe(registered.handler.invoke(&invocation, output.clone()))
                .catch_unwind()
                .await
                .map_err(|_| ToolError::Panicked(name.to_owned()))?
            {
                Ok(result) => result,
                Err(error) => return Err(error.bounded()),
            };
        for chunk in std::mem::take(&mut result.chunks) {
            output.emit(chunk)?;
        }
        if let Some(schema) = &registered.spec.output_schema {
            validate_at(&result.typed_result, schema, schema, "$result", 0)?;
        }
        let non_json_bytes = result.model_text.len().saturating_add(output.bytes());
        if non_json_bytes > self.max_output_bytes {
            return Err(ToolError::OutputTooLarge {
                actual: non_json_bytes,
                limit: self.max_output_bytes,
            });
        }
        let mut encoded = LimitedWriter::new(self.max_output_bytes - non_json_bytes);
        if let Err(error) = serde_json::to_writer(&mut encoded, &result.typed_result) {
            if encoded.exceeded {
                return Err(ToolError::OutputTooLarge {
                    actual: self.max_output_bytes.saturating_add(1),
                    limit: self.max_output_bytes,
                });
            }
            return Err(ToolError::InvalidResult(
                truncate_utf8(&error.to_string(), MAX_TOOL_ERROR_BYTES).to_owned(),
            ));
        }
        let total_bytes = non_json_bytes.saturating_add(encoded.written);
        if total_bytes > self.max_output_bytes {
            return Err(ToolError::OutputTooLarge {
                actual: total_bytes,
                limit: self.max_output_bytes,
            });
        }
        Ok(result)
    }
}

impl ToolExecutor for ToolRegistry {
    fn execute<'a>(&'a self, name: &'a str, arguments: &'a str) -> ToolFuture<'a> {
        Box::pin(async move {
            let arguments = serde_json::from_str(arguments)
                .map_err(|error| format!("invalid tool arguments: {error}"))?;
            self.invoke(
                name,
                ToolInvocation {
                    call_id: String::new(),
                    arguments,
                },
            )
            .await
            .map_err(|error| error.to_string())
        })
    }

    fn execute_stream<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a str,
        output: ToolStreamSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let arguments = serde_json::from_str(arguments)
                .map_err(|error| format!("invalid tool arguments: {error}"))?;
            self.invoke_stream(
                name,
                ToolInvocation {
                    call_id: String::new(),
                    arguments,
                },
                Some(output),
            )
            .await
            .map_err(|error| error.to_string())
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationOutcome {
    Inserted,
    Replaced,
    IgnoredLowerPriority,
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool registry lock is poisoned")]
    RegistryPoisoned,
    #[error("invalid tool name `{0}`")]
    InvalidName(String),
    #[error("tool `{name}` is published by two sources: {existing}, {incoming}")]
    DuplicateName {
        name: String,
        existing: String,
        incoming: String,
    },
    #[error("tool `{0}` is unavailable")]
    Unavailable(String),
    #[error("tool `{0}` panicked during execution")]
    Panicked(String),
    #[error("invalid {kind} schema: {message}")]
    InvalidSchema { kind: &'static str, message: String },
    #[error("schema validation failed at {path}: {message}")]
    SchemaViolation { path: String, message: String },
    #[error("invalid typed tool result: {0}")]
    InvalidResult(String),
    #[error("tool output is {actual} bytes, exceeding the {limit}-byte limit")]
    OutputTooLarge { actual: usize, limit: usize },
    #[error("{0}")]
    Execution(String),
}

impl ToolError {
    fn bounded(self) -> Self {
        match self {
            Self::Execution(message) => {
                Self::Execution(truncate_utf8(&message, MAX_TOOL_ERROR_BYTES).to_owned())
            }
            Self::InvalidResult(message) => {
                Self::InvalidResult(truncate_utf8(&message, MAX_TOOL_ERROR_BYTES).to_owned())
            }
            error => error,
        }
    }
}

struct LimitedWriter {
    written: usize,
    limit: usize,
    exceeded: bool,
}

impl LimitedWriter {
    fn new(limit: usize) -> Self {
        Self {
            written: 0,
            limit,
            exceeded: false,
        }
    }
}

impl Write for LimitedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() > self.limit.saturating_sub(self.written) {
            self.exceeded = true;
            return Err(io::Error::other("tool output limit exceeded"));
        }
        self.written = self.written.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn validate_tool_name(name: &str) -> Result<(), ToolError> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(ToolError::InvalidName(name.to_owned()));
    }
    Ok(())
}

fn validate_schema_object(schema: &Value, kind: &'static str) -> Result<(), ToolError> {
    let object = schema.as_object().ok_or_else(|| ToolError::InvalidSchema {
        kind,
        message: "schema must be an object".to_owned(),
    })?;
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return Err(ToolError::InvalidSchema {
            kind,
            message: "root type must be object".to_owned(),
        });
    }
    let properties = match object.get("properties") {
        None => Map::new(),
        Some(Value::Object(properties)) => properties.clone(),
        Some(_) => {
            return Err(ToolError::InvalidSchema {
                kind,
                message: "properties must be an object".to_owned(),
            });
        }
    };
    if let Some(required) = object.get("required") {
        let names = required
            .as_array()
            .ok_or_else(|| ToolError::InvalidSchema {
                kind,
                message: "required must be an array of strings".to_owned(),
            })?;
        for name in names {
            let name = name.as_str().ok_or_else(|| ToolError::InvalidSchema {
                kind,
                message: "required must be an array of strings".to_owned(),
            })?;
            if !properties.contains_key(name) {
                return Err(ToolError::InvalidSchema {
                    kind,
                    message: format!("required property `{name}` is absent from properties"),
                });
            }
        }
    }
    if let Some(defs) = object.get("$defs")
        && !defs.is_object()
    {
        return Err(ToolError::InvalidSchema {
            kind,
            message: "$defs must be an object".to_owned(),
        });
    }
    validate_references(schema, schema, "", kind)
}

/// Rejects a `$ref` that no `$defs` entry resolves, so a broken pointer fails at
/// registration instead of reaching the model as a schema nothing can satisfy.
fn validate_references(
    node: &Value,
    root: &Value,
    pointer: &str,
    kind: &'static str,
) -> Result<(), ToolError> {
    match node {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref") {
                let target = reference.as_str().ok_or_else(|| ToolError::InvalidSchema {
                    kind,
                    message: format!("$ref at {pointer} must be a string"),
                })?;
                if resolve_reference(target, root).is_none() {
                    return Err(ToolError::InvalidSchema {
                        kind,
                        message: format!("unresolved $ref `{target}` at {pointer}/$ref"),
                    });
                }
            }
            for (key, value) in object {
                validate_references(value, root, &format!("{pointer}/{key}"), kind)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                validate_references(item, root, &format!("{pointer}/{index}"), kind)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn resolve_reference<'a>(target: &str, root: &'a Value) -> Option<&'a Value> {
    let name = target.strip_prefix("#/$defs/")?;
    root.get("$defs")?.get(name)
}

fn validate_output_schema(schema: &Value) -> Result<(), ToolError> {
    let object = schema.as_object().ok_or_else(|| ToolError::InvalidSchema {
        kind: "output",
        message: "schema must be an object".to_owned(),
    })?;
    if object.get("type").and_then(Value::as_str).is_none() {
        return Err(ToolError::InvalidSchema {
            kind: "output",
            message: "root type must be declared".to_owned(),
        });
    }
    Ok(())
}

/// Bounds `$ref` resolution and nesting so a cyclic schema fails instead of
/// recursing until the stack gives out.
const MAX_SCHEMA_DEPTH: usize = 32;

/// Validates tool arguments with the semantics the Python reference gets from
/// Pydantic: `$ref` resolved against `$defs`, `anyOf` accepted when any variant
/// matches, `items` applied element by element, array-form `type` accepted, and
/// unknown properties tolerated unless the schema forbids them.
pub fn validate_arguments(arguments: &Value, schema: &Value) -> Result<(), ToolError> {
    validate_at(arguments, schema, schema, "$", 0)
}

/// Fills every absent property that declares a `default`, so a handler reads the
/// same value the reference model would have materialized.
pub fn apply_defaults(arguments: &mut Value, schema: &Value) {
    fill_defaults(arguments, schema, schema, 0);
}

/// Rewrites scalars the reference model would have accepted in a looser form,
/// then validates what the handler is actually going to read.
///
/// The reference builds its arguments through Pydantic in lax mode, which
/// coerces before it validates: `"yes"` reaches a `bool` field as `true` and
/// `"17"` reaches an `int` field as `17`. Validating the raw payload therefore
/// rejects calls the reference accepts, which is what made two of the 92
/// argument fixtures diverge. Coercing first closes that gap and, because the
/// rewrite is in place, the handler reads the coerced value rather than the
/// string the model sent.
///
/// This is the single entry point for both: `ToolRegistry::invoke_stream` calls
/// it before dispatch, and the fixture replay calls it to reproduce a verdict.
pub fn coerce_and_validate(arguments: &mut Value, schema: &Value) -> Result<(), ToolError> {
    coerce_at(arguments, schema, schema, 0);
    validate_arguments(arguments, schema)
}

/// Applies the reference scalar coercion in place, leaving anything it cannot
/// coerce untouched so validation reports it against the property that declared
/// the type.
fn coerce_at(value: &mut Value, schema: &Value, root: &Value, depth: usize) {
    if depth > MAX_SCHEMA_DEPTH {
        return;
    }
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        if let Some(target) = resolve_reference(reference, root) {
            coerce_at(value, target, root, depth + 1);
        }
        return;
    }
    if let Some(variants) = object.get("anyOf").and_then(Value::as_array) {
        coerce_union(value, variants, root, depth);
        return;
    }
    match object.get("type") {
        Some(Value::String(declared)) => coerce_declared(value, declared, object, root, depth),
        // The array form is a union spelled inline, so it follows the same
        // exact-match-first rule.
        Some(Value::Array(declared)) => {
            let variants = declared
                .iter()
                .filter_map(Value::as_str)
                .map(|name| serde_json::json!({ "type": name }))
                .collect::<Vec<_>>();
            coerce_union(value, &variants, root, depth);
        }
        _ => {}
    }
}

/// Picks the branch of a union the way Pydantic's smart mode does: a variant the
/// value already satisfies wins over any coercion, and only when none matches is
/// each variant tried in declaration order.
fn coerce_union(value: &mut Value, variants: &[Value], root: &Value, depth: usize) {
    if variants
        .iter()
        .any(|variant| validate_at(value, variant, root, "$", depth + 1).is_ok())
    {
        return;
    }
    for variant in variants {
        let mut candidate = value.clone();
        coerce_at(&mut candidate, variant, root, depth + 1);
        if validate_at(&candidate, variant, root, "$", depth + 1).is_ok() {
            *value = candidate;
            return;
        }
    }
}

/// Coerces `value` toward a single declared type, recursing into containers.
fn coerce_declared(
    value: &mut Value,
    declared: &str,
    schema: &Map<String, Value>,
    root: &Value,
    depth: usize,
) {
    match declared {
        "boolean" => {
            if let Some(coerced) = coerce_boolean(value) {
                *value = coerced;
            }
        }
        "integer" => {
            if let Some(coerced) = coerce_integer(value) {
                *value = coerced;
            }
        }
        "number" => {
            if let Some(coerced) = coerce_number(value) {
                *value = coerced;
            }
        }
        // A `string` field coerces nothing: the reference accepts only `str`
        // and `bytes`, so a number or a boolean stays as it is and validation
        // rejects it naming the property.
        "array" => {
            if let Some(items) = value.as_array_mut()
                && let Some(item_schema) = schema.get("items")
            {
                for item in items {
                    coerce_at(item, item_schema, root, depth + 1);
                }
            }
        }
        "object" => {
            if let Some(fields) = value.as_object_mut()
                && let Some(properties) = schema.get("properties").and_then(Value::as_object)
            {
                for (name, field_schema) in properties {
                    if let Some(field) = fields.get_mut(name) {
                        coerce_at(field, field_schema, root, depth + 1);
                    }
                }
            }
        }
        _ => {}
    }
}

/// The booleanish forms the reference accepts, or [`None`] when the value is
/// already a boolean or cannot become one.
///
/// The word set is case-insensitive and tolerates no surrounding whitespace,
/// both measured against the reference interpreter.
fn coerce_boolean(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => {
            let lowered = text.to_ascii_lowercase();
            match lowered.as_str() {
                "yes" | "on" | "true" | "t" | "y" | "1" => Some(Value::Bool(true)),
                "no" | "off" | "false" | "f" | "n" | "0" => Some(Value::Bool(false)),
                _ => None,
            }
        }
        // Only the two values that name a truth: `2` and `-1` are rejected
        // rather than treated as truthy.
        Value::Number(number) => {
            let flag = if let Some(integer) = number.as_i64() {
                match integer {
                    0 => false,
                    1 => true,
                    _ => return None,
                }
            } else {
                let float = number.as_f64()?;
                if float == 0.0 {
                    false
                } else if float == 1.0 {
                    true
                } else {
                    return None;
                }
            };
            Some(Value::Bool(flag))
        }
        _ => None,
    }
}

/// The integral forms the reference accepts: a boolean, a float whose fraction
/// is zero, and a string spelling either of those.
fn coerce_integer(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(flag) => Some(Value::from(i64::from(*flag))),
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                return None;
            }
            let float = number.as_f64()?;
            (float.fract() == 0.0).then(|| Value::from(float as i64))
        }
        Value::String(text) => parse_integer(text).map(Value::from),
        _ => None,
    }
}

/// The numeric forms the reference accepts for a float field. An integer is
/// already a valid JSON number, so it is left alone rather than rewritten into
/// a float that no acceptance decision depends on.
fn coerce_number(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(flag) => Some(Value::from(f64::from(u8::from(*flag)))),
        Value::String(text) => {
            let normalized = text.trim().replace('_', "");
            let parsed = normalized.parse::<f64>().ok()?;
            // JSON carries no infinity and no NaN, so a string spelling either
            // cannot be represented and is refused instead of silently clamped.
            parsed.is_finite().then(|| Value::from(parsed))
        }
        _ => None,
    }
}

/// Parses the integer spellings the reference accepts and no others.
///
/// Python's `int()` trims whitespace, honors a sign and ignores underscore
/// separators, and Pydantic additionally accepts a decimal spelling whose
/// fraction is zero. It refuses exponent notation, so `"1e2"` is not an integer
/// even though it is a whole number.
fn parse_integer(text: &str) -> Option<i64> {
    let normalized = text.trim().replace('_', "");
    if normalized.is_empty() {
        return None;
    }
    if let Ok(integer) = normalized.parse::<i64>() {
        return Some(integer);
    }
    let decimal = normalized
        .split_once('.')
        .filter(|_| !normalized.contains(['e', 'E']))?;
    let whole = format!("{}{}", decimal.0, decimal.1);
    if !whole
        .trim_start_matches(['+', '-'])
        .chars()
        .all(|character| character.is_ascii_digit())
    {
        return None;
    }
    let parsed = normalized.parse::<f64>().ok()?;
    (parsed.fract() == 0.0 && parsed.is_finite()).then_some(parsed as i64)
}

fn validate_at(
    value: &Value,
    schema: &Value,
    root: &Value,
    path: &str,
    depth: usize,
) -> Result<(), ToolError> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(ToolError::SchemaViolation {
            path: path.to_owned(),
            message: format!("schema nesting exceeds the {MAX_SCHEMA_DEPTH}-level bound"),
        });
    }
    let schema = schema
        .as_object()
        .ok_or_else(|| ToolError::SchemaViolation {
            path: path.to_owned(),
            message: "schema is not an object".to_owned(),
        })?;
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let target =
            resolve_reference(reference, root).ok_or_else(|| ToolError::SchemaViolation {
                path: path.to_owned(),
                message: format!("unresolved $ref `{reference}`"),
            })?;
        return validate_at(value, target, root, path, depth + 1);
    }
    if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
        let mut matched = false;
        let mut failures = Vec::new();
        for variant in variants {
            match validate_at(value, variant, root, path, depth + 1) {
                Ok(()) => {
                    matched = true;
                    break;
                }
                Err(error) => failures.push((variant, error)),
            }
        }
        if !matched {
            // A nullable property is one variant plus a null branch, so when
            // the value carries the declared type its own diagnosis is the
            // useful one; a value matching no variant gets the generic error.
            return Err(failures
                .into_iter()
                .find(|(variant, _)| declares_type_of(variant, value, root))
                .map(|(_, error)| error)
                .unwrap_or_else(|| ToolError::SchemaViolation {
                    path: path.to_owned(),
                    message: "value matches no declared variant".to_owned(),
                }));
        }
    }
    match schema.get("type") {
        Some(Value::String(expected)) => validate_type(value, expected, path)?,
        Some(Value::Array(expected)) => {
            let matched = expected
                .iter()
                .filter_map(Value::as_str)
                .any(|expected| validate_type(value, expected, path).is_ok());
            if !matched {
                return Err(ToolError::SchemaViolation {
                    path: path.to_owned(),
                    message: "value matches no declared type".to_owned(),
                });
            }
        }
        _ => {}
    }
    if let Some(variants) = schema.get("enum").and_then(Value::as_array)
        && !variants.contains(value)
    {
        return Err(ToolError::SchemaViolation {
            path: path.to_owned(),
            message: "value is not in enum".to_owned(),
        });
    }
    validate_bounds(value, schema, path)?;
    match value {
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    validate_at(
                        item,
                        item_schema,
                        root,
                        &format!("{path}[{index}]"),
                        depth + 1,
                    )?;
                }
            }
            Ok(())
        }
        Value::Object(object) => {
            let empty = Map::new();
            let properties = schema
                .get("properties")
                .and_then(Value::as_object)
                .unwrap_or(&empty);
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for field in required.iter().filter_map(Value::as_str) {
                    if !object.contains_key(field) {
                        return Err(ToolError::SchemaViolation {
                            path: format!("{path}.{field}"),
                            message: "required property is missing".to_owned(),
                        });
                    }
                }
            }
            if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false)
                && let Some(field) = object.keys().find(|field| !properties.contains_key(*field))
            {
                return Err(ToolError::SchemaViolation {
                    path: format!("{path}.{field}"),
                    message: "additional property is not allowed".to_owned(),
                });
            }
            for (field, field_schema) in properties {
                if let Some(field_value) = object.get(field) {
                    validate_at(
                        field_value,
                        field_schema,
                        root,
                        &format!("{path}.{field}"),
                        depth + 1,
                    )?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// True when `schema` declares the JSON type `value` carries, which is how an
/// `anyOf` branch is matched to the value it was meant to describe.
fn declares_type_of(schema: &Value, value: &Value, root: &Value) -> bool {
    let Some(schema) = schema.as_object() else {
        return false;
    };
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        return resolve_reference(reference, root)
            .is_some_and(|target| declares_type_of(target, value, root));
    }
    let declared = |name: &str| validate_type(value, name, "$").is_ok();
    match schema.get("type") {
        Some(Value::String(name)) => declared(name),
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).any(declared),
        _ => false,
    }
}

fn validate_bounds(
    value: &Value,
    schema: &Map<String, Value>,
    path: &str,
) -> Result<(), ToolError> {
    let violation = |message: String| ToolError::SchemaViolation {
        path: path.to_owned(),
        message,
    };
    if let Some(number) = value.as_f64() {
        for keyword in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
            let Some(bound) = schema.get(keyword).and_then(Value::as_f64) else {
                continue;
            };
            let satisfied = match keyword {
                "minimum" => number >= bound,
                "maximum" => number <= bound,
                "exclusiveMinimum" => number > bound,
                _ => number < bound,
            };
            if !satisfied {
                return Err(violation(format!("value violates {keyword} {bound}")));
            }
        }
    }
    if let Some(text) = value.as_str() {
        let length = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
        if let Some(bound) = schema.get("minLength").and_then(Value::as_u64)
            && length < bound
        {
            return Err(violation(format!(
                "value is shorter than minLength {bound}"
            )));
        }
        if let Some(bound) = schema.get("maxLength").and_then(Value::as_u64)
            && length > bound
        {
            return Err(violation(format!("value is longer than maxLength {bound}")));
        }
    }
    if let Some(items) = value.as_array() {
        let length = u64::try_from(items.len()).unwrap_or(u64::MAX);
        if let Some(bound) = schema.get("minItems").and_then(Value::as_u64)
            && length < bound
        {
            return Err(violation(format!(
                "array holds fewer than minItems {bound}"
            )));
        }
        if let Some(bound) = schema.get("maxItems").and_then(Value::as_u64)
            && length > bound
        {
            return Err(violation(format!("array holds more than maxItems {bound}")));
        }
    }
    Ok(())
}

fn fill_defaults(value: &mut Value, schema: &Value, root: &Value, depth: usize) {
    if depth > MAX_SCHEMA_DEPTH {
        return;
    }
    let Some(schema) = schema.as_object() else {
        return;
    };
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if let Some(target) = resolve_reference(reference, root) {
            fill_defaults(value, target, root, depth + 1);
        }
        return;
    }
    if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
        if let Some(variant) = variants
            .iter()
            .find(|variant| declares_type_of(variant, value, root))
        {
            fill_defaults(value, variant, root, depth + 1);
        }
        return;
    }
    match value {
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for item in items {
                    fill_defaults(item, item_schema, root, depth + 1);
                }
            }
        }
        Value::Object(object) => {
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return;
            };
            for (field, field_schema) in properties {
                match object.get_mut(field) {
                    Some(present) => fill_defaults(present, field_schema, root, depth + 1),
                    None => {
                        if let Some(default) = field_schema.get("default") {
                            object.insert(field.clone(), default.clone());
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn validate_type(value: &Value, expected: &str, path: &str) -> Result<(), ToolError> {
    let valid = match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ToolError::SchemaViolation {
            path: path.to_owned(),
            message: format!("expected {expected}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ObjectSchema, Property};
    use serde_json::json;

    fn spec(priority: i32) -> ToolSpec {
        ToolSpec {
            name: "read".to_owned(),
            description: "Read a file".to_owned(),
            input_schema: ObjectSchema::new()
                .required("path", Property::string())
                .build(),
            output_schema: Some(
                ObjectSchema::new()
                    .required("content", Property::string())
                    .build(),
            ),
            config: json!({"maxBytes": 1024}),
            state: json!({"calls": 0}),
            availability: ToolAvailability::Available,
            presentation: ToolPresentationKind::Read,
            source: ToolSource::BuiltIn,
            selection_priority: priority,
        }
    }

    fn handler(content: &'static str) -> Arc<dyn ToolHandler> {
        Arc::new(
            move |_invocation: &ToolInvocation,
                  _output: ToolOutputSink|
                  -> OwnedToolHandlerFuture {
                Box::pin(async move {
                    Ok(ToolExecutionOutput {
                        typed_result: json!({"content": content}),
                        model_text: content.to_owned(),
                        display: json!({"kind": "read"}),
                        chunks: vec![content.to_owned()],
                    })
                })
            },
        )
    }

    /// Publishes the argument payload the handler actually received, so a test
    /// can assert what defaults and pass-through keys reach a tool.
    fn echo_handler() -> Arc<dyn ToolHandler> {
        Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let arguments = invocation.arguments.clone();
                Box::pin(async move {
                    Ok(ToolExecutionOutput {
                        typed_result: arguments,
                        model_text: String::new(),
                        display: Value::Null,
                        chunks: Vec::new(),
                    })
                })
            },
        )
    }

    /// Remote providers publish `{alias}_{tool}`, a rule two providers can
    /// land on for the same string. The second claim is rejected and both
    /// sources are named, rather than one silently shadowing the other.
    #[test]
    fn a_second_source_claiming_a_published_name_is_rejected_naming_both() {
        let tools = ToolRegistry::default();
        tools
            .register_exclusive(spec(50), handler("first"), "MCP server `docs` tool `read`")
            .expect("the first claim wins the name");

        let conflict = tools
            .register_exclusive(
                spec(50),
                handler("second"),
                "MCP server `docs_read` tool ``",
            )
            .expect_err("the second claim cannot take the name");

        let message = conflict.to_string();
        assert!(
            message.contains("MCP server `docs` tool `read`"),
            "{message}"
        );
        assert!(
            message.contains("MCP server `docs_read` tool ``"),
            "{message}"
        );
        assert!(message.contains("read"), "{message}");

        // The same provider re-registering is a refresh, not a duplicate.
        tools
            .register_exclusive(
                spec(50),
                handler("refreshed"),
                "MCP server `docs` tool `read`",
            )
            .expect("a provider may republish its own tool");
    }

    fn reference_shaped_spec(input_schema: Value) -> ToolSpec {
        ToolSpec {
            name: "reference_shaped".to_owned(),
            description: "Reference-shaped arguments".to_owned(),
            input_schema,
            output_schema: None,
            config: Value::Null,
            state: Value::Null,
            availability: ToolAvailability::Available,
            presentation: ToolPresentationKind::Generic,
            source: ToolSource::BuiltIn,
            selection_priority: 0,
        }
    }

    async fn invoke_reference_shaped(
        input_schema: Value,
        arguments: Value,
    ) -> Result<ToolExecutionOutput, ToolError> {
        let registry = ToolRegistry::default();
        registry
            .register(reference_shaped_spec(input_schema), echo_handler())
            .expect("register");
        registry
            .invoke(
                "reference_shaped",
                ToolInvocation {
                    call_id: "call-1".to_owned(),
                    arguments,
                },
            )
            .await
    }

    fn todo_shaped_schema() -> Value {
        ObjectSchema::new()
            .define(
                "TodoStatus",
                Property::string().constrained("enum", json!(["pending", "completed"])),
            )
            .define(
                "TodoItem",
                ObjectSchema::new()
                    .required("id", Property::string())
                    .optional(
                        "status",
                        Property::reference("TodoStatus").with_default("pending"),
                    ),
            )
            .required("action", Property::string())
            .optional(
                "todos",
                Property::array(Property::reference("TodoItem"))
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build()
    }

    #[test]
    fn registration_accepts_reference_constructs_and_rejects_broken_pointers() {
        reference_shaped_spec(todo_shaped_schema())
            .validate()
            .expect("defs, refs and anyOf are accepted");

        let unresolved = reference_shaped_spec(json!({
            "type": "object",
            "properties": {"todos": {"items": {"$ref": "#/$defs/TodoItem"}, "type": "array"}},
        }))
        .validate()
        .expect_err("unresolved pointer");
        assert!(
            unresolved
                .to_string()
                .contains("unresolved $ref `#/$defs/TodoItem` at /properties/todos/items/$ref"),
            "{unresolved}"
        );

        let undeclared = reference_shaped_spec(json!({
            "type": "object",
            "properties": {"action": {"type": "string"}},
            "required": ["action", "todos"],
        }))
        .validate()
        .expect_err("required property absent from properties");
        assert!(
            undeclared
                .to_string()
                .contains("required property `todos` is absent from properties"),
            "{undeclared}"
        );
    }

    #[tokio::test]
    async fn arguments_validate_with_reference_semantics() {
        let schema = todo_shaped_schema();

        let resolved = invoke_reference_shaped(
            schema.clone(),
            json!({"action": "write", "todos": [{"id": "a", "status": "completed"}]}),
        )
        .await
        .expect("referenced subschema accepts a valid item");
        assert_eq!(resolved.typed_result["todos"][0]["status"], "completed");

        invoke_reference_shaped(schema.clone(), json!({"action": "read", "todos": null}))
            .await
            .expect("the null branch of anyOf is accepted");

        let wrong_variant =
            invoke_reference_shaped(schema.clone(), json!({"action": "read", "todos": 7}))
                .await
                .expect_err("a third type is rejected");
        assert!(
            matches!(&wrong_variant, ToolError::SchemaViolation { path, .. } if path == "$.todos"),
            "{wrong_variant}"
        );

        let bad_element = invoke_reference_shaped(
            schema.clone(),
            json!({"action": "write", "todos": [{"id": "a"}, {"id": 7}]}),
        )
        .await
        .expect_err("an element violating the item schema is rejected");
        assert!(
            matches!(&bad_element, ToolError::SchemaViolation { path, .. } if path == "$.todos[1].id"),
            "{bad_element}"
        );

        let enum_violation = invoke_reference_shaped(
            schema.clone(),
            json!({"action": "write", "todos": [{"id": "a", "status": "archived"}]}),
        )
        .await
        .expect_err("a value outside the referenced enum is rejected");
        assert!(
            matches!(&enum_violation, ToolError::SchemaViolation { path, .. } if path == "$.todos[0].status"),
            "{enum_violation}"
        );
    }

    #[tokio::test]
    async fn absent_defaults_reach_the_handler_and_unknown_keys_pass_through() {
        let filled = invoke_reference_shaped(
            todo_shaped_schema(),
            json!({"action": "write", "todos": [{"id": "a"}], "hallucinated": "kept"}),
        )
        .await
        .expect("permissive schemas ignore unknown keys, as Pydantic does");
        assert_eq!(filled.typed_result["todos"][0]["status"], "pending");
        assert_eq!(filled.typed_result["hallucinated"], "kept");

        let strict = ObjectSchema::new()
            .required("task", Property::string())
            .optional("agent", Property::string().with_default("explore"))
            .forbid_extra_properties()
            .build();
        let refused = invoke_reference_shaped(strict.clone(), json!({"task": "go", "extra": 1}))
            .await
            .expect_err("extra=forbid rejects unknown keys");
        assert!(
            matches!(&refused, ToolError::SchemaViolation { path, message }
                if path == "$.extra" && message.contains("additional property")),
            "{refused}"
        );
        let defaulted = invoke_reference_shaped(strict, json!({"task": "go"}))
            .await
            .expect("default applies");
        assert_eq!(defaulted.typed_result["agent"], "explore");
    }

    #[tokio::test]
    async fn a_reference_cycle_terminates_with_a_bounded_depth_error() {
        let schema = json!({
            "$defs": {"Loop": {"$ref": "#/$defs/Loop"}},
            "type": "object",
            "properties": {"node": {"$ref": "#/$defs/Loop"}},
        });
        reference_shaped_spec(schema.clone())
            .validate()
            .expect("the pointer resolves, the cycle is a runtime concern");
        let bounded = invoke_reference_shaped(schema, json!({"node": {}}))
            .await
            .expect_err("cycle is bounded");
        assert!(
            matches!(&bounded, ToolError::SchemaViolation { message, .. }
                if message.contains("exceeds the 32-level bound")),
            "{bounded}"
        );
    }

    #[tokio::test]
    async fn declared_bounds_reject_what_pydantic_rejects() {
        let schema = ObjectSchema::new()
            .required(
                "questions",
                Property::array(
                    ObjectSchema::new()
                        .required("question", Property::string())
                        .required(
                            "options",
                            Property::array(Property::string()).constrained("minItems", 2),
                        ),
                )
                .constrained("minItems", 1),
            )
            .optional(
                "offset",
                Property::integer()
                    .constrained("minimum", 1)
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build();

        let too_few = invoke_reference_shaped(
            schema.clone(),
            json!({"questions": [{"question": "why", "options": ["a"]}]}),
        )
        .await
        .expect_err("minItems is enforced");
        assert!(
            matches!(&too_few, ToolError::SchemaViolation { path, .. }
                if path == "$.questions[0].options"),
            "{too_few}"
        );

        let below_minimum = invoke_reference_shaped(
            schema.clone(),
            json!({"questions": [{"question": "why", "options": ["a", "b"]}], "offset": 0}),
        )
        .await
        .expect_err("minimum is enforced inside the non-null variant");
        assert!(
            matches!(&below_minimum, ToolError::SchemaViolation { path, message }
                if path == "$.offset" && message.contains("minimum")),
            "{below_minimum}"
        );

        let empty_list = invoke_reference_shaped(schema, json!({"questions": []}))
            .await
            .expect_err("minItems is enforced on the outer array");
        assert!(
            matches!(&empty_list, ToolError::SchemaViolation { path, .. } if path == "$.questions"),
            "{empty_list}"
        );
    }

    #[tokio::test]
    async fn registry_exposes_typed_contract_and_later_equal_priority_wins() {
        let registry = ToolRegistry::default();
        assert_eq!(
            registry
                .register(spec(10), handler("first"))
                .expect("register"),
            RegistrationOutcome::Inserted
        );
        assert_eq!(
            registry
                .register(spec(10), handler("second"))
                .expect("replace"),
            RegistrationOutcome::Replaced
        );
        let listed = registry.list().expect("list");
        assert_eq!(listed[0].config["maxBytes"], 1024);
        assert_eq!(listed[0].state["calls"], 0);
        let result = registry
            .invoke(
                "read",
                ToolInvocation {
                    call_id: "call-1".to_owned(),
                    arguments: json!({"path": "README.md"}),
                },
            )
            .await
            .expect("invoke");
        assert_eq!(result.typed_result["content"], "second");
    }

    /// Reference `_select_available_variant` ranks the variants published under
    /// one name by `(selection_priority, discovery_index)`, so the highest
    /// priority owns the name whichever order the variants register in.
    #[tokio::test]
    async fn the_highest_priority_variant_owns_the_published_name() {
        for order in [[10, 50], [50, 10]] {
            let registry = ToolRegistry::default();
            for (index, priority) in order.into_iter().enumerate() {
                let content = if priority == 50 { "managed" } else { "legacy" };
                let outcome = registry
                    .register(spec(priority), handler(content))
                    .expect("register");
                assert_eq!(
                    outcome,
                    if index == 0 {
                        RegistrationOutcome::Inserted
                    } else if priority == 50 {
                        RegistrationOutcome::Replaced
                    } else {
                        RegistrationOutcome::IgnoredLowerPriority
                    }
                );
            }
            let result = registry
                .invoke(
                    "read",
                    ToolInvocation {
                        call_id: "call-1".to_owned(),
                        arguments: json!({"path": "README.md"}),
                    },
                )
                .await
                .expect("invoke");
            assert_eq!(result.typed_result["content"], "managed", "{order:?}");
        }
    }

    /// Reference `is_available` is asked again on every `available_tools`
    /// access, so a prerequisite that comes and goes moves the tool in and out
    /// of the published surface without a re-registration.
    #[tokio::test]
    async fn a_conditional_tool_follows_its_prerequisite_at_publication_time() {
        let present = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = Arc::clone(&present);
        let registry = ToolRegistry::default();
        registry
            .register_conditional(
                spec(0),
                handler("conditional"),
                Arc::new(move || probe.load(Ordering::Acquire)),
            )
            .expect("register");
        let published = || {
            registry
                .available(&NameFilter::default(), &NameFilter::default())
                .expect("available")
                .into_iter()
                .map(|spec| spec.name)
                .collect::<Vec<_>>()
        };
        // Registered, so it is listed, and withheld, so nothing publishes it.
        assert_eq!(registry.list().expect("list").len(), 1);
        assert!(published().is_empty());
        assert_eq!(registry.withheld().expect("withheld"), ["read".to_owned()]);
        assert!(matches!(
            registry
                .invoke(
                    "read",
                    ToolInvocation {
                        call_id: "call-1".to_owned(),
                        arguments: json!({"path": "README.md"}),
                    },
                )
                .await,
            Err(ToolError::Unavailable(name)) if name == "read"
        ));

        present.store(true, Ordering::Release);
        assert_eq!(published(), ["read".to_owned()]);
        assert!(registry.withheld().expect("withheld").is_empty());
        assert!(
            registry
                .invoke(
                    "read",
                    ToolInvocation {
                        call_id: "call-2".to_owned(),
                        arguments: json!({"path": "README.md"}),
                    },
                )
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn schema_invalid_arguments_and_results_fail_closed() {
        let registry = ToolRegistry::default();
        registry.register(spec(0), handler("ok")).expect("register");
        let missing = registry
            .invoke(
                "read",
                ToolInvocation {
                    call_id: "call-1".to_owned(),
                    arguments: json!({}),
                },
            )
            .await;
        assert!(matches!(missing, Err(ToolError::SchemaViolation { .. })));

        let invalid_result: Arc<dyn ToolHandler> = Arc::new(
            |_invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                Box::pin(async {
                    Ok(ToolExecutionOutput {
                        typed_result: json!({"content": 7}),
                        model_text: "invalid".to_owned(),
                        display: Value::Null,
                        chunks: Vec::new(),
                    })
                })
            },
        );
        registry.register(spec(1), invalid_result).expect("replace");
        let result = registry
            .invoke(
                "read",
                ToolInvocation {
                    call_id: "call-2".to_owned(),
                    arguments: json!({"path": "README.md"}),
                },
            )
            .await;
        assert!(matches!(result, Err(ToolError::SchemaViolation { .. })));
    }

    #[tokio::test]
    async fn unavailable_and_oversized_outputs_are_bounded() {
        let registry = ToolRegistry::new(8);
        registry
            .register(spec(0), handler("larger than eight bytes"))
            .expect("register");
        let result = registry
            .invoke(
                "read",
                ToolInvocation {
                    call_id: "call-1".to_owned(),
                    arguments: json!({"path": "README.md"}),
                },
            )
            .await;
        assert!(matches!(result, Err(ToolError::OutputTooLarge { .. })));
        assert!(matches!(
            registry
                .invoke(
                    "missing",
                    ToolInvocation {
                        call_id: "call-2".to_owned(),
                        arguments: json!({}),
                    },
                )
                .await,
            Err(ToolError::Unavailable(_))
        ));
    }
}
