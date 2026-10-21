//! The session operations each backend answers with methods of its own: a
//! unified session takes the reference's session methods (`session/fork`,
//! `session/history/list`, ...), a legacy one this port's `internal/` ones
//! over the store. Each operation picks the method family once, here, and an
//! answer whose shape differs by backend comes back as the variant that names
//! it.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::HeadlessService;
use crate::client::{ClientError, TurnDriver};

/// A page of a session's saved history, in the form its backend pages it.
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryPage {
    /// A `session/history/list` answer: public history entries, oldest first,
    /// and the cursor of the page before them.
    Entries(BTreeMap<String, Value>),
    /// A page of the transcript the store keeps.
    Stored(StoredPage),
}

/// An `internal/history/list` answer: the stored messages from `offset`.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredPage {
    pub offset: usize,
    pub page: BTreeMap<String, Value>,
}

/// The saved sessions of a working directory, in the form the backend lists
/// them.
#[derive(Debug, Clone, PartialEq)]
pub enum SavedSessions {
    /// A `session/list` answer, reference `SessionListResponse`.
    Listed(BTreeMap<String, Value>),
    /// An `internal/session/list` answer over the store.
    Stored(BTreeMap<String, Value>),
}

/// What a client opens a session's saved history from.
#[derive(Debug, Clone, PartialEq)]
pub enum OpeningHistory {
    /// The state `session/read` publishes, which holds the latest page of a
    /// unified session's saved history (reference `ClientState.history`).
    Published,
    /// The transcript the store keeps for a legacy session, when it was asked
    /// for and its record could be read.
    Stored(Option<StoredHistory>),
}

/// The tail of a stored transcript, and the context its record accounts.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredHistory {
    pub context_tokens: u64,
    /// The last page of the transcript, absent when the record lists no
    /// messages.
    pub tail: Option<StoredPage>,
}

