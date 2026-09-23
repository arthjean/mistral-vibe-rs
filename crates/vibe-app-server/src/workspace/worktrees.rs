//! The `workspace/git/worktrees/*` host methods: the listing a worktree picker
//! reads, the retention limit and its sweep, and the removal a client asks for
//! once it deleted the session that ran in a worktree.
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
struct LimitUpdateParams {
    limit: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PruneParams {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RemoveParams {
    cwd: String,
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
        let params: RemoveParams = parse(params)?;
        let answer = crate::worktrees::remove_response(
            required_path(&params.cwd)?,
            &self.managed_worktrees(),
        );
        Ok(WorkspaceDispatch::result(
            answer.as_object().cloned().unwrap_or_default(),
        ))
    }
}
