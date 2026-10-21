//! Background titles of a unified session.
//!
//! Reference `_maybe_schedule_title_generation` and `_generate_title`
//! (`harness/runtimes/python/python/mistralai_vibe_local_harness/vibe/_session.py`):
//! every state the session publishes ticks the cadence, at most one title
//! runs at a time, and a title that lands renames the session as `auto`
//! unless it was renamed meanwhile.

use super::*;
use vibe_harness::state::SessionStatus;
use vibe_harness::title::{TitleCadence, TitleRoute, count_model_steps, latest_compaction_id};

/// What a session keeps of its titles.
#[derive(Default)]
pub(crate) struct TitleWork {
    /// Where titles are asked for; `None` in a session that is not titled.
    pub(crate) route: Option<Arc<TitleRoute>>,
    cadence: TitleCadence,
    running: bool,
}

impl UnifiedSession {
    /// Ticks the cadence on the state `publication` carries, starting a title
    /// when one is due.
    pub(crate) fn tick_title(self: &Arc<Self>, publication: &Publication) {
        let turn_completing = publication.state.session.status == SessionStatus::Idle;
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let job = {
            let mut titles = lock(&self.titles);
            let Some(route) = titles.route.clone() else {
                return;
            };
            if titles.running {
                return;
            }
            // The sink publishes with the core locked, so the state is read
            // from the publication.
            let state = &publication.state;
            let entries = state.entries.clone();
            let title = state.session.title.clone();
            if !titles.cadence.begin_if_due(
                count_model_steps(&entries),
                title.as_deref(),
                latest_compaction_id(&entries).as_deref(),
                turn_completing,
            ) {
                return;
            }
            titles.running = true;
            let message_id = state
                .latest_turn
                .as_ref()
                .and_then(|turn| turn.input_entry_id.clone());
            (route, entries, title, message_id)
        };
        let session = Arc::clone(self);
        handle.spawn(async move {
            let (route, entries, previous, message_id) = job;
            let title = route
                .generate(
                    &session.id,
                    &entries,
                    previous.as_deref(),
                    message_id.as_deref(),
                )
                .await;
            if let Some(title) = title {
                session.land_title(&title);
            }
            lock(&session.titles).running = false;
        });
    }

    /// Reference `_generate_title` once the model answered.
    fn land_title(&self, title: &str) {
        let publication = {
            let mut titles = lock(&self.titles);
            let mut core = self.core();
            if titles
                .cadence
                .is_manual(core.projector.state.session.title.as_deref())
            {
                return;
            }
            titles.cadence.record(title);
            let advance = core.projector.rename(title, "auto", now_millis());
            core.publication(advance, None)
        };
        let frames = self.publish(&publication);
        self.send(frames);
    }
}
