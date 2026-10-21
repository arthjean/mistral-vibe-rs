//! Moving a saved session to another checkout of its repository.
//!
//! Reference `session/relocate` (`_relocate_session` in
//! `vibe/app_server/_host.py`).

use vibe_core::worktree::WorktreeRepository;

use super::*;

impl ServerConnection {
    /// Reference `session/relocate`: the root moves itself, and the store
    /// moves a session nobody holds (`_relocate_session`).
    pub(super) fn relocate_session(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let RelocateParams { session_id, cwd } =
            wire_validation::validate_params(&request.method, &request.params).map_err(rejected)?;
        if self.root_key().is_some() {
            return self.relocate_root(request, &session_id, &cwd);
        }
        let hydrated = self.load_saved(&session_id)?;
        let current = hydrated
            .metadata
            .environment
            .get("working_directory")
            .cloned()
            .flatten()
            .filter(|current| !current.is_empty())
            .ok_or_else(|| {
                ProtocolFault::plain(
                    ProtocolErrorCode::InvalidParams,
                    format!("Session has no working directory: {session_id}"),
                )
            })?;
        let target = relocation_target(&self.server.workspace, &current, &cwd)?;
        self.store()
            .relocate(&hydrated.metadata.id, &target)
            .map_err(storage_fault)?;
        let hydrated = self.load_saved(&session_id)?;
        Ok(success_batch(
            request.id.clone(),
            result_map([("state", stored_state(&session_id, &hydrated, 200))]),
        ))
    }

    /// Reference `_relocate`: the live session moves, and its history closes
    /// on the checkpoint that records where it went.
    fn relocate_root(
        &self,
        request: &ServerRequest,
        session_id: &str,
        cwd: &str,
    ) -> Result<DispatchBatch, ProtocolFault> {
        self.require_root(session_id)?;
        let previous = {
            let sessions = self.server.lock_sessions();
            let session = sessions
                .get(session_id)
                .ok_or_else(|| not_found(session_id))?;
            // Reference `SessionExecution.reserve`, which words the conflict
            // as `begin` does rather than as `require_idle`.
            if let Some(turn_id) = &session.active_turn {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::Conflict,
                    format!("Session is already running turn {turn_id}"),
                ));
            }
            session.working_directory.clone()
        };
        let target = relocation_target(&self.server.workspace, &previous, cwd)?;
        let store = self.store();
        let mut metadata = store.open(session_id).map_err(storage_fault)?.metadata;
        let mut sessions = self.server.lock_sessions();
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| not_found(session_id))?;
        if target != previous {
            vibe_core::storage::relocate_metadata(&mut metadata, &target);
            store.update_metadata(&metadata).map_err(storage_fault)?;
            if let Some(persisted) = session.persisted.as_mut() {
                persisted.metadata.environment = metadata.environment.clone();
                persisted
                    .metadata
                    .origin_directory
                    .clone_from(&metadata.origin_directory);
                persisted.metadata.working_directory.clone_from(&target);
            }
            session.working_directory.clone_from(&target);
            let checkpoint = checkpoint_entry(
                &session.id,
                "relocation",
                &format!("Moved to {target}"),
                json!({"cwd": target, "previousCwd": previous}),
            );
            if let Some(snapshot) = session.snapshot.as_mut() {
                snapshot.history.push(checkpoint);
            }
            session.turns.clear();
            session.latest_turn = None;
            session.updated_at = now_millis();
        }
        let moved = target != previous;
        let state = public_session_state(session);
        drop(sessions);
        // Reference `relocate` rebuilds the runtime with `reload_hooks=True`, so
        // the destination's hooks are the ones that run.
        if moved {
            self.server
                .rebind_session_workspace(session_id)
                .and_then(|()| self.server.reload_session_hooks(session_id))
                .map_err(|error| ProtocolFault::internal(error.to_string()))?;
        }
        let mut batch = success_batch(request.id.clone(), result_map([("state", state)]));
        batch
            .outbound
            .extend(self.runtime_updated_frame(session_id));
        Ok(batch)
    }
}

/// Reference `_relocation_target` and `_is_counterpart`: a directory, and one
/// of the checkouts of the repository the session already sits in.
fn relocation_target(
    workspace: &WorkspaceService,
    current: &str,
    requested: &str,
) -> Result<String, ProtocolFault> {
    let expanded = crate::host::expand_home(Path::new(requested));
    let target =
        vibe_core::platform::canonicalize(&expanded).unwrap_or_else(|_| absolute(&expanded));
    if !target.is_dir() {
        return Err(ProtocolFault::plain(
            ProtocolErrorCode::InvalidParams,
            format!("Not a directory: {}", target.display()),
        ));
    }
    let managed = workspace.managed_worktrees();
    let counterparts = WorktreeRepository::open(Path::new(current), &managed)
        .and_then(|repository| {
            let mut counterparts: Vec<PathBuf> = repository
                .linked()?
                .into_iter()
                .map(|worktree| {
                    vibe_core::platform::canonicalize(&worktree.path).unwrap_or(worktree.path)
                })
                .collect();
            counterparts.extend(repository.repository_counterpart());
            Ok(counterparts)
        })
        .unwrap_or_default();
    if !counterparts.contains(&target) {
        return Err(ProtocolFault::plain(
            ProtocolErrorCode::InvalidParams,
            format!(
                "Not a worktree of this session's repository: {}",
                target.display()
            ),
        ));
    }
    Ok(target.to_string_lossy().into_owned())
}

/// Reference `SessionRelocateParams`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelocateParams {
    session_id: String,
    cwd: String,
}
