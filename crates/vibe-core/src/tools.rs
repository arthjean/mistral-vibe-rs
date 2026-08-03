use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::io::{self, Write};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::engine::{ToolExecutor, ToolFuture, ToolStreamSink};
use crate::text::truncate_utf8;

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

#[derive(Clone)]
struct RegisteredTool {
    spec: ToolSpec,
    handler: Arc<dyn ToolHandler>,
    discovery_index: u64,
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Arc<RwLock<BTreeMap<String, RegisteredTool>>>,
    next_discovery_index: Arc<AtomicU64>,
    max_output_bytes: usize,
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
        }
    }

    pub fn register(
        &self,
        spec: ToolSpec,
        handler: Arc<dyn ToolHandler>,
    ) -> Result<RegistrationOutcome, ToolError> {
        spec.validate()?;
        let discovery_index = self.next_discovery_index.fetch_add(1, Ordering::Relaxed);
        let mut tools = self
            .tools
            .write()
            .map_err(|_| ToolError::RegistryPoisoned)?;
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

    pub fn available(
        &self,
        enabled: &BTreeSet<String>,
        disabled: &BTreeSet<String>,
    ) -> Result<Vec<ToolSpec>, ToolError> {
        let tools = self.list()?;
        Ok(tools
            .into_iter()
            .filter(|spec| spec.availability == ToolAvailability::Available)
            .filter(|spec| enabled.is_empty() || enabled.contains(&spec.name))
            .filter(|spec| !disabled.contains(&spec.name))
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
        invocation: ToolInvocation,
        stream: Option<ToolStreamSink>,
    ) -> Result<ToolExecutionOutput, ToolError> {
        let registered = {
            let tools = self.tools.read().map_err(|_| ToolError::RegistryPoisoned)?;
            tools
                .get(name)
                .cloned()
                .ok_or_else(|| ToolError::Unavailable(name.to_owned()))?
        };
        if registered.spec.availability != ToolAvailability::Available {
            return Err(ToolError::Unavailable(name.to_owned()));
        }
        validate_value(&invocation.arguments, &registered.spec.input_schema, "$")?;
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
            validate_value(&result.typed_result, schema, "$result")?;
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
    if let Some(properties) = object.get("properties")
        && !properties.is_object()
    {
        return Err(ToolError::InvalidSchema {
            kind,
            message: "properties must be an object".to_owned(),
        });
    }
    if let Some(required) = object.get("required")
        && required
            .as_array()
            .is_none_or(|items| items.iter().any(|item| !item.is_string()))
    {
        return Err(ToolError::InvalidSchema {
            kind,
            message: "required must be an array of strings".to_owned(),
        });
    }
    Ok(())
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

fn validate_value(value: &Value, schema: &Value, path: &str) -> Result<(), ToolError> {
    let schema = schema
        .as_object()
        .ok_or_else(|| ToolError::SchemaViolation {
            path: path.to_owned(),
            message: "schema is not an object".to_owned(),
        })?;
    if let Some(expected) = schema.get("type").and_then(Value::as_str) {
        validate_type(value, expected, path)?;
    }
    if let Some(variants) = schema.get("enum").and_then(Value::as_array)
        && !variants.contains(value)
    {
        return Err(ToolError::SchemaViolation {
            path: path.to_owned(),
            message: "value is not in enum".to_owned(),
        });
    }
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
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
    if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false) {
        let unknown = object
            .keys()
            .find(|field| !properties.contains_key(*field))
            .cloned();
        if let Some(field) = unknown {
            return Err(ToolError::SchemaViolation {
                path: format!("{path}.{field}"),
                message: "additional property is not allowed".to_owned(),
            });
        }
    }
    for (field, field_schema) in properties {
        if let Some(field_value) = object.get(&field) {
            validate_value(field_value, &field_schema, &format!("{path}.{field}"))?;
        }
    }
    Ok(())
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

#[must_use]
pub fn object_schema(
    properties: impl IntoIterator<Item = (impl Into<String>, Value)>,
    required: impl IntoIterator<Item = impl Into<String>>,
) -> Value {
    let properties = properties
        .into_iter()
        .map(|(name, schema)| (name.into(), schema))
        .collect::<Map<String, Value>>();
    let required = required
        .into_iter()
        .map(|name| Value::String(name.into()))
        .collect::<Vec<_>>();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(priority: i32) -> ToolSpec {
        ToolSpec {
            name: "read".to_owned(),
            description: "Read a file".to_owned(),
            input_schema: object_schema([("path", json!({"type": "string"}))], ["path"]),
            output_schema: Some(object_schema(
                [("content", json!({"type": "string"}))],
                ["content"],
            )),
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
