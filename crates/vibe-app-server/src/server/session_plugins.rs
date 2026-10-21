//! The plugins each unified session runs, keyed by the identifier it opened
//! under, and what the session derives from them: its skills and its plugin
//! MCP servers.

use super::*;

impl AppServer {
    /// The plugins a unified session runs, or `None` for a legacy session.
    ///
    /// The table is keyed by the identifier the session opened under, which
    /// stays its registry key across renames, so a later identifier resolves
    /// through the registry. A caller already holding the session lock reads
    /// [`Self::plugins_of`] instead.
    #[must_use]
    pub fn session_plugins(&self, session_id: &str) -> Option<Arc<crate::plugins::SessionPlugins>> {
        let table = self.session_plugins.lock().ok()?;
        if let Some(plugins) = table.get(session_id) {
            return Some(plugins.clone());
        }
        drop(table);
        let key = self.lock_sessions().ok()?.key(session_id)?.to_owned();
        self.session_plugins.lock().ok()?.get(&key).cloned()
    }

    /// The plugins `session` runs, found through any identifier it carried.
    pub(crate) fn plugins_of(
        &self,
        session: &SessionRuntime,
    ) -> Option<Arc<crate::plugins::SessionPlugins>> {
        let table = self.session_plugins.lock().ok()?;
        std::iter::once(&session.id)
            .chain(session.aliases.iter())
            .find_map(|id| table.get(id).cloned())
    }

    pub(crate) fn set_session_plugins(
        &self,
        session_id: &str,
        plugins: crate::plugins::SessionPlugins,
    ) {
        if let Ok(mut map) = self.session_plugins.lock() {
            map.insert(session_id.to_owned(), Arc::new(plugins));
        }
    }

    /// Resolves, pins and binds the plugins a unified session opens with. A
    /// legacy session resolves none, as the reference's legacy backend does.
    /// No discovery port is handed in: the tool catalog a plugin MCP server
    /// publishes is filled in once the deferred MCP connect answers.
    pub(crate) fn open_session_plugins(
        &self,
        session_id: &str,
        working_directory: &std::path::Path,
        project_trusted: bool,
        add_directories: &[String],
        mcp_configs: &[vibe_core::mcp::McpServerConfig],
    ) {
        if !self.harness_selection().use_unified
            || self
                .pluginless
                .lock()
                .is_ok_and(|pluginless| pluginless.contains(session_id))
        {
            return;
        }
        let sources = self.workspace.plugin_sources(
            working_directory,
            project_trusted,
            add_directories,
            mcp_configs
                .iter()
                .map(|config| config.alias.clone())
                .collect(),
        );
        if let Some(plugins) = crate::plugins::start_discovering(&sources, session_id) {
            self.set_session_plugins(session_id, plugins);
        }
    }

    /// Marks `session_id` as a session that opens with no plugins.
    pub(crate) fn bind_no_plugins(&self, session_id: &str) {
        if let Ok(mut pluginless) = self.pluginless.lock() {
            pluginless.insert(session_id.to_owned());
        }
    }

    /// Rescans and re-pins the plugins of `session_id`, files the new set in
    /// place of `previous`, and derives the session's skills and prompt again.
    pub(crate) fn reload_session_plugins(
        &self,
        session_id: &str,
        previous: &crate::plugins::SessionPlugins,
    ) -> Result<Arc<crate::plugins::SessionPlugins>, ServerError> {
        let (key, working_directory, trusted, add_directories) = {
            let sessions = self.lock_sessions()?;
            let key = sessions
                .key(session_id)
                .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?
                .to_owned();
            let session = sessions
                .get(&key)
                .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
            (
                key,
                PathBuf::from(&session.working_directory),
                session.intent.trusted,
                session.intent.add_directories.clone(),
            )
        };
        let configured = self
            .workspace
            .mcp_servers_for_session(&working_directory, trusted, &[])
            .unwrap_or_default();
        let sources = self.workspace.plugin_sources(
            &working_directory,
            trusted,
            &add_directories,
            configured
                .iter()
                .map(|config| config.alias.clone())
                .collect(),
        );
        let reloaded = crate::plugins::reload_discovering(&sources, &key, previous)
            .ok_or_else(|| ServerError::Resource("the plugin rescan could not run".to_owned()))?;
        let reloaded = Arc::new(reloaded);
        {
            let mut sessions = self.lock_sessions()?;
            if let Some(session) = sessions.get_mut(&key) {
                if let Ok(mut map) = self.session_plugins.lock() {
                    map.insert(key.clone(), reloaded.clone());
                    map.insert(session.id.clone(), reloaded.clone());
                }
                session.system_prompt = None;
            }
        }
        self.bind_plugin_mcp(&key);
        self.refresh_session_workspace_tools(session_id)?;
        Ok(reloaded)
    }

