//! `workspace/trust/status`, `workspace/trust/decision` and
//! `workspace/trust/untrustedConfig`: what the trust store says about a
//! directory, and the one write a client may make to it.
//!
//! Reference `_dispatch_workspace` (`vibe/app_server/_host.py`) for the two
//! reads and a decision that names no session, and `_workspace_trust_decision`
//! (`vibe/app_server/_handler.py`) for one that does. The reads answer about
//! any directory, the server's own by default; a decision about a session is
//! pinned to that session's working directory, refused while a grant would
//! land mid-turn, and a grant re-derives the session's tool surface.

use super::*;
use crate::startup::{
    WorkspaceTrustDecision, WorkspaceTrustError, decide_workspace_trust,
    read_untrusted_config_dirs, read_workspace_trust,
};
use vibe_core::trust::{self, TrustStore};

/// The parameters one of the three methods declares, read as the reference's
/// pydantic models read them: camelCase keys only, a nullable string for the
/// directory and the session, the decision one of three literals, and no other
/// key.
struct TrustParams {
    cwd: Option<String>,
    session_id: Option<String>,
    decision: Option<WorkspaceTrustDecision>,
}

const DECISION_LITERALS: &str = "Input should be 'trust_repo', 'trust_cwd' or 'decline'";

fn trust_params(
    params: &BTreeMap<String, Value>,
    fields: &[&str],
) -> Result<TrustParams, ParamsRejection> {
    let mut issues = Vec::new();
    let text = |field: &str, issues: &mut Vec<InvalidParamsIssue>| match params.get(field) {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(_) => {
            issues.push(issue(field, "Input should be a valid string"));
            None
        }
    };
    let decision = if fields.contains(&"decision") {
        match params.get("decision") {
            None => {
                issues.push(issue("decision", "Field required"));
                None
            }
            Some(value) => {
                let parsed = value.as_str().and_then(WorkspaceTrustDecision::parse);
                if parsed.is_none() {
                    issues.push(issue("decision", DECISION_LITERALS));
                }
                parsed
            }
        }
    } else {
        None
    };
    let mut read = TrustParams {
        cwd: text("cwd", &mut issues),
        session_id: None,
        decision,
    };
    if fields.contains(&"sessionId") {
        read.session_id = text("sessionId", &mut issues);
    }
    for key in params.keys() {
        if !fields.contains(&key.as_str()) {
            issues.push(issue(key, "Extra inputs are not permitted"));
        }
    }
    if issues.is_empty() {
        Ok(read)
    } else {
        Err(ParamsRejection::with_issues(issues))
    }
}

fn issue(field: &str, message: &str) -> InvalidParamsIssue {
    InvalidParamsIssue {
        path: vec![PathSegment::Field(field.to_owned())],
        message: message.to_owned(),
    }
}

/// A trust refusal, which upstream raises as `invalid_params` with no detail.
fn refused(error: &WorkspaceTrustError) -> ProtocolFault {
    ProtocolFault::plain(ProtocolErrorCode::InvalidParams, error.to_string())
}

impl ServerConnection {
    pub(super) fn trust_request(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.answer_trust(request))
    }

    fn trust_store(&self) -> TrustStore {
        TrustStore::for_vibe_home(self.server.workspace.vibe_home())
    }

    /// The directory a request names, or the one the server runs in.
    fn trust_directory(&self, cwd: Option<&str>) -> PathBuf {
        cwd.map_or_else(
            || self.server.workspace.working_directory().to_path_buf(),
            PathBuf::from,
        )
    }

    fn answer_trust(&mut self, request: ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let store = self.trust_store();
        let answer = match request.method.as_str() {
            "workspace/trust/status" => {
                let params = trust_params(&request.params, &["cwd"])?;
                read_workspace_trust(&store, &self.trust_directory(params.cwd.as_deref()))
            }
            "workspace/trust/untrustedConfig" => {
                let params = trust_params(&request.params, &["cwd"])?;
                read_untrusted_config_dirs(&store, &self.trust_directory(params.cwd.as_deref()))
            }
            _ => {
                let params = trust_params(&request.params, &["decision", "cwd", "sessionId"])?;
                let decision = params
                    .decision
                    .ok_or_else(|| ProtocolFault::invalid_params("decision is required"))?;
                match params.session_id {
                    Some(session_id) => {
                        return self.decide_session_trust(
                            request.id,
                            &store,
                            &session_id,
                            params.cwd.as_deref(),
                            decision,
                        );
                    }
                    None => decide_workspace_trust(
                        &store,
                        &self.trust_directory(params.cwd.as_deref()),
                        decision,
                    )
                    .map_err(|error| refused(&error))?,
                }
            }
        };
        Ok(success_batch(request.id, answer.into_iter().collect()))
    }

    /// Reference `_workspace_trust_decision`: the decision is pinned to the
    /// session's own working directory, a grant waits for the session to be
    /// idle, and a grant, unlike a decline, re-derives what the session reads
    /// from its project and says so on `runtime/updated`.
    fn decide_session_trust(
        &mut self,
        id: RequestId,
        store: &TrustStore,
        session_id: &str,
        requested: Option<&str>,
        decision: WorkspaceTrustDecision,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let (working_directory, busy) = {
            let sessions = self.server.lock_sessions()?;
            let session = sessions.get(session_id).ok_or_else(|| {
                ProtocolFault::new(
                    ProtocolErrorCode::NotFound,
                    format!("Session not found: {session_id}"),
                )
            })?;
            (
                session.working_directory.clone(),
                session.active_turn.is_some(),
            )
        };
        if let Some(batch) = self.attachment_error(id.clone(), session_id) {
            return Ok(batch);
        }
        let target = trust::resolve(Path::new(&working_directory));
        if requested.is_some_and(|requested| trust::resolve(Path::new(requested)) != target) {
            return Err(refused(&WorkspaceTrustError::OutsideSession));
        }
        let grant = matches!(
            decision,
            WorkspaceTrustDecision::TrustRepository | WorkspaceTrustDecision::TrustDirectory
        );
        if grant && busy {
            return Err(ProtocolFault::new(
                ProtocolErrorCode::Conflict,
                format!("Session {session_id} is running a turn"),
            ));
        }
        let answer =
            decide_workspace_trust(store, &target, decision).map_err(|error| refused(&error))?;
        if !grant {
            return Ok(success_batch(id, answer.into_iter().collect()));
        }
        // The project roots follow the new trust at once; the project file
        // keeps the verdict the session started with, as upstream's layer
        // cache does.
        let trusted = store.is_trusted(&target) == Some(true);
        if let Some(session) = self.server.lock_sessions()?.get_mut(session_id) {
            session.intent.trusted = trusted;
        }
        self.server
            .refresh_session_workspace_tools(session_id)
            .map_err(|error| ProtocolFault::internal(error.to_string()))?;
        let mut batch = success_batch(id, answer.into_iter().collect());
        batch.outbound.extend(signal_frames(
            &self.server,
            session_id,
            &ResourceSignals {
                runtime_updated: true,
                ..ResourceSignals::default()
            },
        ));
        Ok(batch)
    }
}
