//! Paging the saved sessions the store holds, merged with the live ones.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::{Value, json};
use vibe_app_server::client::{HeadlessService, TurnDriver};

use crate::agent::{AcpAgent, SESSION_LIST_PAGE_SIZE};
use crate::protocol::{AcpError, AcpSessionInfo, AcpSessionList};
use crate::session::{
    acp_session_info, decode_session_cursor, encode_session_cursor, require_absolute_cwd, same_path,
};

const MAX_SESSION_LIST_SCAN: usize = 100_000;
const SESSION_LIST_SCAN_PAGE: usize = 500;

struct SavedScan {
    saved: Vec<Value>,
    matched: BTreeSet<String>,
    complete: bool,
}

impl<D> AcpAgent<D>
where
    D: TurnDriver,
{
    pub fn list_sessions(
        &self,
        cwd: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<AcpSessionList, AcpError> {
        self.require_initialized()?;
        if let Some(cwd) = cwd {
            require_absolute_cwd(cwd)?;
        }
        let offset = decode_session_cursor(cursor)?;
        let working_directory = cwd.map_or_else(current_directory, ToOwned::to_owned);
        let live = self.live_sessions(cwd)?;
        let scan = self.with_probe(&working_directory, &[], |service| {
            Self::scan_saved_sessions(service, cwd, offset, &live)
        })?;

        // Live sessions the store has not recorded only have a stable position
        // once the whole listing is known, so they are appended at the end.
        let unrecorded = if scan.complete {
            live.iter()
                .filter(|(session_id, _)| !scan.matched.contains(*session_id))
                .map(|(session_id, cwd)| (session_id.clone(), cwd.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let total = scan.saved.len().saturating_add(unrecorded.len());
        let start = offset.min(total);
        let end = start.saturating_add(SESSION_LIST_PAGE_SIZE).min(total);
        let wanted = end.saturating_sub(start);

        // Only the requested page is projected. The scan holds everything it
        // had to read to locate the live sessions, which can be far more.
        let mut sessions = Vec::with_capacity(wanted);
        for session in scan.saved.iter().skip(start).take(wanted) {
            let mut info = acp_session_info(session)?;
            if live.contains_key(&info.session_id) {
                info.meta.insert("active".to_owned(), json!(true));
            }
            sessions.push(info);
        }
        let tail = wanted.saturating_sub(sessions.len());
        sessions.extend(
            unrecorded
                .into_iter()
                .skip(start.saturating_sub(scan.saved.len()))
                .take(tail)
                .map(|(session_id, cwd)| AcpSessionInfo {
                    session_id,
                    cwd,
                    title: None,
                    updated_at: None,
                    additional_directories: None,
                    meta: BTreeMap::from([("active".to_owned(), json!(true))]),
                }),
        );
        let exhausted = scan.complete && end == total;
        Ok(AcpSessionList {
            sessions,
            next_cursor: (!exhausted).then(|| encode_session_cursor(end)),
        })
    }

    /// Reads saved sessions until the requested page can be served and every
    /// live session has been located. Live sessions sort first in the store
    /// (most recently updated), so this normally stops after one page.
    fn scan_saved_sessions(
        service: &mut HeadlessService<D>,
        cwd: Option<&str>,
        offset: usize,
        live: &BTreeMap<String, String>,
    ) -> Result<SavedScan, AcpError> {
        let needed = offset.saturating_add(SESSION_LIST_PAGE_SIZE);
        let mut scan = SavedScan {
            saved: Vec::new(),
            matched: BTreeSet::new(),
            complete: false,
        };
        let mut app_offset = 0_usize;
        loop {
            let mut params = json!({"offset": app_offset, "limit": SESSION_LIST_SCAN_PAGE});
            if let Some(cwd) = cwd {
                params["cwd"] = json!(cwd);
            }
            let result = service.public_call("session/list", params)?;
            let page = result
                .get("sessions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for session in &page {
                if let Some(session_id) = session.get("id").and_then(Value::as_str)
                    && live.contains_key(session_id)
                {
                    scan.matched.insert(session_id.to_owned());
                }
            }
            let received = page.len();
            scan.saved.extend(page);
            if scan.saved.len() > MAX_SESSION_LIST_SCAN {
                return Err(AcpError::InvalidResponse(format!(
                    "session/list exceeded the {MAX_SESSION_LIST_SCAN}-session scan limit"
                )));
            }
            // `SessionListResponse` declares the page and nothing else, so the
            // scan advances by what it received and stops on a short page,
            // which is the same signal a cursor carried.
            if received < SESSION_LIST_SCAN_PAGE {
                scan.complete = true;
                return Ok(scan);
            }
            if scan.saved.len() >= needed && scan.matched.len() == live.len() {
                return Ok(scan);
            }
            app_offset = app_offset.saturating_add(received);
        }
    }

    fn live_sessions(&self, cwd: Option<&str>) -> Result<BTreeMap<String, String>, AcpError> {
        Ok(self
            .lock_state()?
            .sessions
            .iter()
            .filter_map(|(session_id, slot)| {
                let harness = slot.harness()?;
                cwd.is_none_or(|filter| same_path(filter, &harness.cwd))
                    .then(|| (session_id.clone(), harness.cwd.clone()))
            })
            .collect())
    }
}

fn current_directory() -> String {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .to_string_lossy()
        .into_owned()
}
