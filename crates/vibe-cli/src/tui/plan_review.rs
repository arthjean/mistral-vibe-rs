use std::path::PathBuf;
use std::time::Duration;

use tokio::time::Instant;

use super::state::{PlanReviewState, TuiState};

const REFRESH_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Default)]
pub struct PlanReviewMonitor {
    path: Option<PathBuf>,
    refreshed_at: Option<Instant>,
}

impl PlanReviewMonitor {
    pub async fn sync(&mut self, path: Option<PathBuf>, state: &mut TuiState) {
        let path_changed = self.path != path;
        if path_changed {
            self.path = path.clone();
            self.refreshed_at = None;
        }
        let Some(path) = path else {
            state.plan_review = None;
            return;
        };
        if !path_changed
            && self
                .refreshed_at
                .is_some_and(|refreshed| refreshed.elapsed() < REFRESH_INTERVAL)
        {
            return;
        }
        self.refreshed_at = Some(Instant::now());
        state.plan_review = Some(match tokio::fs::read_to_string(&path).await {
            Ok(content) => PlanReviewState {
                path,
                content,
                error: None,
            },
            Err(error) => PlanReviewState {
                path,
                content: String::new(),
                error: Some(error.to_string()),
            },
        });
    }
}
