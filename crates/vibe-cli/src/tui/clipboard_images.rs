use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use tokio::task::JoinSet;
use vibe_core::images::ImageDigest;

use super::attachments::PromptDraft;
use super::chat_input::{ChatInputState, InputEffect};
use super::clipboard::{
    CapturedClipboardImage, ClipboardError, SystemClipboard, capture_clipboard_image,
};
use super::push_local_notice;
use super::state::{EntryStatus, TuiState};

#[derive(Debug, Clone, Copy)]
pub(super) struct ImageModel<'a> {
    pub alias: &'a str,
    pub supports_images: bool,
}

#[derive(Debug, Clone, Default)]
pub(super) struct ImageModels {
    support_by_alias: BTreeMap<String, bool>,
}

impl ImageModels {
    pub fn insert(&mut self, alias: impl Into<String>, supports_images: bool) {
        self.support_by_alias.insert(alias.into(), supports_images);
    }

    #[must_use]
    pub fn get<'a>(&self, alias: &'a str) -> ImageModel<'a> {
        ImageModel {
            alias,
            supports_images: self.support_by_alias.get(alias).copied().unwrap_or(false),
        }
    }
}

pub(super) enum ClipboardImageCompletion {
    Finished {
        notify_when_empty: bool,
        result: Result<Option<CapturedClipboardImage>, ClipboardError>,
    },
    TaskFailed(String),
}

#[derive(Default)]
pub(super) struct ClipboardImageManager {
    tasks: JoinSet<ClipboardImageCompletion>,
    files: BTreeMap<PathBuf, ImageDigest>,
}

impl ClipboardImageManager {
    pub fn schedule_effects(&mut self, effects: &[InputEffect]) {
        for notify_when_empty in effects.iter().filter_map(|effect| match effect {
            InputEffect::ClipboardImageRequested { notify_when_empty } => Some(*notify_when_empty),
            _ => None,
        }) {
            self.schedule(notify_when_empty);
        }
    }

    pub fn schedule(&mut self, notify_when_empty: bool) {
        self.schedule_with(notify_when_empty, || {
            capture_clipboard_image(&SystemClipboard)
        });
    }

    fn schedule_with(
        &mut self,
        notify_when_empty: bool,
        capture: impl FnOnce() -> Result<Option<CapturedClipboardImage>, ClipboardError>
        + Send
        + 'static,
    ) {
        self.tasks
            .spawn_blocking(move || ClipboardImageCompletion::Finished {
                notify_when_empty,
                result: capture(),
            });
    }

    #[must_use]
    pub fn has_pending_capture(&self) -> bool {
        !self.tasks.is_empty()
    }

    pub async fn next_completion(&mut self) -> Option<ClipboardImageCompletion> {
        match self.tasks.join_next().await {
            Some(Ok(completion)) => Some(completion),
            Some(Err(error)) => Some(ClipboardImageCompletion::TaskFailed(error.to_string())),
            None => None,
        }
    }

    pub async fn apply_completion(
        &mut self,
        completion: ClipboardImageCompletion,
        model: Option<ImageModel<'_>>,
        input: &mut ChatInputState,
        state: &mut TuiState,
    ) {
        match completion {
            ClipboardImageCompletion::Finished {
                notify_when_empty,
                result,
            } => {
                self.apply_capture(result, notify_when_empty, model, input, state)
                    .await;
            }
            ClipboardImageCompletion::TaskFailed(error) => {
                state.push_diagnostic(format!("Clipboard image capture task failed: {error}"));
            }
        }
    }

