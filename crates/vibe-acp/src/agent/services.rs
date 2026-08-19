//! Building the canonical services one editor session runs over.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vibe_app_server::client::{HeadlessService, TurnDriver};
use vibe_app_server::experiments::SessionExperiments;
use vibe_app_server::projects::{ProjectsService, VibeCodeCloudConfig};
use vibe_app_server::server::AppServer;
use vibe_app_server::workspace::WorkspaceService;
use vibe_core::config::DotenvValues;
use vibe_protocol::{
    CallbackKind, ClientCapabilities, ClientEntrypoint, ClientInfo, TerminalEmulator,
};

use crate::agent::AcpAgent;
use crate::client_tools::{AcpClientToolFactory, declared_client_tools};
use crate::protocol::AcpError;
use crate::session::AcpHarness;

impl<D> AcpAgent<D>
where
    D: TurnDriver,
{
    /// The enrollment one session resolves, or [`None`] when this process
    /// resolves none.
    fn session_experiments(&self, service: &HeadlessService<D>) -> Option<Arc<SessionExperiments>> {
        let experiments = self.experiments.as_ref()?;
        Some(Arc::new(SessionExperiments::new(
            &service.workspace_service(),
            Arc::clone(&experiments.credentials),
            Some(experiments.launch.clone()),
            experiments.exposures.clone(),
        )))
    }

    /// One adopted harness, with the enrollment this process resolves attached.
    pub(in crate::agent) fn adopt(
        &self,
        service: HeadlessService<D>,
        session_id: &str,
    ) -> Result<AcpHarness<D>, AcpError> {
        let experiments = self.session_experiments(&service);
        let harness = AcpHarness::adopt(service, session_id)?;
        Ok(match experiments {
            Some(experiments) => harness.resolving_experiments(experiments),
            None => harness,
        })
    }

    pub(in crate::agent) fn new_service(
        &self,
        working_directory: &str,
        additional_directories: &[String],
    ) -> Result<HeadlessService<D>, AcpError> {
        let (capabilities, client_info) = self.lock_state()?.client_context();
        let mut server = AppServer::default()
            .using_session_tool_factory(Arc::new(AcpClientToolFactory {
                client: self.client.clone(),
                capabilities: capabilities.clone(),
                timeout: self.client_tool_timeout,
            }))
            .using_client_telemetry(Arc::clone(&self.telemetry));
        if let Some(projects) = self.production_projects()? {
            server = server.using_projects_service(projects);
        }
        if let Some(session_root) = &self.session_root {
            let workspace = WorkspaceService::for_runtime_session_root(
                session_root,
                Path::new(working_directory),
            )
            .with_allowed_roots(additional_directories.iter().map(PathBuf::from).collect());
            // The editor session starts here, so an older configuration file is
            // brought forward before the first read. A failure to write is
            // carried by the configuration snapshot, not raised.
            workspace
                .migrate_configuration()
                .map_err(|error| AcpError::Configuration(error.to_string()))?;
            server = server.using_workspace_service(workspace);
        }
        Ok(
            HeadlessService::new_interactive_shared_with_server_and_client(
                self.driver.clone(),
                server,
                ClientInfo {
                    name: client_info
                        .as_ref()
                        .map_or_else(|| "vibe_acp_client".to_owned(), |info| info.name.clone()),
                    version: client_info
                        .as_ref()
                        .map_or_else(|| "unknown".to_owned(), |info| info.version.clone()),
                    title: client_info.and_then(|info| info.title),
                    entrypoint: ClientEntrypoint::Acp,
                    terminal_emulator: TerminalEmulator::Unknown,
                },
                ClientCapabilities {
                    callback_kinds: self
                        .client
                        .is_some()
                        .then_some(CallbackKind::Approval)
                        .into_iter()
                        .collect(),
                    client_tools: declared_client_tools(&capabilities),
                    // The bridge renders every notification the server sends, so
                    // it mutes none of them.
                    disabled_notifications: Vec::new(),
                },
            )?,
        )
    }

    /// Runs `work` against a throwaway service and stops that service on every
    /// outcome.
    ///
    /// [`HeadlessService`] has no `Drop`: shutting it down is an explicit call,
    /// so a probe whose work fails would otherwise be left running. The work's
    /// own failure wins over a shutdown failure, since it is the one that
    /// explains what happened.
    pub(crate) fn with_probe<T>(
        &self,
        working_directory: &str,
        additional_directories: &[String],
        work: impl FnOnce(&mut HeadlessService<D>) -> Result<T, AcpError>,
    ) -> Result<T, AcpError> {
        let mut probe = self.new_service(working_directory, additional_directories)?;
        let result = work(&mut probe);
        let stopped = probe.shutdown();
        result.and_then(|value| stopped.map(|()| value).map_err(AcpError::from))
    }

    /// Cloud services are resolved lazily so sessions start without a
    /// provider credential.
    fn production_projects(&self) -> Result<Option<ProjectsService>, AcpError> {
        if !self.production_cloud {
            return Ok(None);
        }
        let mut cached = self.projects.lock().map_err(|_| AcpError::StatePoisoned)?;
        if let Some(service) = cached.as_ref() {
            return Ok(Some(service.clone()));
        }
        // The session root sits inside the vibe home, so the global dotenv file
        // is resolvable here and a key kept there starts the cloud services the
        // same way an exported one does.
        let dotenv = self
            .session_root
            .as_deref()
            .and_then(Path::parent)
            .map_or_else(DotenvValues::default, DotenvValues::global);
        let Some(api_key) = dotenv
            .variable(&self.credential_environment)
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(None);
        };
        let config = VibeCodeCloudConfig::from_credential(api_key)
            .map_err(|error| AcpError::Configuration(error.to_string()))?;
        let service = ProjectsService::production(config)
            .map_err(|error| AcpError::Configuration(error.to_string()))?;
        *cached = Some(service.clone());
        Ok(Some(service))
    }
}
