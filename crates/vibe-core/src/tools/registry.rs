//! The tools one session publishes, and the path one call takes through
//! them: argument validation, the handler, and the output budget.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use futures_util::FutureExt;
use serde_json::Value;

use super::validate::{self, validate_at};
use super::{
    DEFAULT_MAX_TOOL_OUTPUT_BYTES, MAX_TOOL_ERROR_BYTES, ToolAvailability, ToolError,
    ToolExecutionOutput, ToolHandler, ToolInvocation, ToolOutputSink, ToolSource, ToolSpec,
    apply_defaults, coerce_and_validate, descriptions,
};
use crate::engine::{ToolExecutor, ToolFuture, ToolStreamSink};
use crate::events::RemoteToolOrigin;
use crate::matching::NameFilter;
use crate::sync;
use crate::text::truncate_utf8;

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
    /// Where this name sits in the published order, which is the discovery
    /// index of its *first* registration rather than of the variant that owns
    /// it now.
    ///
    /// Reference `_all_tools` is a dict keyed by name: a second variant
    /// reassigns the key and a dict keeps a reassigned key at its original
    /// position, so the winning variant is published where the name was first
    /// seen. Carrying the position separately from `discovery_index` is what
    /// keeps the tie-break of [`Self::available`] and the published order
    /// answering two different questions.
    position: u64,
    /// Who published this name, so a second claim on it can name the first.
    origin: String,
    /// The prerequisite this tool is published under, when it has one.
    condition: Option<ToolCondition>,
    /// The remote this tool proxies, when it is not implemented here. The
    /// published name does not carry it back, so the registration that knew it
    /// is where it is kept.
    remote: Option<RemoteToolOrigin>,
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
    /// The composition every tool family publishes behind, installed when the
    /// session's builtin surface registers. A family that registers later than
    /// that surface, as `task` does once its turn resolves a subagent runner,
    /// reads the session's own store, approval agent and configuration from
    /// here rather than being handed a second composition.
    guard: Arc<RwLock<Option<crate::policy::ToolGuard>>>,
    /// Where the descriptions an operator wrote are read from, installed when
    /// the session's workspace tools register. It is a source rather than a
    /// resolved map because reference `ToolManager` recomputes its descriptions
    /// per construction, so a file written mid-session reaches the next
    /// publication.
    descriptions: Arc<RwLock<Option<Arc<dyn descriptions::DescriptionSource>>>>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolRegistry")
            .field("tool_count", &sync::read(&self.tools).len())
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
            guard: Arc::new(RwLock::new(None)),
            descriptions: Arc::new(RwLock::new(None)),
        }
    }

    /// Installs where this session reads description overrides from, replacing
    /// any earlier source so a re-registration after a working-directory change
    /// reads the directories that change resolved.
    pub fn set_descriptions(&self, source: Arc<dyn descriptions::DescriptionSource>) {
        *sync::write(&self.descriptions) = Some(source);
    }

    /// The session's description source, absent until the workspace tools
    /// register.
    #[must_use]
    pub fn descriptions(&self) -> Option<Arc<dyn descriptions::DescriptionSource>> {
        sync::read(&self.descriptions).clone()
    }

    /// Installs the guard the session's tools are published behind, replacing
    /// any earlier one so a re-registration after an agent switch carries the
    /// approval agent that registration resolved.
    pub fn set_guard(&self, guard: crate::policy::ToolGuard) {
        *sync::write(&self.guard) = Some(guard);
    }

    /// The session's guard, absent until the builtin surface registers.
    #[must_use]
    pub fn guard(&self) -> Option<crate::policy::ToolGuard> {
        sync::read(&self.guard).clone()
    }

    /// Installs the session's invoked-skill resolver, replacing any earlier
    /// one: a re-registration after an agent switch re-reads the discovery it
    /// was handed then.
    pub fn set_invoked_skills(&self, resolver: Arc<dyn crate::skills::InvokedSkillResolver>) {
        *sync::write(&self.invoked_skills) = Some(resolver);
    }

    /// The invoked-skill resolver, absent until the `skill` tool registers.
    #[must_use]
    pub fn invoked_skills(&self) -> Option<Arc<dyn crate::skills::InvokedSkillResolver>> {
        sync::read(&self.invoked_skills).clone()
    }

    pub fn register(
        &self,
        spec: ToolSpec,
        handler: Arc<dyn ToolHandler>,
    ) -> Result<RegistrationOutcome, ToolError> {
        let origin = format!("{:?} tool `{}`", spec.source, spec.name);
        self.insert(spec, handler, origin, false, None, None)
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
        self.insert(spec, handler, origin, false, Some(condition), None)
    }

    /// Registers a tool that must own its published name outright.
    ///
    /// Remote providers publish `{alias}_{tool}`, a rule two different
    /// providers can land on for the same string. Priority-based replacement
    /// would let the later claim silently shadow the earlier one, so an
    /// exclusive registration that finds the name taken is rejected and both
    /// sources are named.
    ///
    /// `remote` names the server-side tool the registration proxies, which the
    /// projection needs to present the call and which `{alias}_{tool}` cannot
    /// be split back into.
    pub fn register_exclusive(
        &self,
        spec: ToolSpec,
        handler: Arc<dyn ToolHandler>,
        origin: impl Into<String>,
        remote: Option<RemoteToolOrigin>,
    ) -> Result<RegistrationOutcome, ToolError> {
        self.insert(spec, handler, origin.into(), true, None, remote)
    }

    /// The remote a published tool proxies, or nothing when this session
    /// implements it.
    #[must_use]
    pub fn remote_origin(&self, name: &str) -> Option<RemoteToolOrigin> {
        sync::read(&self.tools).get(name)?.remote.clone()
    }

    fn insert(
        &self,
        spec: ToolSpec,
        handler: Arc<dyn ToolHandler>,
        origin: String,
        exclusive: bool,
        condition: Option<ToolCondition>,
        remote: Option<RemoteToolOrigin>,
    ) -> Result<RegistrationOutcome, ToolError> {
        spec.validate()?;
        let discovery_index = self.next_discovery_index.fetch_add(1, Ordering::Relaxed);
        let mut tools = sync::write(&self.tools);
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
            // A replacement inherits the position of the name it takes over, so
            // a late high-priority variant is published where the name first
            // appeared rather than at the end of the surface.
            let position = tools
                .get(&spec.name)
                .map_or(discovery_index, |existing| existing.position);
            tools.insert(
                spec.name.clone(),
                RegisteredTool {
                    spec,
                    handler,
                    discovery_index,
                    position,
                    origin,
                    condition,
                    remote,
                },
            );
            Ok(outcome)
        } else {
            Ok(RegistrationOutcome::IgnoredLowerPriority)
        }
    }

    /// Every registration in the order this session discovered it.
    ///
    /// The registry is keyed by name so that a lookup stays a lookup, but
    /// reference `available_tool_specs` and `registered_tools` both walk an
    /// insertion-ordered dict. Every view that leaves this module is therefore
    /// resequenced here rather than published in the map's own alphabetical
    /// order, which no reader upstream would ever see.
    fn in_discovery_order(tools: &BTreeMap<String, RegisteredTool>) -> Vec<&RegisteredTool> {
        let mut ordered = tools.values().collect::<Vec<_>>();
        ordered.sort_by_key(|tool| tool.position);
        ordered
    }

    pub fn list(&self) -> Vec<ToolSpec> {
        let tools = sync::read(&self.tools);
        Self::in_discovery_order(&tools)
            .into_iter()
            .map(|tool| tool.spec.clone())
            .collect()
    }

    /// The surface a session publishes: every available tool the filters keep.
    ///
    /// Reference `available_tools` narrows with `enabled_tools` only when that
    /// list carries an entry, and applies `disabled_tools` last, so a name both
    /// lists match is withheld. The narrowing is decided by the written list
    /// rather than by what it compiled to, which is why `enabled` arrives as an
    /// [`Option`]: upstream tests the list itself and then matches with
    /// `name_matches`, where a blank entry and an uncompilable expression are
    /// both skipped. A list of nothing but those therefore narrows to nothing,
    /// and reading the compiled filter instead would publish everything.
    ///
    /// Reference `available_tool_specs` then prefers the description an
    /// operator wrote over the tool's own, which is why the override is applied
    /// here rather than at registration: it reaches a tool registered later
    /// than the source, which is every MCP and connector tool, and it is re-read
    /// per publication rather than cached. The directories are read before the
    /// registry lock is taken, so a slow filesystem never holds it.
    ///
    /// The order is the discovery order both reference filters preserve, not
    /// the name order this registry stores under: see
    /// [`Self::in_discovery_order`].
    pub fn available(&self, enabled: Option<&NameFilter>, disabled: &NameFilter) -> Vec<ToolSpec> {
        let overrides = self
            .descriptions()
            .map(|source| source.overrides())
            .unwrap_or_default();
        let tools = sync::read(&self.tools);
        Self::in_discovery_order(&tools)
            .into_iter()
            .filter(|tool| tool.available())
            .map(|tool| tool.spec.clone())
            .filter(|spec| enabled.is_none_or(|enabled| enabled.matches(&spec.name)))
            .filter(|spec| !disabled.matches(&spec.name))
            .map(|mut spec| {
                if let Some(text) = overrides.get(&spec.name) {
                    spec.description.clone_from(text);
                }
                spec
            })
            .collect()
    }

    /// The registered names no publication can carry, because the prerequisite
    /// they were registered under does not hold.
    pub fn withheld(&self) -> Vec<String> {
        let tools = sync::read(&self.tools);
        tools
            .values()
            .filter(|tool| {
                tool.condition
                    .as_ref()
                    .is_some_and(|condition| !condition())
            })
            .map(|tool| tool.spec.name.clone())
            .collect()
    }

    /// Removes every tool `source` published, answering their names.
    ///
    /// A source that republishes its whole surface retracts it first, so the
    /// tools it registers again take the end of the discovery order: reference
    /// `integrate_connectors_async` drops the connector tools from the manager
    /// and appends the new ones.
    pub fn retract_source(&self, source: ToolSource) -> Vec<String> {
        let mut tools = sync::write(&self.tools);
        let retracted = tools
            .values()
            .filter(|tool| tool.spec.source == source)
            .map(|tool| tool.spec.name.clone())
            .collect::<Vec<_>>();
        for name in &retracted {
            tools.remove(name);
        }
        retracted
    }

    pub fn set_availability(
        &self,
        name: &str,
        source: ToolSource,
        availability: ToolAvailability,
    ) -> bool {
        let mut tools = sync::write(&self.tools);
        let Some(registered) = tools.get_mut(name) else {
            return false;
        };
        if registered.spec.source != source {
            return false;
        }
        registered.spec.availability = availability;
        true
    }

    pub fn set_availabilities(
        &self,
        source: ToolSource,
        updates: &[(String, ToolAvailability)],
    ) -> bool {
        let mut tools = sync::write(&self.tools);
        if updates.iter().any(|(name, _)| {
            tools
                .get(name)
                .is_none_or(|registered| registered.spec.source != source)
        }) {
            return false;
        }
        for (name, availability) in updates {
            if let Some(registered) = tools.get_mut(name) {
                registered.spec.availability = *availability;
            }
        }
        true
    }

    /// What the policy in front of `name` makes of a call, the way invoking
    /// it would read it, without asking anyone or running it: `None` when the
    /// tool is not published, its arguments do not validate, or nothing
    /// guards it.
    pub async fn resolve_permission(
        &self,
        name: &str,
        mut invocation: ToolInvocation,
    ) -> Option<Result<crate::policy::PermissionOutcome, crate::policy::PolicyError>> {
        let registered = { sync::read(&self.tools).get(name).cloned()? };
        if !registered.available() {
            return None;
        }
        coerce_and_validate(
            name,
            &mut invocation.arguments,
            &registered.spec.input_schema,
        )
        .ok()?;
        apply_defaults(&mut invocation.arguments, &registered.spec.input_schema);
        let decided = registered.handler.resolve_permission(&invocation)?;
        Some(decided.await)
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
            let tools = sync::read(&self.tools);
            tools
                .get(name)
                .cloned()
                .ok_or_else(|| ToolError::Unavailable(name.to_owned()))?
        };
        if !registered.available() {
            return Err(ToolError::Unavailable(name.to_owned()));
        }
        coerce_and_validate(
            name,
            &mut invocation.arguments,
            &registered.spec.input_schema,
        )?;
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
            // A typed result is the tool's own output rather than the model's
            // input, so it keeps the single-violation diagnostic: what a handler
            // needs is the first place its result went wrong.
            validate_at(&result.typed_result, schema, schema, "$result", 0).map_err(
                |violations| match violations.into_iter().next() {
                    Some(first) => ToolError::SchemaViolation {
                        path: first.path,
                        message: first.message,
                    },
                    None => ToolError::InvalidResult("unreported schema violation".to_owned()),
                },
            )?;
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
    fn prepare_arguments(&self, name: &str, arguments: &str) -> Result<Value, String> {
        let mut arguments: Value = serde_json::from_str(arguments)
            .map_err(|error| format!("invalid tool arguments: {error}"))?;
        let schema = {
            sync::read(&self.tools)
                .get(name)
                .map(|registered| registered.spec.input_schema.clone())
                .ok_or_else(|| ToolError::Unavailable(name.to_owned()).to_string())?
        };
        coerce_and_validate(name, &mut arguments, &schema).map_err(|error| error.to_string())?;
        // A remote tool's arguments model keeps what it does not declare
        // (reference `_OpenArgs`); a built-in one ignores it.
        if Self::remote_origin(self, name).is_some() {
            apply_defaults(&mut arguments, &schema);
        } else {
            validate::dump_validated(&mut arguments, &schema);
        }
        Ok(arguments)
    }

    fn remote_origin(&self, name: &str) -> Option<RemoteToolOrigin> {
        Self::remote_origin(self, name)
    }

    fn publishes(&self, name: &str) -> bool {
        sync::read(&self.tools).contains_key(name)
    }

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
        self.execute_call("", name, arguments, output)
    }

    fn execute_call<'a>(
        &'a self,
        call_id: &'a str,
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
                    call_id: call_id.to_owned(),
                    arguments,
                },
                Some(output),
            )
            .await
            .or_else(|error| match error {
                ToolError::TurnFailed(message) => Ok(ToolExecutionOutput {
                    turn_failure: Some(message.clone()),
                    approval: None,
                    ..ToolExecutionOutput::text(message)
                }),
                ToolError::Approved { approval, source } => match *source {
                    ToolError::TurnFailed(message) => Ok(ToolExecutionOutput {
                        turn_failure: Some(message.clone()),
                        approval: Some(approval),
                        ..ToolExecutionOutput::text(message)
                    }),
                    source => Ok(ToolExecutionOutput {
                        approval: Some(approval),
                        failure: Some(source.to_string()),
                        ..ToolExecutionOutput::text(String::new())
                    }),
                },
                error => Err(error.to_string()),
            })
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationOutcome {
    Inserted,
    Replaced,
    IgnoredLowerPriority,
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
