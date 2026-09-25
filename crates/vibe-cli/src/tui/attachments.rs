use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::json;
use thiserror::Error;
use vibe_app_server::client::{PreparedImages, PublicContentBlock, TurnRequest};
use vibe_core::images::{ImageDigest, ImageReadError, MAX_IMAGES_PER_MESSAGE, read_image};
use vibe_core::provider::ImageInput;

use vibe_core::path_mentions::{mention_values, resolve_owned_candidate};
pub use vibe_core::path_mentions::{normalize_pasted_text, normalize_typed_text};
pub use vibe_core::path_resources::MentionStats;
use vibe_core::path_resources::{PathResourceKind, build_path_prompt_payload};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDraft {
    text: String,
    transient_images: Vec<TransientImage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransientImage {
    alias: String,
    path: PathBuf,
    digest: ImageDigest,
}

impl PromptDraft {
    #[must_use]
    pub fn text_only(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            transient_images: Vec::new(),
        }
    }

    #[must_use]
    pub(super) fn with_transient_images(
        workspace: &Path,
        text: impl Into<String>,
        tracked_images: &BTreeMap<PathBuf, ImageDigest>,
    ) -> Self {
        let text = text.into();
        let transient_images = mention_values(&text)
            .into_iter()
            .filter_map(|alias| {
                let path = resolve_owned_candidate(workspace, &alias, |path| {
                    tracked_images.contains_key(path)
                })?;
                Some(TransientImage {
                    digest: *tracked_images.get(&path)?,
                    alias,
                    path,
                })
            })
            .fold(Vec::new(), |mut images, image| {
                if !images
                    .iter()
                    .any(|existing: &TransientImage| existing.path == image.path)
                {
                    images.push(image);
                }
                images
            });
        Self {
            text,
            transient_images,
        }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn transient_image_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.transient_images.iter().map(|image| &image.path)
    }

    #[must_use]
    pub fn into_text(self) -> String {
        self.text
    }

    /// Reference `QueueController._server_text`: queued prompts merged into
    /// one turn join with a blank line, and every transient image they hold
    /// stays owned by the merged draft.
    #[must_use]
    pub fn merged<'a>(drafts: impl IntoIterator<Item = &'a Self>) -> Self {
        let mut merged = Self::text_only(String::new());
        for (index, draft) in drafts.into_iter().enumerate() {
            if index > 0 {
                merged.text.push_str(QUEUE_MERGE_SEPARATOR);
            }
            merged.text.push_str(&draft.text);
            for image in &draft.transient_images {
                if !merged
                    .transient_images
                    .iter()
                    .any(|existing| existing.path == image.path)
                {
                    merged.transient_images.push(image.clone());
                }
            }
        }
        merged
    }
}

/// Reference `_MERGE_SEPARATOR` (`vibe/cli/textual_ui/message_queue.py`).
pub const QUEUE_MERGE_SEPARATOR: &str = "\n\n";

