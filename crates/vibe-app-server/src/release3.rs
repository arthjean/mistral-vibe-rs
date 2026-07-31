use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::builtin_agents;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use thiserror::Error;
use toml::{Table, Value as TomlValue};
use vibe_core::config::{ConfigMutation, ConfigPaths, ConfigTarget, ConfigWrite, LayeredConfig};
use vibe_core::continuity::SessionContinuity;
use vibe_core::events::ModelMessage;
use vibe_core::extensions::{
    AgentKind, AgentProfile, AgentRegistry, DiscoveryRoots, ExtensionCatalog, ExtensionSource,
    SkillDefinition, discover_extensions,
};
use vibe_core::mcp::McpServerConfig;
use vibe_core::prompt::{
    InstructionLoader, PromptComposition, PromptResolver, SkillSummary, SubagentSummary,
    UserResource, prepare_user_resources,
};
use vibe_core::storage::{HydratedSession, SessionStore, StorageError};

pub const RELEASE3_METHODS: &[&str] = &[
    "agents/install",
    "agents/list",
    "agents/uninstall",
    "config/batchWrite",
    "config/proxy/read",
    "config/proxy/write",
    "config/read",
    "config/reload",
    "config/schema",
    "config/thinking/write",
    "history/list",
    "session/agent/update",
    "session/continue",
    "session/delete",
    "session/fork",
    "session/history/clear",
    "session/list",
    "session/log/read",
    "session/resume",
    "session/rewind",
    "session/rewind/read",
    "session/title/update",
    "skills/list",
    "workspace/prompt/prepare",
];

#[derive(Debug, Clone)]
pub struct Release3Paths {
    pub vibe_home: PathBuf,
    pub working_directory: PathBuf,
    pub session_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RuntimeAttachment {
    pub id: String,
    pub working_directory: String,
    pub parent_session_id: Option<String>,
    pub agent: Option<String>,
    pub agent_profile: Option<AgentProfile>,
    pub hydrated: HydratedSession,
}

#[derive(Debug, Clone)]
pub struct Release3Dispatch {
    pub result: BTreeMap<String, Value>,
    pub attachment: Option<RuntimeAttachment>,
}

impl Release3Dispatch {
    fn result(entries: impl IntoIterator<Item = (impl Into<String>, Value)>) -> Self {
        Self {
            result: entries
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
            attachment: None,
        }
    }
}

#[derive(Clone)]
pub struct Release3Service {
    paths: Release3Paths,
    defaults: Table,
    config: LayeredConfig,
    store: SessionStore,
    continuity: SessionContinuity,
    discovery_roots: DiscoveryRoots,
    project_trusted: bool,
    allowed_roots: Vec<PathBuf>,
    agents: Arc<Mutex<AgentRegistry>>,
    next_session: Arc<AtomicU64>,
    persist_runtime_sessions: bool,
}

impl Default for Release3Service {
    fn default() -> Self {
        let working_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let vibe_home = std::env::var_os("VIBE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".vibe"))
            })
            .unwrap_or_else(|| working_directory.join(".vibe"));
        Self::build(
            Release3Paths {
                session_root: vibe_home.join("sessions"),
                vibe_home,
                working_directory,
            },
            Table::new(),
            false,
        )
    }
}

impl Release3Service {
    pub fn new(
        paths: Release3Paths,
        defaults: Table,
        project_trusted: bool,
    ) -> Result<Self, Release3Error> {
        Ok(Self::build(paths, defaults, project_trusted))
    }

    fn build(paths: Release3Paths, defaults: Table, project_trusted: bool) -> Self {
        let config = LayeredConfig::new(
            ConfigPaths {
                vibe_home: paths.vibe_home.clone(),
                working_directory: paths.working_directory.clone(),
            },
            defaults.clone(),
        )
        .with_environment(
            std::env::vars().filter(|(key, _)| key != "VIBE_HOME" && key.starts_with("VIBE_")),
        )
        .with_project_trusted(project_trusted);
        let user_extensions = paths.vibe_home.join("extensions");
        let discovery_roots = DiscoveryRoots {
            configured: Vec::new(),
            project: vec![paths.working_directory.join(".vibe")],
            user: vec![user_extensions.clone()],
            project_trusted,
        };
        let mut registry = AgentRegistry::with_initial(
            builtin_agents::default_profile(),
            user_extensions.join("agents"),
        );
        for profile in builtin_agents::profiles(&paths.vibe_home) {
            registry.register_builtin(profile);
        }
        let store = SessionStore::new(&paths.session_root);
        Self {
            config,
            store: store.clone(),
            continuity: SessionContinuity::new(store),
            defaults,
            discovery_roots,
            project_trusted,
            allowed_roots: vec![paths.working_directory.clone()],
            paths,
            agents: Arc::new(Mutex::new(registry)),
            next_session: Arc::new(AtomicU64::new(1)),
            persist_runtime_sessions: false,
        }
    }

    #[must_use]
    pub fn with_runtime_session_persistence(mut self) -> Self {
        self.persist_runtime_sessions = true;
        self
    }

