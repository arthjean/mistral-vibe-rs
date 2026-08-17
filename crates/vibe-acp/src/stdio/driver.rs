//! A turn driver built on first use.
//!
//! An editor opens sessions before any credential is available, so building
//! the live driver eagerly would refuse work the adapter can still serve. The
//! driver is therefore built on the first turn that needs it, and a build that
//! fails is retried by the next one.

use std::sync::{Arc, Mutex, OnceLock};

use vibe_app_server::client::{
    CompactionDriverFuture, DriverError, DriverFuture, EventObserver, TurnDriver, TurnReservation,
};

pub(crate) struct DeferredTurnDriver<D> {
    driver: OnceLock<Arc<D>>,
    initialize: Mutex<()>,
    factory: Box<dyn Fn() -> Result<D, DriverError> + Send + Sync>,
}

impl<D> DeferredTurnDriver<D> {
    pub(crate) fn new(
        factory: impl Fn() -> Result<D, DriverError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            driver: OnceLock::new(),
            initialize: Mutex::new(()),
            factory: Box::new(factory),
        }
    }

    /// The built driver, building it if this is the first call. The mutex is
    /// what [`OnceLock`] cannot express on its own: a fallible initialization
    /// that stores nothing when it fails, so a later call can try again.
    fn resolve(&self) -> Result<&Arc<D>, DriverError> {
        if let Some(driver) = self.driver.get() {
            return Ok(driver);
        }
        let _guard = self
            .initialize
            .lock()
            .map_err(|_| DriverError::StatePoisoned)?;
        if self.driver.get().is_none() {
            let driver = Arc::new((self.factory)()?);
            self.driver
                .set(driver)
                .map_err(|_| DriverError::StatePoisoned)?;
        }
        self.driver.get().ok_or(DriverError::StatePoisoned)
    }
}

impl<D> TurnDriver for DeferredTurnDriver<D>
where
    D: TurnDriver + 'static,
{
    fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
        let driver = self.resolve().cloned();
        Box::pin(async move { driver?.run(reservation).await })
    }

    fn run_observed<'a>(
        &'a self,
        reservation: &'a TurnReservation,
        observer: Arc<dyn EventObserver>,
    ) -> DriverFuture<'a> {
        let driver = self.resolve().cloned();
        Box::pin(async move { driver?.run_observed(reservation, observer).await })
    }

    fn interrupt(&self, session_id: &str, turn_id: &str) -> Result<(), DriverError> {
        self.resolve()?.interrupt(session_id, turn_id)
    }

    fn steer(
        &self,
        session_id: &str,
        turn_id: &str,
        content: &str,
        inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        self.resolve()?
            .steer(session_id, turn_id, content, inject_invoked_skill)
    }

    fn inject_context(
        &self,
        session_id: &str,
        content: &str,
        as_message: bool,
        inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        self.resolve()?
            .inject_context(session_id, content, as_message, inject_invoked_skill)
    }

    fn resolve_callback(
        &self,
        session_id: &str,
        turn_id: &str,
        callback_id: &str,
        accepted: bool,
        value: Option<&str>,
    ) -> Result<(), DriverError> {
        self.resolve()?
            .resolve_callback(session_id, turn_id, callback_id, accepted, value)
    }

    fn compact<'a>(
        &'a self,
        session_id: &'a str,
        extra_instructions: &'a str,
    ) -> CompactionDriverFuture<'a> {
        let driver = self.resolve().cloned();
        Box::pin(async move { driver?.compact(session_id, extra_instructions).await })
    }
}