/// Reference `QueueController._server_text` and `_server_images`: the prompts
/// queued while busy promote as one turn whose text joins theirs with a blank
/// line and which carries every image they attached, in order. The promoted
/// turn names no mention statistics, as `session/turn/enqueue` carries none;
/// the first prompt's statistics stay on the submission for its telemetry.
pub fn merge_submissions(
    submissions: Vec<PreparedSubmission>,
) -> Result<PreparedSubmission, SubmissionError> {
    let mut texts = Vec::with_capacity(submissions.len());
    let mut attachments = Vec::new();
    let mut images = Vec::new();
    let mut cleanup_paths = Vec::new();
    let mut mention_stats = None;
    for submission in submissions {
        texts.push(submission.turn.prompt);
        attachments.extend(
            submission
                .turn
                .input
                .into_iter()
                .filter(|block| !matches!(block, PublicContentBlock::Text { .. })),
        );
        images.extend(submission.provider_images.as_slice().iter().cloned());
        cleanup_paths.extend(submission.cleanup_paths);
        mention_stats.get_or_insert(submission.mention_stats);
    }
    let prompt = texts.join(QUEUE_MERGE_SEPARATOR);
    let mut input = Vec::with_capacity(attachments.len().saturating_add(1));
    input.push(PublicContentBlock::Text {
        text: prompt.clone(),
    });
    input.extend(attachments);
    Ok(PreparedSubmission {
        turn: TurnRequest {
            idempotency_key: None,
            prompt,
            input,
            injected: false,
            client_user_message_id: None,
            auto_title: None,
            user_display_content: None,
            mention_stats: None,
        },
        provider_images: PreparedImages::try_new(images)
            .map_err(|error| SubmissionError::ProviderImage(error.to_string()))?,
        mention_stats: mention_stats.unwrap_or_default(),
        cleanup_paths,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSubmission {
    pub turn: TurnRequest,
    pub provider_images: PreparedImages,
    pub mention_stats: MentionStats,
    pub cleanup_paths: Vec<PathBuf>,
}

pub fn prepare_submission(
    workspace: &Path,
    draft: &PromptDraft,
    active_model: &str,
    supports_images: bool,
) -> Result<PreparedSubmission, SubmissionError> {
    let canonical_workspace = fs::canonicalize(workspace)
        .map_err(|error| SubmissionError::Workspace(error.to_string()))?;
    let payload = build_path_prompt_payload(&canonical_workspace, draft.text());
    let mention_stats = payload.mention_stats();
    let images = payload
        .resources
        .iter()
        .filter(|resource| resource.kind == PathResourceKind::Image)
        .collect::<Vec<_>>();
    if !images.is_empty() && !supports_images {
        return Err(SubmissionError::ImagesUnsupported {
            model: active_model.to_owned(),
        });
    }
    if images.len() > MAX_IMAGES_PER_MESSAGE {
        return Err(SubmissionError::TooManyImages {
            actual: images.len(),
            maximum: MAX_IMAGES_PER_MESSAGE,
        });
    }
    for transient in &draft.transient_images {
        if !images
            .iter()
            .any(|resource| resource.path == transient.path)
        {
            return Err(SubmissionError::ImageChanged {
                alias: transient.alias.clone(),
            });
        }
    }

    let mut input = Vec::with_capacity(images.len().saturating_add(1));
    input.push(PublicContentBlock::Text {
        text: draft.text().to_owned(),
    });
    let mut provider_images = Vec::with_capacity(images.len());
    let mut cleanup_paths = Vec::new();
    for resource in images {
        let image =
            read_image(&resource.path).map_err(|error| attachment_error(&resource.alias, error))?;
        let transient = draft
            .transient_images
            .iter()
            .find(|transient| transient.path == resource.path);
        if let Some(transient) = transient {
            if image.digest != transient.digest {
                return Err(SubmissionError::ImageChanged {
                    alias: transient.alias.clone(),
                });
            }
            cleanup_paths.push(resource.path.clone());
        }
        let media_type = image.format.media_type();
        let encoded = BASE64_STANDARD.encode(&image.bytes);
        provider_images.push(ImageInput {
            media_type: media_type.to_owned(),
            data: encoded.clone(),
        });
        // Reference `snapshot_image` (`vibe/core/session/image_snapshot.py`)
        // keeps the bytes inline when there is no session directory; with
        // one, the server writes the same bytes under the session's
        // `attachments` and the entry points there, as `snapshot_attachments`
        // does here.
        input.push(PublicContentBlock::Image {
            attachment: json!({
                "source": {"kind": "inline", "data": encoded},
                "alias": resource.alias,
                "mimeType": media_type,
            }),
        });
    }

    let turn = TurnRequest {
        idempotency_key: None,
        prompt: draft.text().to_owned(),
        input,
        injected: false,
        client_user_message_id: None,
        auto_title: None,
        user_display_content: None,
        mention_stats: Some(serde_json::to_value(&mention_stats).map_err(SubmissionError::Json)?),
    };
    let provider_images = PreparedImages::try_new(provider_images)
        .map_err(|error| SubmissionError::ProviderImage(error.to_string()))?;
    Ok(PreparedSubmission {
        turn,
        provider_images,
        mention_stats,
        cleanup_paths,
    })
}

fn attachment_error(alias: &str, error: ImageReadError) -> SubmissionError {
    let reason = match error {
        ImageReadError::Unsupported(path) => format!(
            "Unsupported image extension: {}",
            path.extension()
                .map_or_else(String::new, |extension| format!(
                    ".{}",
                    extension.to_string_lossy()
                ))
        ),
        ImageReadError::NotFile(path) => format!("Not a file: {}", path.display()),
        ImageReadError::TooLarge {
            actual, maximum, ..
        } => format!("Image is too large: {actual} > {maximum}"),
        ImageReadError::Io { path, source } => {
            format!("Failed to read image {}: {source}", path.display())
        }
    };
    SubmissionError::ImageAttachment {
        alias: alias.to_owned(),
        reason,
    }
}

#[derive(Debug, Error)]
pub enum SubmissionError {
    #[error("workspace is unavailable: {0}")]
    Workspace(String),
    #[error(
        "Model `{model}` does not support images. Switch with /model or remove the attachment."
    )]
    ImagesUnsupported { model: String },
    #[error("Too many image attachments (got {actual}, max {maximum}).")]
    TooManyImages { actual: usize, maximum: usize },
    #[error("Failed to attach image {alias}: {reason}")]
    ImageAttachment { alias: String, reason: String },
    #[error("Failed to attach image {alias}: Image changed before it could be read")]
    ImageChanged { alias: String },
    #[error("mention statistics could not be encoded: {0}")]
    Json(serde_json::Error),
    #[error("provider image preparation failed: {0}")]
    ProviderImage(String),
}

#[cfg(test)]
#[path = "attachments/tests.rs"]
mod tests;
