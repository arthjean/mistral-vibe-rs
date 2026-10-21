//! What a session opened in a directory reads under its own scope: the
//! configuration its directory, trust and roots select, and what it loads
//! through it (hooks, plugin roots, MCP servers, tool filters and the census
//! its start reports).
//!
//! Reference `HarnessFilesManager.for_session`: a session's roots are the ones
//! the process opened followed by the ones the session lists.

use super::*;

impl WorkspaceService {
    /// The configuration a session opened in `working_directory` reads: its
    /// trust, the project-file verdict it pinned, and as additional roots the
    /// ones this service opened beside its own directory, then
    /// `add_directories`.
    pub(super) fn session_config(
        &self,
        working_directory: PathBuf,
        trusted: bool,
        project_file_trust: Option<bool>,
        add_directories: &[String],
    ) -> LayeredConfig {
        // The directory the server runs in is an authorized root, not an
        // opened project: only its trust makes it one.
        let roots = self
            .allowed_roots
            .iter()
            .filter(|root| **root != self.paths.working_directory)
            .cloned()
            .chain(add_directories.iter().map(PathBuf::from))
            .collect();
        self.config
            .scoped_to_working_directory(working_directory, trusted)
            .with_project_file_trust(project_file_trust)
            .with_additional_roots(roots)
    }

    /// The hooks a session in `working_directory` loads: every open project's
    /// `.vibe/hooks.toml`, then the user's. Reference `load_hooks_from_fs`
    /// over the session's `HarnessFilesManager`.
    #[must_use]
    pub fn session_hooks(
        &self,
        working_directory: &Path,
        project_trusted: bool,
        add_directories: &[String],
    ) -> crate::session_hooks::SessionHooks {
        let files = self
            .session_config(
                working_directory.to_path_buf(),
                project_trusted,
                None,
                add_directories,
            )
            .harness_files()
            .hook_files();
        crate::session_hooks::SessionHooks::load(&files, working_directory)
    }

    /// Where a unified session opened in `working_directory` looks for
    /// plugins, with the MCP server names its configuration already holds.
    /// Reference `PluginResolver.from_harness_files` over the session's
    /// harness files, rooted like [`Self::session_hooks`] so an untrusted
    /// project contributes no plugin directory.
    #[must_use]
    pub fn plugin_sources(
        &self,
        working_directory: &Path,
        project_trusted: bool,
        add_directories: &[String],
        configured_mcp_names: BTreeSet<String>,
    ) -> crate::plugins::PluginSources {
        let files = self
            .session_config(
                working_directory.to_path_buf(),
                project_trusted,
                None,
                add_directories,
            )
            .harness_files();
        crate::plugins::PluginSources {
            project_roots: files.project_plugins_dirs(),
            user_roots: files.user_plugins_dirs(),
            vibe_home: self.paths.vibe_home.clone(),
            storage_root: self.session_logging.save_dir.clone(),
            workdir: vibe_core::platform::resolve_lenient(working_directory),
            configured_mcp_names,
        }
    }

    /// Reference `emit_new_session_telemetry`'s four counts, read off the
    /// same services a session is built from: the workspace instructions
    /// file, every discovered skill, the MCP servers this session would
    /// connect and the models the merged configuration declares.
    #[must_use]
    pub fn session_census(
        &self,
        working_directory: &Path,
        trust: bool,
    ) -> vibe_core::telemetry::records::NewSession {
        let nb_skills = self
            .dispatch("skills/list", &BTreeMap::new())
            .ok()
            .and_then(|dispatch| dispatch.result.get("skills").cloned())
            .as_ref()
            .and_then(Value::as_array)
            .map_or(0, Vec::len) as u64;
        let nb_mcp_servers = self
            .mcp_servers_for_session(working_directory, trust, &[])
            .map_or(0, |servers| servers.len()) as u64;
        let nb_models = self.read_config(0, |snapshot| match snapshot.effective.get("models") {
            Some(TomlValue::Array(entries)) => entries.len(),
            Some(TomlValue::Table(entries)) => entries.len(),
            _ => 0,
        }) as u64;
        vibe_core::telemetry::records::NewSession {
            has_agents_md: has_agents_md(working_directory),
            nb_skills,
            nb_mcp_servers,
            nb_models,
        }
    }

    pub fn mcp_servers_for_session(
        &self,
        working_directory: &Path,
        project_trusted: bool,
        runtime_servers: &[Value],
    ) -> Result<Vec<McpServerConfig>, WorkspaceServiceError> {
        let mut runtime = Table::new();
        if !runtime_servers.is_empty() {
            runtime.insert(
                "mcp_servers".to_owned(),
                TomlValue::try_from(Value::Array(runtime_servers.to_vec()))
                    .map_err(|error| WorkspaceServiceError::InvalidParams(error.to_string()))?,
            );
        }
        let snapshot = LayeredConfig::new(
            ConfigPaths {
                vibe_home: self.paths.vibe_home.clone(),
                working_directory: working_directory.to_path_buf(),
            },
            self.defaults.clone(),
        )
        .with_environment(vibe_environment(&self.paths.vibe_home))
        .with_runtime_overrides(runtime)
        .with_project_trusted(project_trusted)
        .load()
        .map_err(config_error)?;
        snapshot
            .mcp_servers(working_directory)
            .map_err(config_error)
    }

    /// The `enabled_tools` and `disabled_tools` lists the configuration carries
    /// for a session opened in `working_directory`, in that order.
    ///
    /// The reference reads them from the same layered configuration the session
    /// runs under, so the project file of the directory being opened counts,
    /// not the one the service was constructed with.
    pub fn tool_filters_for_session(
        &self,
        working_directory: &Path,
        project_trusted: bool,
    ) -> Result<(Vec<String>, Vec<String>), WorkspaceServiceError> {
        let snapshot = self
            .config
            .clone()
            .scoped_to_working_directory(working_directory.to_path_buf(), project_trusted)
            .load()
            .map_err(config_error)?;
        Ok((snapshot.enabled_tools(), snapshot.disabled_tools()))
    }
}