    #[must_use]
    pub fn for_runtime_session_root(
        session_root: impl Into<PathBuf>,
        working_directory: impl Into<PathBuf>,
    ) -> Self {
        let session_root = session_root.into();
        let vibe_home = session_root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| session_root.clone());
        Self::build(
            Release3Paths {
                vibe_home,
                working_directory: working_directory.into(),
                session_root,
            },
            Table::new(),
            false,
        )
        .with_runtime_session_persistence()
    }

    #[must_use]
    pub const fn persists_runtime_sessions(&self) -> bool {
        self.persist_runtime_sessions
    }

    pub fn update_runtime_settings(
        &self,
        session_id: &str,
        settings: &BTreeMap<String, Value>,
    ) -> Result<Option<HydratedSession>, Release3Error> {
        if !self.persist_runtime_sessions {
            return Ok(None);
        }
        let mut hydrated = self.store.load(session_id).map_err(storage_error)?;
        hydrated.metadata.config.extend(settings.clone());
        hydrated.metadata.updated_at_ms = now_millis();
        self.store
            .update_metadata(&hydrated.metadata)
            .map_err(storage_error)?;
        hydrated.current_config = hydrated.metadata.config.clone();
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(Some(hydrated))
    }

    pub fn create_runtime_session(
        &self,
        session_id: &str,
        working_directory: &str,
        now_ms: u64,
    ) -> Result<HydratedSession, Release3Error> {
        match self.store.load(session_id) {
            Ok(_) => {
                return Err(Release3Error::Storage(format!(
                    "session `{session_id}` already exists"
                )));
            }
            Err(StorageError::SessionNotFound(_)) => {}
            Err(error) => return Err(storage_error(error)),
        }
        self.store
            .create(session_id, working_directory, None, now_ms)
            .map_err(storage_error)?;
        let hydrated = self.store.load(session_id).map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated)
    }

    pub fn update_runtime_agent(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<Option<HydratedSession>, Release3Error> {
        if !self.persist_runtime_sessions {
            return Ok(None);
        }
        self.set_session_agent(session_id, name)
            .map(|(_, hydrated)| Some(hydrated))
    }

    pub(crate) fn agent_profile(&self, name: &str) -> Result<AgentProfile, Release3Error> {
        if name == "lean" && !self.installed_agent_names()?.contains(name) {
            return Err(Release3Error::InvalidParams(
                "agent `lean` must be installed with /leanstall first".to_owned(),
            ));
        }
        let profile = self.catalog().agents.remove(name).map_or_else(
            || {
                if self.persist_runtime_sessions {
                    Err(Release3Error::Extension(format!(
                        "agent `{name}` was not found"
                    )))
                } else {
                    Ok(AgentProfile {
                        name: name.to_owned(),
                        display_name: name.to_owned(),
                        description: "Externally supplied agent profile".to_owned(),
                        kind: AgentKind::Agent,
                        safety: "neutral".to_owned(),
                        overrides: Table::new(),
                        source: ExtensionSource::Configured,
                        path: None,
                    })
                }
            },
            Ok,
        )?;
        if profile.kind != AgentKind::Agent {
            return Err(Release3Error::Extension(format!(
                "subagent `{name}` cannot be selected as the primary agent"
            )));
        }
        if let Some(prompt_id) = profile.runtime_settings().system_prompt_id
            && builtin_agents::system_prompt(&prompt_id).is_none()
        {
            return Err(Release3Error::InvalidParams(format!(
                "agent `{name}` references unsupported system prompt `{prompt_id}`"
            )));
        }
        Ok(profile)
    }

    pub(crate) fn default_agent_name(&self) -> Result<String, Release3Error> {
        Ok(self
            .config
            .load()
            .map_err(config_error)?
            .effective
            .get("default_agent")
            .and_then(TomlValue::as_str)
            .unwrap_or("default")
            .to_owned())
    }

    #[must_use]
    pub fn with_allowed_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.allowed_roots.extend(roots);
        self
    }

    pub fn dispatch(
        &self,
        method: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        match method {
            "config/read" => self.config_snapshot(false),
            "config/reload" => self.config_snapshot(true),
            "config/schema" => Ok(Release3Dispatch::result([(
                "schema",
                LayeredConfig::schema(),
            )])),
            "config/batchWrite" => self.config_batch_write(params),
            "config/thinking/write" => self.single_config_write(params, "thinking", "value"),
            "config/proxy/write" => self.single_config_write(params, "proxy", "value"),
            "config/proxy/read" => {
                let snapshot = self.config.load().map_err(config_error)?;
                Ok(Release3Dispatch::result([(
                    "proxy",
                    snapshot.public_value("proxy"),
                )]))
            }
            "session/list" => self.session_list(params),
            "history/list" => self.history_list(params),
            "session/log/read" => self.session_log(params),
            "session/resume" => self.resume(params),
            "session/continue" => self.continue_session(params),
            "session/fork" => self.fork(params),
            "session/title/update" => self.title_update(params),
            "session/delete" => self.delete(params),
            "session/rewind" => self.rewind(params),
            "session/rewind/read" => self.rewind_read(params),
            "session/history/clear" => self.history_clear(params),
            "agents/list" => self.agents_list(),
            "agents/install" => self.agent_install(params),
            "agents/uninstall" => self.agent_uninstall(params),
            "session/agent/update" => self.agent_update(params),
            "skills/list" => self.skills_list(),
            "workspace/prompt/prepare" => self.prompt_prepare(params),
            _ => Err(Release3Error::MethodNotFound(method.to_owned())),
        }
    }

    pub fn close_saved_session(&self, session_id: &str, now_ms: u64) -> Result<(), Release3Error> {
        match self.store.close(session_id, now_ms) {
            Ok(_) | Err(StorageError::SessionNotFound(_)) => Ok(()),
            Err(error) => Err(storage_error(error)),
        }
    }

    pub fn mcp_servers_for_session(
        &self,
        working_directory: &Path,
        project_trusted: bool,
        runtime_servers: &[Value],
    ) -> Result<Vec<McpServerConfig>, Release3Error> {
        if !project_trusted && !runtime_servers.is_empty() {
            return Err(Release3Error::InvalidParams(
                "runtime MCP servers require a trusted workspace".to_owned(),
            ));
        }
        let mut runtime = Table::new();
        if !runtime_servers.is_empty() {
            runtime.insert(
                "mcp_servers".to_owned(),
                TomlValue::try_from(Value::Array(runtime_servers.to_vec()))
                    .map_err(|error| Release3Error::InvalidParams(error.to_string()))?,
            );
        }
        let snapshot = LayeredConfig::new(
            ConfigPaths {
                vibe_home: self.paths.vibe_home.clone(),
                working_directory: working_directory.to_path_buf(),
            },
            self.defaults.clone(),
        )
        .with_environment(
            std::env::vars().filter(|(key, _)| key != "VIBE_HOME" && key.starts_with("VIBE_")),
        )
        .with_runtime_overrides(runtime)
        .with_project_trusted(project_trusted)
        .load()
        .map_err(config_error)?;
        snapshot
            .mcp_servers(working_directory)
            .map_err(config_error)
    }

    fn config_snapshot(&self, reload: bool) -> Result<Release3Dispatch, Release3Error> {
        let snapshot = if reload {
            self.config.reload()
        } else {
            self.config.load()
        }
        .map_err(config_error)?;
        Ok(Release3Dispatch::result([(
            "snapshot",
            snapshot.public_view(),
        )]))
    }

    fn config_batch_write(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let raw = params
            .get("writes")
            .and_then(Value::as_array)
            .ok_or_else(|| Release3Error::InvalidParams("writes must be an array".to_owned()))?;
        let writes = raw
            .iter()
            .map(parse_config_write)
            .collect::<Result<Vec<_>, _>>()?;
        let snapshot = self.config.batch_write(&writes).map_err(config_error)?;
        Ok(Release3Dispatch::result([(
            "snapshot",
            snapshot.public_view(),
        )]))
    }

    fn single_config_write(
        &self,
        params: &BTreeMap<String, Value>,
        config_key: &str,
        value_key: &str,
    ) -> Result<Release3Dispatch, Release3Error> {
        let target = parse_target(
            params
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or("user"),
        )?;
        let snapshot = self.config.load().map_err(config_error)?;
        let expected_fingerprint = params
            .get("expectedFingerprint")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| snapshot.fingerprints.get(&target).cloned().flatten());
        let value = params
            .get(value_key)
            .cloned()
            .ok_or_else(|| Release3Error::InvalidParams(format!("{value_key} is required")))?;
        let mutation = ConfigMutation::set(
            [config_key],
            TomlValue::try_from(value)
                .map_err(|error| Release3Error::InvalidParams(error.to_string()))?,
        );
        let snapshot = self
            .config
            .batch_write(&[ConfigWrite {
                target,
                expected_fingerprint,
                mutations: vec![mutation],
            }])
            .map_err(config_error)?;
        Ok(Release3Dispatch::result([(
            "snapshot",
            snapshot.public_view(),
        )]))
    }

    fn session_list(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let offset = usize_param(params, "offset", 0)?;
        let limit = usize_param(params, "limit", 50)?;
        let cwd = params.get("cwd").and_then(Value::as_str);
        let migration = self.store.migrate_legacy().map_err(storage_error)?;
        let page = self.store.list(cwd, offset, limit).map_err(storage_error)?;
        Ok(Release3Dispatch::result([
            ("sessions", serde_json::to_value(page.sessions)?),
            ("nextOffset", json!(page.next_offset)),
            ("migration", serde_json::to_value(migration)?),
        ]))
    }

    fn history_list(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let session_id = required_string(params, "sessionId")?;
        let history = self
            .store
            .history(
                session_id,
                usize_param(params, "offset", 0)?,
                usize_param(params, "limit", 100)?,
            )
            .map_err(storage_error)?;
        Ok(Release3Dispatch::result([(
            "history",
            serde_json::to_value(history)?,
        )]))
    }

    fn session_log(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let hydrated = self
            .store
            .load(required_string(params, "sessionId")?)
            .map_err(storage_error)?;
        Ok(hydrated_result(&hydrated, None))
    }

    fn resume(&self, params: &BTreeMap<String, Value>) -> Result<Release3Dispatch, Release3Error> {
        let hydrated = self
            .store
            .resume(
                required_string(params, "sessionId")?,
                optional_string(params, "systemPrompt").unwrap_or_default(),
                config_map(params.get("config"))?,
            )
            .map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    fn continue_session(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let cwd = optional_string(params, "cwd")
            .unwrap_or_else(|| self.paths.working_directory.to_string_lossy().into_owned());
        let hydrated = self
            .store
            .continue_session(
                &cwd,
                optional_string(params, "systemPrompt").unwrap_or_default(),
                config_map(params.get("config"))?,
            )
            .map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    fn fork(&self, params: &BTreeMap<String, Value>) -> Result<Release3Dispatch, Release3Error> {
        let source = required_string(params, "sessionId")?;
        let keep_messages = fork_keep_messages(params)?;
        let new_id = optional_string(params, "newSessionId").unwrap_or_else(|| {
            format!(
                "session-{}-{}",
                now_millis(),
                self.next_session.fetch_add(1, Ordering::Relaxed)
            )
        });
        let mut hydrated = self
            .store
            .fork(
                source,
                &new_id,
                &optional_string(params, "systemPrompt").unwrap_or_default(),
                config_map(params.get("config"))?,
                now_millis(),
            )
            .map_err(storage_error)?;
        if let Some(keep_messages) = keep_messages {
            hydrated = self
                .store
                .rewind(
                    &hydrated.metadata.id,
                    keep_messages,
                    hydrated.metadata.statistics.clone(),
                    now_millis(),
                )
                .map_err(storage_error)?;
        }
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    fn title_update(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let metadata = self
            .store
            .update_title(
                required_string(params, "sessionId")?,
                required_string(params, "title")?,
                now_millis(),
            )
            .map_err(storage_error)?;
        Ok(Release3Dispatch::result([(
            "metadata",
            serde_json::to_value(metadata)?,
        )]))
    }

    fn delete(&self, params: &BTreeMap<String, Value>) -> Result<Release3Dispatch, Release3Error> {
        match self.store.delete(required_string(params, "sessionId")?) {
            Ok(()) | Err(StorageError::SessionNotFound(_)) => {}
            Err(error) => return Err(storage_error(error)),
        }
        Ok(Release3Dispatch::result([("deleted", json!(true))]))
    }

    fn rewind(&self, params: &BTreeMap<String, Value>) -> Result<Release3Dispatch, Release3Error> {
        let session_id = required_string(params, "sessionId")?;
        let message_index = params
            .get("messageIndex")
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| {
                        Release3Error::InvalidParams(
                            "messageIndex must be a non-negative integer".to_owned(),
                        )
                    })
            })
            .transpose()?;
        let restore_files = params
            .get("restoreFiles")
            .map(|value| {
                value.as_bool().ok_or_else(|| {
                    Release3Error::InvalidParams("restoreFiles must be a boolean".to_owned())
                })
            })
            .transpose()?
            .unwrap_or(false);
        if restore_files {
            return Err(Release3Error::InvalidParams(
                "this session has no restorable file checkpoint".to_owned(),
            ));
        }
        let inplace = params
            .get("inplace")
            .map(|value| {
                value.as_bool().ok_or_else(|| {
                    Release3Error::InvalidParams("inplace must be a boolean".to_owned())
                })
            })
            .transpose()?
            .unwrap_or(false);
        let source = self.store.load(session_id).map_err(storage_error)?;
        let message = message_index
            .map(|index| {
                source
                    .messages
                    .get(index)
                    .and_then(|message| match message {
                        ModelMessage::User { content } => Some(content.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        Release3Error::InvalidParams(
                            "messageIndex must identify a stored user message".to_owned(),
                        )
                    })
            })
            .transpose()?;
        let keep_messages = match message_index {
            Some(index) => index,
            None => usize_param(params, "keepMessages", 0)?,
        };
        let requested_statistics = statistics_map(params.get("statistics"))?;
        let rewind_statistics = if requested_statistics.is_empty() {
            source.metadata.statistics.clone()
        } else {
            requested_statistics
        };
        let timestamp = now_millis();
        let hydrated = if message_index.is_some() && !inplace {
            let new_id = format!(
                "session-{}-{}",
                timestamp,
                self.next_session.fetch_add(1, Ordering::Relaxed)
            );
            self.store
                .fork_rewound(
                    session_id,
                    &new_id,
                    keep_messages,
                    rewind_statistics,
                    timestamp,
                )
                .map_err(storage_error)?
        } else {
            self.store
                .rewind(session_id, keep_messages, rewind_statistics, timestamp)
                .map_err(storage_error)?
        };
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        let mut dispatch = hydrated_result(&hydrated, Some(runtime_attachment(&hydrated)));
        dispatch
            .result
            .insert("message".to_owned(), json!(message.unwrap_or_default()));
        dispatch
            .result
            .insert("restoreErrors".to_owned(), json!([]));
        dispatch
            .result
            .insert("restoredPaths".to_owned(), json!([]));
        Ok(dispatch)
    }

    fn rewind_read(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let hydrated = self
            .store
            .load(required_string(params, "sessionId")?)
            .map_err(storage_error)?;
        let messages = hydrated
            .messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| match message {
                ModelMessage::User { content } => Some(json!({
                    "messageIndex": index,
                    "message": content,
                })),
                _ => None,
            })
            .collect::<Vec<_>>();
        Ok(Release3Dispatch::result([
            ("messageCount", json!(hydrated.messages.len())),
            ("statistics", json!(hydrated.metadata.statistics)),
            ("messages", json!(messages)),
            ("restoreSupported", json!(false)),
        ]))
    }

    fn history_clear(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let hydrated = self
            .store
            .rewind(
                required_string(params, "sessionId")?,
                0,
                BTreeMap::new(),
                now_millis(),
            )
            .map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok(hydrated_result(
            &hydrated,
            Some(runtime_attachment(&hydrated)),
        ))
    }

    fn agents_list(&self) -> Result<Release3Dispatch, Release3Error> {
        let installed = self.installed_agent_names()?;
        let agents = self
            .catalog()
            .agents
            .into_values()
            .filter(|profile| profile.name != "lean" || installed.contains("lean"))
            .collect::<Vec<_>>();
        Ok(Release3Dispatch::result([(
            "agents",
            serde_json::to_value(agents)?,
        )]))
    }

    fn agent_install(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        if let Some(name) = params.get("agentName").and_then(Value::as_str) {
            self.set_builtin_agent_installed(name, true)?;
            return self.agents_list();
        }
        let source = self.authorized_existing_path(Path::new(required_string(params, "path")?))?;
        let profile = self
            .agents
            .lock()
            .map_err(|_| Release3Error::StatePoisoned)?
            .install(&source)
            .map_err(|error| Release3Error::Extension(error.to_string()))?;
        Ok(Release3Dispatch::result([(
            "agent",
            serde_json::to_value(profile)?,
        )]))
    }

    fn agent_uninstall(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        if let Some(name) = params.get("agentName").and_then(Value::as_str) {
            self.set_builtin_agent_installed(name, false)?;
            let mut dispatch = self.agents_list()?;
            if let Some(session_id) = params.get("sessionId").and_then(Value::as_str)
                && self
                    .store
                    .load(session_id)
                    .map_err(storage_error)?
                    .metadata
                    .agent_profile
                    .as_ref()
                    .and_then(|profile| profile.get("name"))
                    .and_then(Value::as_str)
                    == Some(name)
            {
                let (profile, hydrated) = self.set_session_agent(session_id, "default")?;
                dispatch
                    .result
                    .insert("agent".to_owned(), serde_json::to_value(profile)?);
                dispatch.attachment = Some(runtime_attachment(&hydrated));
            }
            return Ok(dispatch);
        }
        self.agents
            .lock()
            .map_err(|_| Release3Error::StatePoisoned)?
            .uninstall(required_string(params, "name")?)
            .map_err(|error| Release3Error::Extension(error.to_string()))?;
        Ok(Release3Dispatch::result([("removed", json!(true))]))
    }

    fn set_builtin_agent_installed(&self, name: &str, install: bool) -> Result<(), Release3Error> {
        if name != "lean" {
            return Err(Release3Error::InvalidParams(format!(
                "unknown installable built-in agent `{name}`"
            )));
        }
        let snapshot = self.config.load().map_err(config_error)?;
        let mut installed = snapshot
            .effective
            .get("installed_agents")
            .and_then(TomlValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(TomlValue::as_str)
            .map(ToOwned::to_owned)
            .collect::<BTreeSet<_>>();
        if install {
            installed.insert(name.to_owned());
        } else {
            installed.remove(name);
        }
        let expected_fingerprint = snapshot
            .fingerprints
            .get(&snapshot.selected_target)
            .cloned()
            .flatten();
        self.config
            .batch_write(&[ConfigWrite {
                target: snapshot.selected_target,
                expected_fingerprint,
                mutations: vec![ConfigMutation::set(
                    ["installed_agents"],
                    TomlValue::Array(installed.iter().cloned().map(TomlValue::String).collect()),
                )],
            }])
            .map_err(config_error)?;
        Ok(())
    }

    fn installed_agent_names(&self) -> Result<BTreeSet<String>, Release3Error> {
        Ok(self
            .config
            .load()
            .map_err(config_error)?
            .effective
            .get("installed_agents")
            .and_then(TomlValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(TomlValue::as_str)
            .map(ToOwned::to_owned)
            .collect())
    }

    fn set_session_agent(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<(AgentProfile, HydratedSession), Release3Error> {
        let profile = self.agent_profile(name)?;
        let mut metadata = self.store.load(session_id).map_err(storage_error)?.metadata;
        metadata.agent_profile = Some(serde_json::to_value(&profile)?);
        self.store
            .update_metadata(&metadata)
            .map_err(storage_error)?;
        let hydrated = self.store.load(session_id).map_err(storage_error)?;
        self.continuity
            .refresh(hydrated.clone())
            .map_err(|error| Release3Error::Storage(error.to_string()))?;
        Ok((profile, hydrated))
    }

    fn agent_update(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let session_id = required_string(params, "sessionId")?;
        let name = required_string(params, "name")?;
        let (profile, hydrated) = self.set_session_agent(session_id, name)?;
        Ok(Release3Dispatch {
            result: [("agent".to_owned(), serde_json::to_value(profile)?)]
                .into_iter()
                .collect(),
            attachment: Some(runtime_attachment(&hydrated)),
        })
    }

    fn skills_list(&self) -> Result<Release3Dispatch, Release3Error> {
        let catalog = self.catalog();
        Ok(Release3Dispatch::result([
            (
                "skills",
                serde_json::to_value(catalog.skills.into_values().collect::<Vec<_>>())?,
            ),
            ("issues", serde_json::to_value(catalog.issues)?),
        ]))
    }

    fn prompt_prepare(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let prompt: PromptParams = serde_json::from_value(Value::Object(
            params.clone().into_iter().collect::<Map<_, _>>(),
        ))
        .map_err(|error| Release3Error::InvalidParams(error.to_string()))?;
        let additional_directories = prompt
            .add_directories
            .iter()
            .map(|path| self.authorized_existing_path(path))
            .collect::<Result<Vec<_>, _>>()?;
        let catalog = self.catalog();
        let user_home = self
            .paths
            .vibe_home
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.paths.vibe_home.clone());
        let project_roots = if self.project_trusted {
            vec![(
                self.paths.working_directory.clone(),
                self.paths.working_directory.clone(),
            )]
        } else {
            Vec::new()
        };
        let loader = InstructionLoader::new(user_home, project_roots);
        let base = match prompt.prompt_id.as_deref() {
            Some(prompt_id) => {
                PromptResolver::new(
                    vec![self.paths.working_directory.join(".vibe/prompts")],
                    vec![self.paths.vibe_home.join("extensions/prompts")],
                    BTreeMap::new(),
                    self.project_trusted,
                )
                .resolve(prompt_id)
                .map_err(|error| Release3Error::Prompt(error.to_string()))?
                .content
            }
            None => prompt.base,
        };
        let composition = PromptComposition {
            base,
            headless: prompt.headless,
            commit_policy: prompt.commit_policy,
            model_info: prompt.model,
            os_tool_guidance: prompt.os_tool_guidance,
            skills: catalog
                .skills
                .values()
                .map(|skill| SkillSummary {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    path: Some(skill.path.clone()),
                })
                .collect(),
            subagents: catalog
                .agents
                .values()
                .filter(|agent| agent.kind == AgentKind::Subagent)
                .map(|agent| SubagentSummary {
                    name: agent.name.clone(),
                    description: agent.description.clone(),
                })
                .collect(),
            scratchpad: prompt.scratchpad,
            project_context: prompt.project_context,
            project_context_stale: prompt.project_context_stale,
            additional_directories: additional_directories.clone(),
            user_instructions: loader
                .user_document()
                .map_err(|error| Release3Error::Prompt(error.to_string()))?,
            project_instructions: loader
                .project_documents()
                .map_err(|error| Release3Error::Prompt(error.to_string()))?,
        }
        .compose();
        let mut roots = vec![self.paths.working_directory.clone()];
        roots.extend(additional_directories);
        let prepared = prepare_user_resources(&prompt.resources, &roots, prompt.supports_images)
            .map_err(|error| Release3Error::Prompt(error.to_string()))?;
        Ok(Release3Dispatch::result([
            ("prompt", serde_json::to_value(composition)?),
            ("user", serde_json::to_value(prepared)?),
            ("issues", serde_json::to_value(catalog.issues)?),
        ]))
    }

    fn authorized_existing_path(&self, path: &Path) -> Result<PathBuf, Release3Error> {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.paths.working_directory.join(path)
        };
        let canonical = fs::canonicalize(&candidate).map_err(|error| {
            Release3Error::InvalidParams(format!(
                "authorized path `{}` cannot be resolved: {error}",
                candidate.display()
            ))
        })?;
        let authorized = self
            .allowed_roots
            .iter()
            .any(|root| fs::canonicalize(root).is_ok_and(|allowed| canonical.starts_with(allowed)));
        if !authorized {
            return Err(Release3Error::InvalidParams(format!(
                "path `{}` is outside server-authorized workspace roots",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    fn catalog(&self) -> ExtensionCatalog {
        let builtin_agents = self
            .agents
            .lock()
            .ok()
            .map(|agents| {
                agents
                    .list()
                    .into_iter()
                    .filter(|agent| agent.source == ExtensionSource::Builtin)
                    .map(|agent| (agent.name.clone(), agent.clone()))
                    .collect()
            })
            .unwrap_or_default();
        discover_extensions(
            &self.discovery_roots,
            builtin_agents,
            BTreeMap::<String, SkillDefinition>::new(),
            BTreeMap::new(),
        )
    }
}

fn fork_keep_messages(params: &BTreeMap<String, Value>) -> Result<Option<usize>, Release3Error> {
    let explicit = params
        .get("keepMessages")
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| {
                    Release3Error::InvalidParams(
                        "keepMessages must be a non-negative integer".to_owned(),
                    )
                })
        })
        .transpose()?;
    let anchored = params
        .get("messageId")
        .map(|value| {
            let message_id = value.as_str().ok_or_else(|| {
                Release3Error::InvalidParams("messageId must be a string".to_owned())
            })?;
            let index = message_id
                .strip_prefix("history-")
                .and_then(|value| value.parse::<usize>().ok())
                .ok_or_else(|| {
                    Release3Error::InvalidParams(
                        "messageId must use the stable `history-N` form".to_owned(),
                    )
                })?;
            index.checked_add(1).ok_or_else(|| {
                Release3Error::InvalidParams("messageId index is too large".to_owned())
            })
        })
        .transpose()?;
    match (explicit, anchored) {
        (Some(explicit), Some(anchored)) if explicit != anchored => {
            Err(Release3Error::InvalidParams(
                "keepMessages and messageId identify different fork anchors".to_owned(),
            ))
        }
        (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptParams {
    #[serde(default)]
    base: String,
    #[serde(default)]
    prompt_id: Option<String>,
    #[serde(default)]
    headless: bool,
    #[serde(default)]
    commit_policy: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    os_tool_guidance: Option<String>,
    #[serde(default)]
    scratchpad: Option<PathBuf>,
    #[serde(default)]
    project_context: Option<String>,
    #[serde(default)]
    project_context_stale: bool,
    #[serde(default)]
    add_directories: Vec<PathBuf>,
    #[serde(default)]
    resources: Vec<UserResource>,
    #[serde(default)]
    supports_images: bool,
}

fn parse_config_write(value: &Value) -> Result<ConfigWrite, Release3Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Release3Error::InvalidParams("write must be an object".to_owned()))?;
    let target = parse_target(
        object
            .get("target")
            .and_then(Value::as_str)
            .ok_or_else(|| Release3Error::InvalidParams("write.target is required".to_owned()))?,
    )?;
    let expected_fingerprint = object
        .get("expectedFingerprint")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let mutations = object
        .get("mutations")
        .and_then(Value::as_array)
        .ok_or_else(|| Release3Error::InvalidParams("write.mutations must be an array".to_owned()))?
        .iter()
        .map(|mutation| {
            let mutation = mutation.as_object().ok_or_else(|| {
                Release3Error::InvalidParams("mutation must be an object".to_owned())
            })?;
            let path = mutation
                .get("path")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    Release3Error::InvalidParams("mutation.path must be an array".to_owned())
                })?
                .iter()
                .map(|part| {
                    part.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                        Release3Error::InvalidParams(
                            "mutation path parts must be strings".to_owned(),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            if mutation
                .get("remove")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                Ok::<ConfigMutation, Release3Error>(ConfigMutation::remove(path))
            } else {
                let raw = mutation.get("value").cloned().ok_or_else(|| {
                    Release3Error::InvalidParams("mutation.value is required".to_owned())
                })?;
                let value = TomlValue::try_from(raw)
                    .map_err(|error| Release3Error::InvalidParams(error.to_string()))?;
                Ok::<ConfigMutation, Release3Error>(ConfigMutation::set(path, value))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ConfigWrite {
        target,
        expected_fingerprint,
        mutations,
    })
}

fn parse_target(value: &str) -> Result<ConfigTarget, Release3Error> {
    match value {
        "user" => Ok(ConfigTarget::User),
        "project" => Ok(ConfigTarget::Project),
        _ => Err(Release3Error::InvalidParams(
            "target must be user or project".to_owned(),
        )),
    }
}

fn hydrated_result(
    hydrated: &HydratedSession,
    attachment: Option<RuntimeAttachment>,
) -> Release3Dispatch {
    Release3Dispatch {
        result: [
            ("metadata".to_owned(), json!(hydrated.metadata)),
            ("messages".to_owned(), json!(hydrated.messages)),
            ("currentConfig".to_owned(), json!(hydrated.current_config)),
        ]
        .into_iter()
        .collect(),
        attachment,
    }
}

fn runtime_attachment(hydrated: &HydratedSession) -> RuntimeAttachment {
    let agent_profile: Option<AgentProfile> = hydrated
        .metadata
        .agent_profile
        .as_ref()
        .and_then(|profile| serde_json::from_value(profile.clone()).ok());
    RuntimeAttachment {
        id: hydrated.metadata.id.clone(),
        working_directory: hydrated.metadata.working_directory.clone(),
        parent_session_id: hydrated.metadata.parent_session_id.clone(),
        agent: agent_profile
            .as_ref()
            .map(|profile| profile.name.clone())
            .or_else(|| {
                hydrated
                    .metadata
                    .agent_profile
                    .as_ref()
                    .and_then(|profile| profile.get("name"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            }),
        agent_profile,
        hydrated: hydrated.clone(),
    }
}

fn config_map(value: Option<&Value>) -> Result<BTreeMap<String, Value>, Release3Error> {
    value
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(Release3Error::Json)
        .map(Option::unwrap_or_default)
}

fn statistics_map(value: Option<&Value>) -> Result<BTreeMap<String, Value>, Release3Error> {
    config_map(value)
}

fn required_string<'a>(
    params: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a str, Release3Error> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Release3Error::InvalidParams(format!("{key} must be a non-empty string")))
}

fn optional_string(params: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn usize_param(
    params: &BTreeMap<String, Value>,
    key: &str,
    default: usize,
) -> Result<usize, Release3Error> {
    params
        .get(key)
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| Release3Error::InvalidParams(format!("{key} must be an integer")))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn config_error(error: vibe_core::config::ConfigError) -> Release3Error {
    Release3Error::Config(error.to_string())
}

fn storage_error(error: vibe_core::storage::StorageError) -> Release3Error {
    let message = error.to_string();
    match error {
        vibe_core::storage::StorageError::NoSessions
        | vibe_core::storage::StorageError::SessionNotFound(_)
        | vibe_core::storage::StorageError::AmbiguousSession(_) => Release3Error::NotFound(message),
        _ => Release3Error::Storage(message),
    }
}

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

#[derive(Debug, Error)]
pub enum Release3Error {
    #[error("unknown release-3 method `{0}`")]
    MethodNotFound(String),
    #[error("invalid parameters: {0}")]
    InvalidParams(String),
    #[error("{0}")]
    NotFound(String),
    #[error("configuration failed: {0}")]
    Config(String),
    #[error("session storage failed: {0}")]
    Storage(String),
    #[error("extension failed: {0}")]
    Extension(String),
    #[error("prompt preparation failed: {0}")]
    Prompt(String),
    #[error("release-3 state lock is poisoned")]
    StatePoisoned,
    #[error("JSON conversion failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use vibe_core::events::ModelMessage;
    use vibe_protocol::{Envelope, TransportKind, decode_frame};

    use super::*;
    use crate::server::AppServer;

    fn service() -> (tempfile::TempDir, Release3Service) {
        let temporary = tempdir().expect("tempdir");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let service = Release3Service::new(
            Release3Paths {
                vibe_home: temporary.path().join("home"),
                working_directory: workspace,
                session_root: temporary.path().join("sessions"),
            },
            Table::new(),
            true,
        )
        .expect("service");
        (temporary, service)
    }

    #[test]
    fn public_config_methods_preserve_unknown_values_and_redact_proxy_secrets() {
        let (_temporary, service) = service();
        let result = service
            .dispatch(
                "config/batchWrite",
                &BTreeMap::from([(
                    "writes".to_owned(),
                    json!([{
                        "target": "user",
                        "expectedFingerprint": null,
                        "mutations": [
                            {"path": ["future", "flag"], "value": true},
                            {"path": ["proxy"], "value": "https://proxy.example"}
                        ]
                    }]),
                )]),
            )
            .expect("write");
        assert_eq!(
            result.result["snapshot"]["config"]["future"]["flag"],
            json!(true)
        );
        assert_eq!(
            result.result["snapshot"]["config"]["proxy"],
            json!("[redacted]")
        );
        assert_eq!(
            service
                .dispatch("config/proxy/read", &BTreeMap::new())
                .expect("proxy read")
                .result["proxy"],
            json!("[redacted]")
        );
    }

    #[test]
    fn configured_default_agent_is_resolved_from_the_live_config_snapshot() {
        let (_temporary, service) = service();
        service
            .dispatch(
                "config/batchWrite",
                &BTreeMap::from([(
                    "writes".to_owned(),
                    json!([{
                        "target": "user",
                        "expectedFingerprint": null,
                        "mutations": [{"path": ["default_agent"], "value": "plan"}]
                    }]),
                )]),
            )
            .expect("default agent config");

        assert_eq!(
            service.default_agent_name().expect("configured agent"),
            "plan"
        );
    }

    #[test]
    fn discovered_user_agent_remains_selectable_after_service_restart() {
        let temporary = tempdir().expect("tempdir");
        let workspace = temporary.path().join("workspace");
        let vibe_home = temporary.path().join("home");
        let agents = vibe_home.join("extensions/agents");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&agents).expect("agent directory");
        fs::write(
            agents.join("reviewer.toml"),
            "display_name = \"Reviewer\"\ndescription = \"Custom reviewer\"\nagent_type = \"agent\"\n",
        )
        .expect("custom agent");

        let service = Release3Service::new(
            Release3Paths {
                session_root: temporary.path().join("sessions"),
                vibe_home,
                working_directory: workspace,
            },
            Table::new(),
            true,
        )
        .expect("restarted service");

        assert_eq!(
            service
                .agent_profile("reviewer")
                .expect("discovered profile")
                .display_name,
            "Reviewer"
        );
        fs::write(
            agents.join("reviewer.toml"),
            "display_name = \"Reloaded Reviewer\"\ndescription = \"Custom reviewer\"\nagent_type = \"agent\"\n",
        )
        .expect("updated custom agent");
        assert_eq!(
            service
                .agent_profile("reviewer")
                .expect("reloaded profile")
                .display_name,
            "Reloaded Reviewer"
        );
        assert!(
            service
                .dispatch("agents/list", &BTreeMap::new())
                .expect("agents list")
                .result["agents"]
                .as_array()
                .is_some_and(|agents| agents.iter().any(|agent| agent["name"] == "reviewer"))
        );
    }

    #[test]
    fn builtin_lean_agent_installation_is_persisted_and_reflected_in_agent_listing() {
        let (_temporary, service) = service();
        let before = service
            .dispatch("agents/list", &BTreeMap::new())
            .expect("agents list");
        assert!(
            before.result["agents"]
                .as_array()
                .is_some_and(|agents| agents.iter().all(|agent| agent["name"] != "lean"))
        );

        service
            .dispatch(
                "agents/install",
                &BTreeMap::from([("agentName".to_owned(), json!("lean"))]),
            )
            .expect("lean install");
        let installed = service
            .dispatch("agents/list", &BTreeMap::new())
            .expect("installed agents list");
        assert!(
            installed.result["agents"]
                .as_array()
                .is_some_and(|agents| agents.iter().any(|agent| agent["name"] == "lean"))
        );
        let config = service
            .dispatch("config/read", &BTreeMap::new())
            .expect("config after install");
        assert_eq!(
            config.result["snapshot"]["config"]["installed_agents"],
            json!(["lean"])
        );

        service
            .dispatch(
                "agents/uninstall",
                &BTreeMap::from([("agentName".to_owned(), json!("lean"))]),
            )
            .expect("lean uninstall");
        let after = service
            .dispatch("agents/list", &BTreeMap::new())
            .expect("agents after uninstall");
        assert!(
            after.result["agents"]
                .as_array()
                .is_some_and(|agents| agents.iter().all(|agent| agent["name"] != "lean"))
        );
    }

    #[test]
    fn switching_away_from_auto_approve_removes_its_permission_override() {
        let (_temporary, service) = service();
        let session_id = "agent-switch";
        let working_directory = service
            .paths
            .working_directory
            .to_string_lossy()
            .into_owned();
        service
            .create_runtime_session(session_id, &working_directory, 1)
            .expect("session");
        let mut metadata = service.store.load(session_id).expect("metadata").metadata;
        metadata
            .config
            .insert("active_model".to_owned(), json!("base-model"));
        service
            .store
            .update_metadata(&metadata)
            .expect("base session config");

        for name in ["auto-approve", "default"] {
            let update = service
                .dispatch(
                    "session/agent/update",
                    &BTreeMap::from([
                        ("sessionId".to_owned(), json!(session_id)),
                        ("name".to_owned(), json!(name)),
                    ]),
                )
                .expect("agent update");
            assert_eq!(
                update
                    .attachment
                    .as_ref()
                    .and_then(|attachment| attachment.agent.as_deref()),
                Some(name)
            );
        }

        let metadata = service.store.load(session_id).expect("metadata").metadata;
        assert_eq!(
            metadata
                .agent_profile
                .as_ref()
                .and_then(|profile| profile.get("name")),
            Some(&json!("default"))
        );
        assert!(
            !metadata.config.contains_key("bypass_tool_permissions"),
            "the prior auto-approve override must not survive"
        );
        assert_eq!(
            metadata.config.get("active_model"),
            Some(&json!("base-model")),
            "agent overlays must not destroy the underlying session config"
        );
    }

    #[test]
    fn rewind_lists_user_messages_and_forks_before_the_selected_message() {
        let (_temporary, service) = service();
        let session_id = "rewind-source";
        let working_directory = service
            .paths
            .working_directory
            .to_string_lossy()
            .into_owned();
        let mut hydrated = service
            .create_runtime_session(session_id, &working_directory, 1)
            .expect("session");
        for (offset, message) in [
            ModelMessage::User {
                content: "first question".to_owned(),
            },
            ModelMessage::Assistant {
                content: "first answer".to_owned(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: Vec::new(),
            },
            ModelMessage::User {
                content: "edit this question".to_owned(),
            },
            ModelMessage::Assistant {
                content: "second answer".to_owned(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: Vec::new(),
            },
        ]
        .iter()
        .enumerate()
        {
            service
                .store
                .append_message(
                    &mut hydrated.metadata,
                    message,
                    u64::try_from(offset).unwrap_or_default().saturating_add(2),
                )
                .expect("append message");
        }

        let preview = service
            .dispatch(
                "session/rewind/read",
                &BTreeMap::from([("sessionId".to_owned(), json!(session_id))]),
            )
            .expect("rewind preview");
        assert_eq!(
            preview.result["messages"],
            json!([
                {"messageIndex": 0, "message": "first question"},
                {"messageIndex": 2, "message": "edit this question"},
            ])
        );

        let rewind = service
            .dispatch(
                "session/rewind",
                &BTreeMap::from([
                    ("sessionId".to_owned(), json!(session_id)),
                    ("messageIndex".to_owned(), json!(2)),
                    ("restoreFiles".to_owned(), json!(false)),
                    ("statistics".to_owned(), json!({"tokens": 17})),
                ]),
            )
            .expect("rewind");
        let child = rewind.attachment.expect("child attachment");
        assert_eq!(child.parent_session_id.as_deref(), Some(session_id));
        assert_eq!(rewind.result["message"], json!("edit this question"));
        assert_eq!(child.hydrated.messages.len(), 2);
        assert_eq!(child.hydrated.metadata.statistics["tokens"], 17);
        assert_eq!(
            service
                .store
                .load(session_id)
                .expect("source remains")
                .messages
                .len(),
            4
        );
    }

    #[test]
    fn caller_paths_cannot_expand_server_authorized_roots() {
        let (temporary, service) = service();
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&outside).expect("outside directory");
        std::fs::write(
            outside.join("agent.toml"),
            "display_name = \"Outside\"\nagent_type = \"agent\"\n",
        )
        .expect("outside agent");
        let install = service.dispatch(
            "agents/install",
            &BTreeMap::from([("path".to_owned(), json!(outside.join("agent.toml")))]),
        );
        assert!(matches!(install, Err(Release3Error::InvalidParams(_))));
        let prompt = service.dispatch(
            "workspace/prompt/prepare",
            &BTreeMap::from([
                ("base".to_owned(), json!("base")),
                ("addDirectories".to_owned(), json!([outside])),
            ]),
        );
        assert!(matches!(prompt, Err(Release3Error::InvalidParams(_))));
    }

    #[test]
    fn project_mcp_stdio_config_activates_only_after_workspace_trust() {
        let (temporary, service) = service();
        fs::create_dir_all(temporary.path().join("workspace/.vibe")).expect("project config root");
        fs::write(
            temporary.path().join("workspace/.vibe/config.toml"),
            r#"
[[mcp_servers]]
name = "fixture"
transport = "stdio"
command = "/usr/bin/fixture"
args = ["--stdio"]
env = { MODE = "test" }
cwd = "."
startup_timeout_sec = 1
tool_timeout_sec = 2
"#,
        )
        .expect("project MCP config");

        assert!(
            service
                .mcp_servers_for_session(&temporary.path().join("workspace"), false, &[])
                .expect("untrusted user fallback")
                .is_empty()
        );
        let trusted = service
            .mcp_servers_for_session(&temporary.path().join("workspace"), true, &[])
            .expect("trusted project MCP config");
        assert_eq!(trusted.len(), 1);
        assert_eq!(trusted[0].alias, "fixture");
        assert_eq!(trusted[0].startup_timeout_ms, 1_000);
        assert_eq!(trusted[0].tool_timeout_ms, 2_000);

        let runtime = json!({
            "name": "runtime",
            "transport": "stdio",
            "command": "/must-not-run"
        });
        assert!(matches!(
            service.mcp_servers_for_session(
                &temporary.path().join("workspace"),
                false,
                &[runtime]
            ),
            Err(Release3Error::InvalidParams(message))
                if message.contains("trusted workspace")
        ));
    }

    #[test]
    fn saved_session_methods_create_independent_runtime_attachments() {
        let (_temporary, service) = service();
        let mut metadata = service
            .store
            .create("parent", "/workspace", None, 1)
            .expect("create");
        service
            .store
            .append_message(
                &mut metadata,
                &ModelMessage::User {
                    content: "hello".to_owned(),
                },
                2,
            )
            .expect("append");
        let fork = service
            .dispatch(
                "session/fork",
                &BTreeMap::from([
                    ("sessionId".to_owned(), json!("parent")),
                    ("newSessionId".to_owned(), json!("child")),
                    ("systemPrompt".to_owned(), json!("fresh")),
                    ("config".to_owned(), json!({"model": "child"})),
                ]),
            )
            .expect("fork");
        assert_eq!(
            fork.attachment.expect("attachment").parent_session_id,
            Some("parent".to_owned())
        );
        assert_eq!(fork.result["currentConfig"]["model"], "child");
        assert_eq!(
            service
                .dispatch(
                    "session/list",
                    &BTreeMap::from([("limit".to_owned(), json!(50))]),
                )
                .expect("list")
                .result["sessions"]
                .as_array()
                .expect("array")
                .len(),
            2
        );
    }

    #[test]
    fn app_server_advertises_and_dispatches_release3_resources() {
        let (_temporary, service) = service();
        service
            .store
            .create("saved", "/workspace", None, 1)
            .expect("saved session");
        let server = AppServer::with_release3_service(service);
        let mut connection = server.connect(TransportKind::InProcess);
        let initialized = connection.dispatch(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"test","version":"1"},"capabilities":{"callbackKinds":[]}}}"#,
        );
        let response = decode_frame(&initialized.outbound[0]).expect("initialize response");
        assert!(matches!(response, Envelope::Success(_)));
        let Envelope::Success(response) = response else {
            return;
        };
        assert!(
            response.result["capabilities"]["methods"]
                .as_array()
                .expect("methods")
                .contains(&json!("config/schema"))
        );
        connection.dispatch(br#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);
        let resume = connection.dispatch(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"saved","systemPrompt":"fresh","config":{}}}"#,
        );
        assert!(matches!(
            decode_frame(&resume.outbound[0]).expect("resume response"),
            Envelope::Success(_)
        ));
        assert_eq!(
            server.session("saved").expect("attached runtime").id,
            "saved"
        );
    }
}