    async fn apply_capture(
        &mut self,
        result: Result<Option<CapturedClipboardImage>, ClipboardError>,
        notify_when_empty: bool,
        model: Option<ImageModel<'_>>,
        input: &mut ChatInputState,
        state: &mut TuiState,
    ) {
        match result {
            Ok(Some(captured)) if model.is_none_or(|model| !model.supports_images) => {
                if let Err(error) = remove_file(&captured.path).await {
                    self.files.insert(captured.path, captured.digest);
                    state
                        .push_diagnostic(format!("Unused clipboard image cleanup failed: {error}"));
                }
                state.push_diagnostic(image_model_warning(model));
            }
            Ok(Some(captured)) => {
                let name = captured
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("clipboard image")
                    .to_owned();
                if !input.insert_image_mention(&captured.path) {
                    if let Err(error) = remove_file(&captured.path).await {
                        self.files.insert(captured.path, captured.digest);
                        state.push_diagnostic(format!(
                            "Unused clipboard image cleanup failed: {error}"
                        ));
                    }
                    state.push_diagnostic("Failed to paste image into prompt.");
                    return;
                }
                self.files.insert(captured.path, captured.digest);
                push_local_notice(
                    state,
                    &format!(
                        "Image pasted as {name} ({})",
                        natural_binary_size(captured.bytes)
                    ),
                    EntryStatus::Completed,
                );
            }
            Ok(None) if notify_when_empty => {
                state.push_diagnostic("No image found on the clipboard.");
            }
            Ok(None) | Err(ClipboardError::ImageUnsupported) => {}
            Err(ClipboardError::ImageTooLarge { actual, maximum }) => {
                state.push_diagnostic(format!(
                    "Clipboard image is {}; max is {}.",
                    natural_binary_size(actual),
                    natural_binary_size(maximum)
                ))
            }
            Err(ClipboardError::Save(_)) => {
                state.push_diagnostic("Failed to save pasted image to disk.");
            }
            Err(ClipboardError::Timeout) if notify_when_empty => {
                state.push_diagnostic("No image found on the clipboard.");
            }
            Err(ClipboardError::Timeout) => {}
            Err(error) => state.push_diagnostic(error.to_string()),
        }
    }

    #[must_use]
    pub fn draft(&self, workspace: &Path, text: impl Into<String>) -> PromptDraft {
        PromptDraft::with_transient_images(workspace, text, &self.files)
    }

    pub async fn consume(
        &mut self,
        paths: &[PathBuf],
        protected: &HashSet<PathBuf>,
        state: &mut TuiState,
    ) {
        for path in paths.iter().filter(|path| !protected.contains(*path)) {
            self.remove_tracked(path, state).await;
        }
    }

