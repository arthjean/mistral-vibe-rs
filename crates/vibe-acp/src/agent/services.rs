//! Building the canonical services one editor session runs over.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vibe_app_server::client::{HeadlessService, TurnDriver};
use vibe_app_server::experiments::SessionExperiments;
use vibe_app_server::projects::ProjectsService;
use vibe_app_server::server::AppServer;
use vibe_app_server::workspace::WorkspaceService;
use vibe_protocol::{
    CallbackKind, ClientCapabilities, ClientEntrypoint, ClientInfo, TerminalEmulator,
};

use vibe_core::telemetry::LaunchContext;

use crate::agent::AcpAgent;

/// What the session's client descriptor names when the editor declared none.
const ACP_CLIENT_NAME: &str = "vibe_acp_client";
const ACP_CLIENT_VERSION: &str = "unknown";
use crate::client_tools::declared_client_tools;
use crate::protocol::AcpError;
use crate::session::AcpHarness;

impl<D> AcpAgent<D>
where
    D: TurnDriver,
{
    /// The enrollment one session resolves, or [`None`] when this process
    /// resolves none.
    ///
    /// Built before the session starts, because building it applies the
    /// rollout cached for this user to the configuration the session is about
    /// to read, as reference `_build_session_config` does.
    pub(in crate::agent) fn session_experiments(
        &self,
        service: &HeadlessService<D>,
    ) -> Option<Arc<SessionExperiments>> {
        let experiments = self.experiments.as_ref()?;
        // Reference `_build_launch_context_from_services`: every session reports
        // the client descriptor the app server was handed, which for an editor
        // names the editor rather than this adapter.
        let client_info = self.lock_state().ok()?.client_info.clone();
        let launch = LaunchContext {
            client_name: client_info
                .as_ref()
                .map_or_else(|| ACP_CLIENT_NAME.to_owned(), |info| info.name.clone()),
            client_version: client_info.as_ref().map_or_else(
                || ACP_CLIENT_VERSION.to_owned(),
                |info| info.version.clone(),
            ),
            terminal_emulator: Some("unknown".to_owned()),
            ..experiments.launch.clone()
        };
        experiments.declared_launch.declare(launch.clone());
        Some(Arc::new(
            SessionExperiments::new(
                &service.workspace_service(),
                Arc::clone(&experiments.credentials),
                Some(launch),
                experiments.exposures.clone(),
            )
            .reporting_to(Arc::clone(&self.telemetry)),
        ))
    }

    /// One adopted harness, with the enrollment [`Self::session_experiments`]
    /// built for its service attached.
    pub(in crate::agent) fn adopt(
        service: HeadlessService<D>,
        session_id: &str,
        experiments: Option<Arc<SessionExperiments>>,
    ) -> Result<AcpHarness<D>, AcpError> {
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
        let (capabilities, client_info) = {
            let state = self.lock_state()?;
            (state.capabilities(), state.client_info.clone())
        };
        let mut server = AppServer::default()
            .using_client_telemetry(Arc::clone(&self.telemetry))
            .using_harness_selection(self.harness);
        if let Some(projects) = self.shared_projects_service()? {
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
                        .map_or_else(|| ACP_CLIENT_NAME.to_owned(), |info| info.name.clone()),
                    version: client_info.as_ref().map_or_else(
                        || ACP_CLIENT_VERSION.to_owned(),
                        |info| info.version.clone(),
                    ),
                    title: client_info.and_then(|info| info.title),
                    entrypoint: ClientEntrypoint::Acp,
                    terminal_emulator: TerminalEmulator::Unknown,
                },
                ClientCapabilities {
                    // Reference `_client_descriptor`: approvals always, and
                    // questions only for a client that renders a form.
                    callback_kinds: std::iter::once(CallbackKind::Approval)
                        .chain(
                            capabilities
                                .elicitation_form
                                .then_some(CallbackKind::UserInput),
                        )
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

    /// The one project service every session's server shares, so the
    /// scheduled loops of every session live in one store. The project links
    /// need nothing from it: each server reads them from its vibe home.
    fn shared_projects_service(&self) -> Result<Option<ProjectsService>, AcpError> {
        if !self.shared_projects {
            return Ok(None);
        }
        let mut cached = self.projects.lock().map_err(|_| AcpError::StatePoisoned)?;
        Ok(Some(
            cached.get_or_insert_with(ProjectsService::default).clone(),
        ))
    }
}
