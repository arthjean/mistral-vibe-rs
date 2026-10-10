//! The `workspace/git/worktrees/*` host methods: the listing a worktree picker
//! reads, the retention limit and its sweep, and the removal a client asks for
//! once it deleted the session that ran in a worktree, plus
//! `workspace/git/checkouts`, which reads every repository a project links.
//!
//! They sit on the workspace service because two of them read and write the
//! configuration, and all four need the vibe home the managed root lives under
//! (`vibe/app_server/_host.py:445-482`).

use vibe_core::worktree::{ManagedRoot, ManagedWorktree};

use super::*;

/// The largest limit `workspace/git/worktrees/limit/update` accepts.
const MAX_WORKTREE_LIMIT: i64 = 100;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ListParams {
    cwd: String,
    #[serde(default)]
    include_details: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CheckoutsParams {
    repo_local_paths: Vec<String>,
    #[serde(default)]
    session_cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct LimitUpdateParams {
    limit: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PruneParams {}

/// Reference `WorkspaceWorktreeRemoveConfirmParams`: the confirmation flags
/// a client asks before it discards (`vibe/app_server/protocol.py:1657-1677`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RemoveParams {
    cwd: String,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    delete_branch: Option<bool>,
    #[serde(default)]
    inspect: bool,
}

/// Reference `WorkspaceWorktreeReapParams` and
/// `WorkspaceWorktreeReapCancelParams`, whose request identities the wire
/// validation already checked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ReapParams {
    cwd: String,
    #[serde(default)]
    requester_id: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
}

/// The session the asking connection is attached to, which the router hands
/// the worktree methods beside the client's own parameters: its holder is the
/// one that never counts as in use (`vibe/app_server/server.py:765-775`).
fn connection_root(params: &BTreeMap<String, Value>) -> (BTreeMap<String, Value>, Option<String>) {
    let mut params = params.clone();
    let root = params
        .remove(crate::server::CONNECTION_ROOT_PARAM)
        .and_then(|root| root.as_str().map(ToOwned::to_owned));
    (params, root)
}

fn parse<T: serde::de::DeserializeOwned>(
    params: &BTreeMap<String, Value>,
) -> Result<T, WorkspaceServiceError> {
    serde_json::from_value(Value::Object(
        params.clone().into_iter().collect::<Map<_, _>>(),
    ))
    .map_err(|error| WorkspaceServiceError::InvalidParams(error.to_string()))
}

fn required_path(cwd: &str) -> Result<&Path, WorkspaceServiceError> {
    if cwd.is_empty() {
        return Err(WorkspaceServiceError::InvalidParams(
            "cwd must not be empty".to_owned(),
        ));
    }
    Ok(Path::new(cwd))
}

fn git_refusal(error: impl std::fmt::Display) -> WorkspaceServiceError {
    WorkspaceServiceError::InvalidParams(error.to_string())
}

impl WorkspaceService {
    /// The managed root this service's sessions raise worktrees under.
    #[must_use]
    pub fn managed_worktrees(&self) -> ManagedRoot {
        ManagedRoot::for_vibe_home(&self.paths.vibe_home)
    }

    /// Every repository a project links, read as git.
    pub(super) fn git_checkouts(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let params: CheckoutsParams = parse(params)?;
        let answer = crate::worktrees::checkouts_response(
            &params.repo_local_paths,
            params.session_cwd.as_deref().map(Path::new),
            &self.managed_worktrees(),
        );
        Ok(WorkspaceDispatch::result(
            answer.as_object().cloned().unwrap_or_default(),
        ))
    }

    /// The configured retention limit, read on every call so a value written
    /// after startup applies to the next sweep.
    pub(crate) fn worktree_limit(&self) -> Result<usize, WorkspaceServiceError> {
        Ok(self.config.load().map_err(config_error)?.worktree_limit())
    }

    pub(super) fn worktrees_list(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let params: ListParams = parse(params)?;
        let listing = crate::worktrees::list_response(
            required_path(&params.cwd)?,
            params.include_details,
            &self.managed_worktrees(),
        )
        .map_err(git_refusal)?;
        Ok(WorkspaceDispatch::result(
            listing.as_object().cloned().unwrap_or_default(),
        ))
    }

    /// Writes the retention limit through the patch surface, answering the
    /// limit the configuration holds afterward and the targets whose write did
    /// not land.
    pub(super) fn worktrees_limit_update(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let params: LimitUpdateParams = parse(params)?;
        if !(0..=MAX_WORKTREE_LIMIT).contains(&params.limit) {
            return Err(WorkspaceServiceError::InvalidParams(format!(
                "limit must be between 0 and {MAX_WORKTREE_LIMIT}"
            )));
        }
        let operation = ConfigPatchOp {
            mutation: ConfigMutation::set(["worktree_limit"], TomlValue::Integer(params.limit)),
            target: None,
        };
        let failures = match self
            .config
            .apply_patch(&[operation], "desktop worktree retention setting")
        {
            Ok(outcome) => outcome.failures,
            Err(vibe_core::config::ConfigError::PatchRejected(reason)) => vec![reason],
            Err(error) => return Err(config_error(error)),
        };
        Ok(WorkspaceDispatch::result([
            ("limit", json!(self.worktree_limit()?)),
            ("failures", json!(failures)),
        ]))
    }

    /// Reclaims the oldest inactive managed worktrees beyond the limit.
    pub(super) fn worktrees_prune(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let PruneParams {} = parse(params)?;
        let removed = ManagedWorktree::prune(&self.managed_worktrees(), self.worktree_limit()?)
            .map_err(git_refusal)?;
        Ok(WorkspaceDispatch::result([("removed", json!(removed))]))
    }

    pub(super) fn worktrees_remove(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let (params, session_id) = connection_root(params);
        let params: RemoveParams = parse(&params)?;
        let answer = crate::worktrees::remove_response(
            required_path(&params.cwd)?,
            &self.managed_worktrees(),
            &crate::worktrees::RemoveRequest {
                force: params.force,
                delete_branch: params.delete_branch,
                session_id: session_id.as_deref(),
                inspect: params.inspect,
            },
        );
        Ok(WorkspaceDispatch::result(
            answer.as_object().cloned().unwrap_or_default(),
        ))
    }

    /// Reference `workspace/git/worktrees/reap`: the worktree `cwd` sits in is
    /// snapshotted and removed once nobody holds it, and the request is kept
    /// until then (`vibe/app_server/_host.py:516-523`).
    pub(super) fn worktrees_reap(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let (params, _) = connection_root(params);
        let params: ReapParams = parse(&params)?;
        let request = params
            .requester_id
            .as_deref()
            .zip(params.request_id.as_deref());
        let answer = crate::worktrees::reap_response(
            required_path(&params.cwd)?,
            &self.managed_worktrees(),
            request,
        );
        Ok(WorkspaceDispatch::result(
            answer.as_object().cloned().unwrap_or_default(),
        ))
    }

    /// Reference `workspace/git/worktrees/reap/cancel`, which answers nothing
    /// (`vibe/app_server/_host.py:524-532`).
    pub(super) fn worktrees_reap_cancel(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let (params, _) = connection_root(params);
        let params: ReapParams = parse(&params)?;
        crate::worktrees::cancel_reap(
            required_path(&params.cwd)?,
            &self.managed_worktrees(),
            params.requester_id.as_deref(),
            params.request_id.as_deref(),
        )
        .map_err(git_refusal)?;
        Ok(WorkspaceDispatch::result(
            std::iter::empty::<(&str, Value)>(),
        ))
    }
}