    pub async fn discard_unreferenced(
        &mut self,
        protected: &HashSet<PathBuf>,
        state: &mut TuiState,
    ) {
        let discarded = self
            .files
            .keys()
            .filter(|path| !protected.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        for path in discarded {
            self.remove_tracked(&path, state).await;
        }
    }

    async fn remove_tracked(&mut self, path: &Path, state: &mut TuiState) {
        match remove_file(path).await {
            Ok(()) => {
                self.files.remove(path);
            }
            Err(error) => state.push_diagnostic(format!(
                "Clipboard image cleanup failed for `{}`: {error}",
                path.display()
            )),
        }
    }

    pub async fn shutdown(&mut self) -> Result<(), String> {
        let mut cleanup_error = None;
        while let Some(completion) = self.next_completion().await {
            if let ClipboardImageCompletion::Finished {
                result: Ok(Some(captured)),
                ..
            } = completion
                && let Err(error) = remove_file(&captured.path).await
                && cleanup_error.is_none()
            {
                cleanup_error = Some(format!(
                    "clipboard image cleanup failed for `{}`: {error}",
                    captured.path.display()
                ));
            }
        }
        for path in std::mem::take(&mut self.files).into_keys() {
            if let Err(error) = remove_file(&path).await
                && cleanup_error.is_none()
            {
                cleanup_error = Some(format!(
                    "clipboard image cleanup failed for `{}`: {error}",
                    path.display()
                ));
            }
        }
        cleanup_error.map_or(Ok(()), Err)
    }

    #[cfg(test)]
    fn tracked_paths(&self) -> Vec<PathBuf> {
        self.files.keys().cloned().collect()
    }
}

async fn remove_file(path: &Path) -> Result<(), std::io::Error> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn image_model_warning(model: Option<ImageModel<'_>>) -> String {
    model.map_or_else(
        || "The active model does not support images.".to_owned(),
        |model| {
            format!(
                "Model `{}` does not support images. Switch with /model or ask me to enable image support for this model.",
                model.alias
            )
        },
    )
}

fn natural_binary_size(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} {}", if bytes == 1 { "Byte" } else { "Bytes" });
    }
    let mut value = bytes as f64;
    let mut unit = UNITS[0];
    for candidate in UNITS {
        value /= 1024.0;
        unit = candidate;
        if value < 1024.0 || candidate == UNITS[UNITS.len() - 1] {
            break;
        }
    }
    format!("{value:.1} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured(path: PathBuf) -> CapturedClipboardImage {
        CapturedClipboardImage {
            path,
            bytes: 5,
            digest: ImageDigest::of(b"image"),
        }
    }

    #[tokio::test]
    async fn completion_inserts_an_owned_absolute_token() {
        let temporary = tempfile::tempdir().expect("isolated clipboard fixture");
        let image = temporary.path().join("clipboard image.png");
        std::fs::write(&image, b"image").expect("clipboard fixture");
        let mut manager = ClipboardImageManager::default();
        let mut input = ChatInputState::new();
        input.replace_text("inspect");
        let mut state = TuiState::new("session");

        manager
            .apply_capture(
                Ok(Some(captured(image.clone()))),
                true,
                Some(ImageModel {
                    alias: "test-model",
                    supports_images: true,
                }),
                &mut input,
                &mut state,
            )
            .await;

        assert_eq!(
            input.editor().text(),
            format!("inspect @'{}' ", image.to_string_lossy())
        );
        assert_eq!(manager.tracked_paths(), [image]);
        assert!(state.entries.last().is_some_and(|entry| {
            entry
                .text
                .starts_with("Image pasted as clipboard image.png")
        }));
    }

    #[tokio::test]
    async fn empty_feedback_distinguishes_implicit_and_explicit_requests() {
        let mut manager = ClipboardImageManager::default();
        let mut input = ChatInputState::new();
        let mut state = TuiState::new("session");
        manager
            .apply_capture(Ok(None), false, None, &mut input, &mut state)
            .await;
        assert_eq!(state.diagnostics().count(), 0);

        manager
            .apply_capture(Ok(None), true, None, &mut input, &mut state)
            .await;
        assert_eq!(
            state.diagnostics().collect::<Vec<_>>(),
            ["No image found on the clipboard."]
        );
    }

    #[tokio::test]
    async fn insertion_failure_removes_the_unused_image_and_warns() {
        let temporary = tempfile::tempdir().expect("isolated clipboard fixture");
        let image = temporary.path().join("clipboard.png");
        std::fs::write(&image, b"image").expect("clipboard fixture");
        let mut manager = ClipboardImageManager::default();
        let mut input = ChatInputState::new();
        input.set_secret_input(true);
        let mut state = TuiState::new("session");

        manager
            .apply_capture(
                Ok(Some(captured(image.clone()))),
                true,
                Some(ImageModel {
                    alias: "test-model",
                    supports_images: true,
                }),
                &mut input,
                &mut state,
            )
            .await;

        assert!(!image.exists());
        assert!(manager.tracked_paths().is_empty());
        assert_eq!(
            state.diagnostics().collect::<Vec<_>>(),
            ["Failed to paste image into prompt."]
        );
    }

    #[tokio::test]
    async fn shutdown_waits_for_capture_and_removes_unconsumed_file() {
        let temporary = tempfile::tempdir().expect("isolated clipboard fixture");
        let image = temporary.path().join("clipboard.png");
        let captured_path = image.clone();
        let mut manager = ClipboardImageManager::default();
        manager.schedule_with(false, move || {
            std::fs::write(&captured_path, b"image")
                .map_err(|error| ClipboardError::Operation(error.to_string()))?;
            Ok(Some(captured(captured_path)))
        });

        manager.shutdown().await.expect("capture cleanup");

        assert!(!image.exists());
        assert!(!manager.has_pending_capture());
    }

    #[tokio::test]
    async fn submission_discards_images_removed_from_the_prompt() {
        let temporary = tempfile::tempdir().expect("isolated clipboard fixture");
        let discarded = temporary.path().join("discarded.png");
        let retained = temporary.path().join("retained.png");
        std::fs::write(&discarded, b"image").expect("discarded fixture");
        std::fs::write(&retained, b"image").expect("retained fixture");
        let mut manager = ClipboardImageManager::default();
        manager.files.extend([
            (discarded.clone(), ImageDigest::of(b"image")),
            (retained.clone(), ImageDigest::of(b"image")),
        ]);
        let protected = HashSet::from([retained.clone()]);
        let mut state = TuiState::new("session");

        manager.discard_unreferenced(&protected, &mut state).await;

        assert!(!discarded.exists());
        assert!(retained.exists());
        assert_eq!(manager.tracked_paths(), [retained]);
    }
}
