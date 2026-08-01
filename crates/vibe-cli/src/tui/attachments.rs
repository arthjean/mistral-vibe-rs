use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::json;
use thiserror::Error;
use vibe_app_server::client::{PublicContentBlock, TurnRequest};
use vibe_core::images::{ImageDigest, ImageReadError, MAX_IMAGES_PER_MESSAGE, read_image};

pub use super::path_mentions::normalize_pasted_text;
use super::path_mentions::{mention_values, resolve_owned_candidate};
pub use super::path_resources::MentionStats;
use super::path_resources::{PathResourceKind, build_path_prompt_payload};

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
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedSubmission {
    pub turn: TurnRequest,
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
        let source = transient.map_or_else(
            || {
                json!({
                    "kind": "file",
                    "path": resource.path,
                })
            },
            |_| {
                json!({
                    "kind": "inline",
                    "data": BASE64_STANDARD.encode(&image.bytes),
                })
            },
        );
        input.push(PublicContentBlock::Image {
            attachment: json!({
                "source": source,
                "alias": resource.alias,
                "mimeType": image.format.media_type(),
            }),
        });
    }

    let turn = TurnRequest {
        prompt: draft.text().to_owned(),
        input,
        client_user_message_id: None,
        auto_title: None,
        user_display_content: None,
        mention_stats: Some(serde_json::to_value(&mention_stats).map_err(SubmissionError::Json)?),
    };
    Ok(PreparedSubmission {
        turn,
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
}

#[cfg(test)]
#[path = "attachments/tests.rs"]
mod tests;
