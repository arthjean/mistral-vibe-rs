//! Listing the saved sessions: the ones a directory holds, newest first, a
//! page at a time, and the one `--continue` would reopen.
//!
//! Reference `project_session_list` and `_session_roots`
//! (`vibe/app_server/_host.py`).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::*;

impl ServerConnection {
    /// Reference `project_session_list`.
    pub(super) fn list_sessions(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let ListParams {
            cursor,
            limit,
            root_session_id,
            parent_session_id,
            cwd,
            cwds,
        } = wire_validation::validate_params(&request.method, &request.params).map_err(rejected)?;
        let store = self.store();
        store.migrate_legacy().map_err(storage_fault)?;
        // `cwds` is the union of what `cwd` matches for each entry, so an
        // explicit empty list matches nothing.
        let requested: Vec<Option<String>> = match cwds {
            Some(cwds) => cwds.into_iter().map(Some).collect(),
            None => vec![cwd],
        };
        let mut sessions: Vec<SessionInfo> = Vec::new();
        for cwd in &requested {
            for session in self.resume_sessions(cwd.as_deref())? {
                if !sessions
                    .iter()
                    .any(|known| known.session_id == session.session_id)
                {
                    sessions.push(session);
                }
            }
        }
        let roots = session_roots(&sessions);
        let mut filtered: Vec<SessionInfo> = sessions
            .into_iter()
            .filter(|session| {
                root_session_id
                    .as_ref()
                    .is_none_or(|root| roots.get(&session.session_id) == Some(root))
                    && parent_session_id
                        .as_ref()
                        .is_none_or(|parent| session.parent_session_id.as_ref() == Some(parent))
            })
            .collect();
        sort_sessions(&mut filtered);
        let start = session_cursor_index(&filtered, cursor.as_deref());
        let page: Vec<&SessionInfo> = filtered.iter().skip(start).take(limit).collect();
        let next_cursor = (start + page.len() < filtered.len())
            .then(|| page.last().map(|session| encode_session_cursor(session)))
            .flatten();
        let items: Vec<Value> = page
            .iter()
            .map(|session| {
                let root = roots
                    .get(&session.session_id)
                    .cloned()
                    .unwrap_or_else(|| session.session_id.clone());
                listed_session(&store, session, &root)
            })
            .collect();
        let continue_session_id = self.continue_session_id(&filtered);
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("items", json!(items)),
                ("nextCursor", json!(next_cursor)),
                ("previousCursor", Value::Null),
                ("continueSessionId", json!(continue_session_id)),
            ]),
        ))
    }

    /// Reference `_continue_resume_sessions`: the sessions begun in or moved
    /// to `cwd`, then the ones whose directory resolves to it, or maps onto it
    /// from a retained worktree.
    pub(super) fn resume_sessions(
        &self,
        cwd: Option<&str>,
    ) -> Result<Vec<SessionInfo>, ProtocolFault> {
        let store = self.store();
        let mut sessions = store.sessions(cwd).map_err(storage_fault)?;
        let Some(cwd) = cwd else {
            return Ok(sessions);
        };
        let requested = resolve(cwd);
        let managed = self.server.workspace.managed_worktrees();
        let listed: BTreeSet<String> = sessions
            .iter()
            .map(|session| session.session_id.clone())
            .collect();
        for session in store.sessions(None).map_err(storage_fault)? {
            if listed.contains(&session.session_id) {
                continue;
            }
            let session_cwd = resolve(&session.cwd);
            let matches = session_cwd == requested
                || ManagedWorktree::at(&managed, &session_cwd)
                    .and_then(|held| held.retained_repository_mapping(&session_cwd))
                    .is_some_and(|mapping| {
                        requested.starts_with(&mapping.root) && mapping.cwd.starts_with(&requested)
                    });
            if matches {
                sessions.push(session);
            }
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    /// Reference `_continue_session_id`: the terminal's last session when the
    /// page offers it, and otherwise the most recent one.
    pub(super) fn continue_session_id(&self, filtered: &[SessionInfo]) -> Option<String> {
        let first = filtered.first()?;
        let pointer = self
            .server
            .workspace
            .session_logging()
            .enabled
            .then(|| self.store().pointer())
            .flatten();
        Some(
            pointer
                .filter(|pointer| {
                    filtered
                        .iter()
                        .any(|session| &session.session_id == pointer)
                })
                .unwrap_or_else(|| first.session_id.clone()),
        )
    }
}

/// Reference `_session_roots`: each session's furthest ancestor among the
/// listed ones, or itself.
fn session_roots(sessions: &[SessionInfo]) -> BTreeMap<String, String> {
    let parents: BTreeMap<&str, Option<&str>> = sessions
        .iter()
        .map(|session| {
            (
                session.session_id.as_str(),
                session.parent_session_id.as_deref(),
            )
        })
        .collect();
    let mut roots = BTreeMap::new();
    for session_id in parents.keys() {
        let mut seen = BTreeSet::new();
        let mut current = *session_id;
        while let Some(Some(parent)) = parents.get(current)
            && parents.contains_key(parent)
        {
            if !seen.insert(current) {
                current = session_id;
                break;
            }
            current = parent;
        }
        roots.insert((*session_id).to_owned(), current.to_owned());
    }
    roots
}

/// Most recently updated first, the identifier breaking a tie.
fn sort_sessions(sessions: &mut [SessionInfo]) {
    sessions.sort_by(|left, right| {
        (&right.updated_at, &right.session_id).cmp(&(&left.updated_at, &left.session_id))
    });
}

/// Reference `_encode_session_cursor`.
fn encode_session_cursor(session: &SessionInfo) -> String {
    URL_SAFE_NO_PAD.encode(format!("{}\0{}", session.updated_at, session.session_id))
}

/// Reference `_session_cursor_index`: past the session the cursor names, and
/// past everything when it names none.
fn session_cursor_index(sessions: &[SessionInfo], cursor: Option<&str>) -> usize {
    let Some(cursor) = cursor else {
        return 0;
    };
    let decoded = URL_SAFE_NO_PAD
        .decode(cursor.trim_end_matches('='))
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok());
    let Some((updated_at, session_id)) = decoded
        .as_deref()
        .and_then(|decoded| decoded.split_once('\0'))
    else {
        return sessions.len();
    };
    sessions
        .iter()
        .position(|session| session.updated_at == updated_at && session.session_id == session_id)
        .map_or(sessions.len(), |index| index + 1)
}

/// Reference `SessionListParams`; the pinned and archived filters are a
/// unified host's own.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListParams {
    cursor: Option<String>,
    #[serde(default = "default_list_limit")]
    limit: usize,
    root_session_id: Option<String>,
    parent_session_id: Option<String>,
    cwd: Option<String>,
    cwds: Option<Vec<String>>,
}

const fn default_list_limit() -> usize {
    50
}