impl<D> HeadlessService<D>
where
    D: TurnDriver,
{
    /// Reference `sessions.fork(entry_id, attach=False)`: a detached copy of
    /// `session_id`, cut after the user message `entry_id` when one is named.
    /// Answers the copy's identity, `None` when the answer names none.
    ///
    /// # Errors
    ///
    /// The fork was refused.
    pub fn fork_session(
        &mut self,
        session_id: &str,
        entry_id: Option<&str>,
    ) -> Result<Option<String>, ClientError> {
        if self.is_unified(session_id) {
            let result = self.public_call(
                "session/fork",
                json!({"sourceSessionId": session_id, "entryId": entry_id, "attach": false}),
            )?;
            return Ok(state_session_id(&result));
        }
        let mut params = json!({"sessionId": session_id});
        if let Some(entry_id) = entry_id {
            params["messageId"] = json!(entry_id);
        }
        let result = self.public_call("internal/session/fork", params)?;
        Ok(metadata_session_id(&result))
    }

    /// Opens the saved session `target` on the backend that runs
    /// `session_id`, and answers the identity it opened under.
    ///
    /// # Errors
    ///
    /// The resume was refused.
    pub fn resume_session(
        &mut self,
        session_id: &str,
        target: &str,
    ) -> Result<Option<String>, ClientError> {
        let params = json!({"sessionId": target});
        if self.is_unified(session_id) {
            let result = self.public_call("session/resume", params)?;
            return Ok(state_session_id(&result));
        }
        let result = self.public_call("internal/session/resume", params)?;
        Ok(metadata_session_id(&result))
    }

    /// Deletes the saved session `target` through the backend that runs
    /// `session_id`.
    ///
    /// # Errors
    ///
    /// The deletion was refused.
    pub fn delete_session(&mut self, session_id: &str, target: &str) -> Result<(), ClientError> {
        let method = if self.is_unified(session_id) {
            "session/delete"
        } else {
            "internal/session/delete"
        };
        self.public_call(method, json!({"sessionId": target}))
            .map(drop)
    }

    /// The first `limit` sessions saved for `cwd`, listed by the backend that
    /// runs `session_id`.
    ///
    /// # Errors
    ///
    /// The listing was refused.
    pub fn list_sessions(
        &mut self,
        session_id: &str,
        cwd: &str,
        limit: u64,
    ) -> Result<SavedSessions, ClientError> {
        if self.is_unified(session_id) {
            return self
                .public_call("session/list", json!({"cwd": cwd, "limit": limit}))
                .map(SavedSessions::Listed);
        }
        self.public_call(
            "internal/session/list",
            json!({"cwd": cwd, "offset": 0, "limit": limit, "sessionId": session_id}),
        )
        .map(SavedSessions::Stored)
    }

    /// Clears the conversation of `session_id`, which continues as a new
    /// session, and answers the new session's identity.
    ///
    /// # Errors
    ///
    /// The clear was refused.
    pub fn clear_history(&mut self, session_id: &str) -> Result<Option<String>, ClientError> {
        let params = json!({"sessionId": session_id});
        if self.is_unified(session_id) {
            let result = self.public_call("session/history/clear", params)?;
            return Ok(state_session_id(&result));
        }
        let result = self.public_call("internal/session/history/clear", params)?;
        Ok(metadata_session_id(&result))
    }

    /// Titles `session_id` `title`, and answers the title it now carries.
    ///
    /// # Errors
    ///
    /// The rename was refused.
    pub fn rename_session(&mut self, session_id: &str, title: &str) -> Result<String, ClientError> {
        let params = json!({"title": title, "sessionId": session_id});
        if self.is_unified(session_id) {
            let result = self.public_call("session/rename", params)?;
            return Ok(result
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(title)
                .to_owned());
        }
        self.public_call("internal/session/title/update", params)?;
        Ok(title.to_owned())
    }

    /// The skills the runtime of `session_id` holds, as reference
    /// `resources.runtime.skills` reads them: a unified session answers them
    /// from `runtime/read`, a legacy one lists them.
    ///
    /// # Errors
    ///
    /// The read was refused.
    pub fn session_skills(&mut self, session_id: &str) -> Result<Vec<Value>, ClientError> {
        let params = json!({"sessionId": session_id});
        let skills = if self.is_unified(session_id) {
            self.public_call("runtime/read", params)?
                .remove("runtime")
                .and_then(|mut runtime| runtime.get_mut("skills").map(Value::take))
        } else {
            self.public_call("skills/list", params)?.remove("skills")
        };
        Ok(match skills {
            Some(Value::Array(skills)) => skills,
            _ => Vec::new(),
        })
    }

    /// The cursor [`Self::history_before`] reads the page above the history
    /// `state` holds with, `state` being what `session/read` published for
    /// `session_id`: a unified session's `historyBeforeCursor` (reference
    /// `history_before_cursor`). A legacy session's published cursor counts
    /// its projection rather than its stored transcript, so it names none.
    #[must_use]
    pub fn history_cursor(&self, session_id: &str, state: &Value) -> Option<String> {
        if !self.is_unified(session_id) {
            return None;
        }
        state
            .get("historyBeforeCursor")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    }

    /// What a client opens the saved history of `session_id` from. A legacy
    /// session reads the last `tail` messages of its stored transcript, when
    /// `tail` asks for them; a record that cannot be read leaves none.
    ///
    /// # Errors
    ///
    /// The stored transcript could not be paged.
    pub fn opening_history(
        &mut self,
        session_id: &str,
        tail: Option<usize>,
    ) -> Result<OpeningHistory, ClientError> {
        if self.is_unified(session_id) {
            return Ok(OpeningHistory::Published);
        }
        let Some(limit) = tail else {
            return Ok(OpeningHistory::Stored(None));
        };
        // The session record carries both the transcript's length, which
        // decides the page to ask for, and the context accounting.
        let Ok(record) = self.public_call(
            "internal/session/log/read",
            json!({"sessionId": session_id}),
        ) else {
            return Ok(OpeningHistory::Stored(None));
        };
        let context_tokens = record
            .get("metadata")
            .and_then(|metadata| metadata.get("statistics"))
            .and_then(|statistics| statistics.get("context_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let count = record
            .get("messages")
            .and_then(Value::as_array)
            .map(Vec::len);
        let tail = match count {
            Some(count) => {
                let offset = count.saturating_sub(limit);
                let page = self.stored_page(session_id, offset, limit)?;
                Some(StoredPage { offset, page })
            }
            None => None,
        };
        Ok(OpeningHistory::Stored(Some(StoredHistory {
            context_tokens,
            tail,
        })))
    }

    /// The page of the saved history of `session_id` that ends at `cursor`,
    /// at most `limit` entries long. `None` when `cursor` names no position
    /// in a legacy session's stored transcript.
    ///
    /// # Errors
    ///
    /// The page could not be read.
    pub fn history_before(
        &mut self,
        session_id: &str,
        cursor: &str,
        limit: usize,
    ) -> Result<Option<HistoryPage>, ClientError> {
        if self.is_unified(session_id) {
            let page = self.entries_page(session_id, &json!(cursor), limit)?;
            return Ok(Some(HistoryPage::Entries(page)));
        }
        let Ok(before) = cursor.parse::<usize>() else {
            return Ok(None);
        };
        let limit = before.min(limit);
        let offset = before.saturating_sub(limit);
        let page = self.stored_page(session_id, offset, limit)?;
        Ok(Some(HistoryPage::Stored(StoredPage { offset, page })))
    }

    /// Every page of the saved history of `session_id`, oldest first, each
    /// at most `limit` entries long.
    ///
    /// # Errors
    ///
    /// A page could not be read.
    pub fn history_pages(
        &mut self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<HistoryPage>, ClientError> {
        let mut pages = Vec::new();
        if self.is_unified(session_id) {
            // Reference `load_more_history`: each page names the one before
            // it, from the latest back.
            let mut cursor = Value::Null;
            loop {
                let page = self.entries_page(session_id, &cursor, limit)?;
                cursor = page.get("nextCursor").cloned().unwrap_or(Value::Null);
                pages.push(HistoryPage::Entries(page));
                if cursor.is_null() {
                    break;
                }
            }
            pages.reverse();
            return Ok(pages);
        }
        let mut offset = 0;
        loop {
            let page = self.stored_page(session_id, offset, limit)?;
            let read = page
                .get("history")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            pages.push(HistoryPage::Stored(StoredPage { offset, page }));
            if read < limit {
                break;
            }
            offset += read;
        }
        Ok(pages)
    }

    fn entries_page(
        &mut self,
        session_id: &str,
        cursor: &Value,
        limit: usize,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        self.public_call(
            "session/history/list",
            json!({"sessionId": session_id, "page": {"cursor": cursor, "limit": limit}}),
        )
    }

    fn stored_page(
        &mut self,
        session_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        self.public_call(
            "internal/history/list",
            json!({"sessionId": session_id, "offset": offset, "limit": limit}),
        )
    }
}

/// The session a reference session method's answer names, reference
/// `state.session.id`.
fn state_session_id(result: &BTreeMap<String, Value>) -> Option<String> {
    result
        .get("state")
        .and_then(|state| state.pointer("/session/id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

/// The session an `internal/session/*` answer's record names.
fn metadata_session_id(result: &BTreeMap<String, Value>) -> Option<String> {
    result
        .get("metadata")
        .and_then(|metadata| metadata.get("session_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}
