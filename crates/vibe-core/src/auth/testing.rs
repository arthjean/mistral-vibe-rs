//! A scripted [`KeyringBackend`] recording every primitive call in order.
//!
//! The counterpart of the capture script's `_KeyringScript`: the corpus
//! replay and the unit tests drive the store over the same scripted stores,
//! errors and call journal the reference was measured with.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::keyring::{KeyringBackend, KeyringFailure};

/// The failure kinds the capture scripts inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScriptedError {
    /// `KeyringError`: the store exists and the operation failed.
    Backend,
    /// `NoKeyringError`: no store at all.
    NoBackend,
}

impl ScriptedError {
    fn failure(self) -> KeyringFailure {
        match self {
            Self::Backend => KeyringFailure::Backend("scripted failure".to_owned()),
            Self::NoBackend => KeyringFailure::NoBackend,
        }
    }
}

#[derive(Default)]
pub(crate) struct ScriptedBackend {
    stored: Mutex<BTreeMap<String, String>>,
    set_error: Option<ScriptedError>,
    get_error: Option<ScriptedError>,
    delete_errors: BTreeMap<String, ScriptedError>,
    calls: Mutex<Vec<String>>,
}

/// One scripted backend: the seeded `service -> secret` entries, the error
/// every `set` or `get` answers, and the error `delete` answers per service.
pub(crate) fn scripted(
    stored: &[(&str, &str)],
    set_error: Option<ScriptedError>,
    get_error: Option<ScriptedError>,
    delete_errors: &[(&str, ScriptedError)],
) -> Arc<ScriptedBackend> {
    Arc::new(ScriptedBackend {
        stored: Mutex::new(
            stored
                .iter()
                .map(|(service, secret)| ((*service).to_owned(), (*secret).to_owned()))
                .collect(),
        ),
        set_error,
        get_error,
        delete_errors: delete_errors
            .iter()
            .map(|(service, error)| ((*service).to_owned(), *error))
            .collect(),
        calls: Mutex::new(Vec::new()),
    })
}

impl ScriptedBackend {
    /// The stored entries, sorted by service as the corpus records them.
    pub(crate) fn stored(&self) -> BTreeMap<String, String> {
        self.stored.lock().expect("scripted store").clone()
    }

    /// The ordered primitive calls, `op:service` as the corpus records them.
    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("scripted calls").clone()
    }

    fn record(&self, operation: &str, service: &str) {
        self.calls
            .lock()
            .expect("scripted calls")
            .push(format!("{operation}:{service}"));
    }
}

impl KeyringBackend for Arc<ScriptedBackend> {
    fn get(&self, service: &str, _account: &str) -> Result<Option<String>, KeyringFailure> {
        self.record("get", service);
        if let Some(error) = self.get_error {
            return Err(error.failure());
        }
        Ok(self
            .stored
            .lock()
            .expect("scripted store")
            .get(service)
            .cloned())
    }

    fn set(&self, service: &str, _account: &str, secret: &str) -> Result<(), KeyringFailure> {
        self.record("set", service);
        if let Some(error) = self.set_error {
            return Err(error.failure());
        }
        self.stored
            .lock()
            .expect("scripted store")
            .insert(service.to_owned(), secret.to_owned());
        Ok(())
    }

    fn delete(&self, service: &str, _account: &str) -> Result<(), KeyringFailure> {
        self.record("delete", service);
        if let Some(error) = self.delete_errors.get(service) {
            return Err(error.failure());
        }
        if self
            .stored
            .lock()
            .expect("scripted store")
            .remove(service)
            .is_none()
        {
            return Err(KeyringFailure::NoEntry);
        }
        Ok(())
    }
}
