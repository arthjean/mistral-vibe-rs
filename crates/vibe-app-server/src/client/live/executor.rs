//! The tool surface a session's turn publishes and runs: the registry seen
//! through the session's configured filters.

use super::*;

/// The session's own view of the registry: the two configured filters, plus the
/// exact names a subagent is confined to.
///
/// The filters are compiled once here rather than per call, because the
/// reference matching rules cover globs and regular expressions and a turn
/// consults them for every published name and again for every call.
#[derive(Clone)]
pub(in crate::client) struct SessionToolExecutor {
    tools: ToolRegistry,
    /// The session's two lists, shared by every clone so an agent switch in
    /// the middle of a turn reaches the executor the engine already holds.
    filters: Arc<std::sync::RwLock<SessionToolFilters>>,
    allowed: Option<BTreeSet<String>>,
    /// Tools this executor publishes but refuses to run, with the reason the
    /// model reads. A withheld tool and a refused one are different answers:
    /// the reference keeps `task` in a subagent's surface and refuses the call,
    /// so a refusal has to be expressible without dropping the definition.
    refused: BTreeMap<String, String>,
}

/// Which tools a session's configuration publishes.
#[derive(Clone)]
struct SessionToolFilters {
    /// Absent when the session wrote no `enabled_tools`, which is the only
    /// state that publishes everything. A list that carries entries narrows
    /// even when none of them compiles into a usable pattern, because the
    /// reference gates on the written list and matches with `name_matches`,
    /// which skips a blank entry and an uncompilable expression rather than
    /// widening the surface back out.
    enabled: Option<NameFilter>,
    disabled: NameFilter,
}

impl SessionToolFilters {
    fn of(intent: &SessionIntent) -> Self {
        Self {
            enabled: (!intent.enabled_tools.is_empty())
                .then(|| NameFilter::new(&intent.enabled_tools)),
            disabled: NameFilter::new(&intent.disabled_tools),
        }
    }
}

impl SessionToolExecutor {
    pub(in crate::client) fn new(tools: ToolRegistry, intent: &SessionIntent) -> Self {
        Self {
            tools,
            filters: Arc::new(std::sync::RwLock::new(SessionToolFilters::of(intent))),
            allowed: None,
            refused: BTreeMap::new(),
        }
    }

    /// Publishes what `intent` enables from now on, in this executor and in
    /// every clone of it.
    pub(super) fn retarget(&self, intent: &SessionIntent) {
        if let Ok(mut filters) = self.filters.write() {
            *filters = SessionToolFilters::of(intent);
        }
    }

    fn filters(&self) -> SessionToolFilters {
        self.filters.read().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |filters| filters.clone(),
        )
    }

    pub(super) fn with_allowed_tools(mut self, allowed: BTreeSet<String>) -> Self {
        self.allowed = Some(allowed);
        self
    }

    /// The same executor, publishing `name` and answering `reason` when it is
    /// called.
    pub(super) fn refusing(mut self, name: &str, reason: String) -> Self {
        self.refused.insert(name.to_owned(), reason);
        self
    }

    /// Why `name` is refused before it runs, or [`None`] when it may run.
    fn refusal(&self, name: &str) -> Option<String> {
        self.refused.get(name).cloned()
    }

    pub(super) fn permits(&self, name: &str) -> bool {
        let filters = self.filters();
        filters
            .enabled
            .as_ref()
            .is_none_or(|enabled| enabled.matches(name))
            && !filters.disabled.matches(name)
            && self
                .allowed
                .as_ref()
                .is_none_or(|allowed| allowed.contains(name))
    }

    pub(in crate::client) fn definitions(&self) -> Result<Vec<ToolDefinition>, DriverError> {
        let filters = self.filters();
        self.tools
            .available(filters.enabled.as_ref(), &filters.disabled)
            .map_err(|error| DriverError::Tool(error.to_string()))
            .map(|definitions| {
                definitions
                    .into_iter()
                    .filter(|definition| self.permits(&definition.name))
                    .map(|spec| ToolDefinition {
                        name: spec.name,
                        description: spec.description,
                        input_schema: spec.input_schema,
                    })
                    .collect()
            })
    }
}

impl ToolExecutor for SessionToolExecutor {
    fn remote_origin(&self, name: &str) -> Option<RemoteToolOrigin> {
        self.tools.remote_origin(name)
    }

    fn prepare_arguments(&self, name: &str, arguments: &str) -> Result<serde_json::Value, String> {
        ToolExecutor::prepare_arguments(&self.tools, name, arguments)
    }

    fn execute<'a>(&'a self, name: &'a str, arguments: &'a str) -> ToolFuture<'a> {
        if !self.permits(name) {
            return Box::pin(
                async move { Err(format!("tool `{name}` is disabled for this session")) },
            );
        }
        if let Some(reason) = self.refusal(name) {
            return Box::pin(async move { Err(reason) });
        }
        self.tools.execute(name, arguments)
    }

    fn execute_stream<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a str,
        output: ToolStreamSink,
    ) -> ToolFuture<'a> {
        self.execute_call("", name, arguments, output)
    }

    fn publishes(&self, name: &str) -> bool {
        self.permits(name) && ToolExecutor::publishes(&self.tools, name)
    }

    fn execute_call<'a>(
        &'a self,
        call_id: &'a str,
        name: &'a str,
        arguments: &'a str,
        output: ToolStreamSink,
    ) -> ToolFuture<'a> {
        if !self.permits(name) {
            return Box::pin(
                async move { Err(format!("tool `{name}` is disabled for this session")) },
            );
        }
        if let Some(reason) = self.refusal(name) {
            return Box::pin(async move { Err(reason) });
        }
        self.tools.execute_call(call_id, name, arguments, output)
    }
}