    /// Hands the resource backend the MCP servers `session_id`'s plugins
    /// declare, which `/mcp` lists after the configured ones, and the shell
    /// the library search paths its commands start from.
    ///
    /// `session_id` is the registry key, read off the table alone, so a caller
    /// holding the session lock can ask.
    pub(crate) fn bind_plugin_mcp(&self, session_id: &str) {
        let sources = self.session_plugins.lock().ok().and_then(|table| {
            table
                .get(session_id)
                .map(|plugins| plugins.mcp_sources.clone())
        });
        if let (Some(sources), Some(backend)) = (sources, &self.resource_backend) {
            backend.set_plugin_mcp(session_id, sources);
        }
        let environment = self.session_plugins.lock().ok().and_then(|table| {
            table
                .get(session_id)
                .map(|plugins| plugins.runtime_environment.clone())
        });
        if let Some(environment) = environment {
            self.shell_tools
                .set_session_environment(session_id, environment);
        }
    }

    /// Drops the plugins `session` ran under every identifier it carried.
    pub(crate) fn forget_session_plugins(&self, session: &SessionRuntime) {
        if let Ok(mut map) = self.session_plugins.lock() {
            map.remove(&session.id);
            for alias in &session.aliases {
                map.remove(alias);
            }
        }
    }

    /// Files the plugins `session` runs under its current identifier as well,
    /// for the lookups that key on it without the session lock.
    pub(crate) fn rekey_session_plugins(&self, session: &SessionRuntime) {
        let Some(plugins) = self.plugins_of(session) else {
            return;
        };
        if let Ok(mut map) = self.session_plugins.lock() {
            map.entry(session.id.clone()).or_insert(plugins);
        }
    }

    /// The skills a unified session's plugins publish, by alias, which seed its
    /// skill catalog in place of the legacy builtins: those reach a unified
    /// session as skills of the shipped `vibe` plugin.
    ///
    /// Reads the table alone, never the session lock, so a registration that
    /// holds it can ask.
    pub(crate) fn plugin_skill_seed(
        &self,
        session_id: &str,
    ) -> Option<Arc<BTreeMap<String, vibe_core::extensions::SkillDefinition>>> {
        let plugins = self.session_plugins.lock().ok()?.get(session_id).cloned()?;
        Some(skill_seed(&plugins))
    }

    /// [`Self::plugin_skill_seed`] for a caller holding the session lock.
    pub(crate) fn plugin_skill_seed_of(
        &self,
        session: &SessionRuntime,
    ) -> Option<Arc<BTreeMap<String, vibe_core::extensions::SkillDefinition>>> {
        self.plugins_of(session).map(|plugins| skill_seed(&plugins))
    }
}

fn skill_seed(
    plugins: &crate::plugins::SessionPlugins,
) -> Arc<BTreeMap<String, vibe_core::extensions::SkillDefinition>> {
    Arc::new(
        plugins
            .materialized
            .resolution
            .skills
            .iter()
            .map(|(alias, skill)| (alias.clone(), skill.clone()))
            .collect(),
    )
}
