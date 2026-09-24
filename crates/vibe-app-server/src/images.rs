use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use vibe_core::images::{ImageFormat, MAX_IMAGE_BYTES, read_image, validate_image_size};
use vibe_core::provider::ImageInput;

use crate::client::{DriverError, PublicContentBlock, TurnRequest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImages(Vec<ImageInput>);

impl PreparedImages {
    pub fn try_new(images: Vec<ImageInput>) -> Result<Self, DriverError> {
        for image in &images {
            validate_inline_image(&image.media_type, &image.data)?;
        }
        Ok(Self(images))
    }

    #[must_use]
    pub fn as_slice(&self) -> &[ImageInput] {
        &self.0
    }
}

pub(crate) fn validate_prepared_images(
    turn: &TurnRequest,
    images: &PreparedImages,
) -> Result<(), DriverError> {
    let images = images.as_slice();
    let attachments = turn
        .input
        .iter()
        .filter_map(|block| match block {
            PublicContentBlock::Image { attachment } => Some(attachment),
            _ => None,
        })
        .collect::<Vec<_>>();
    if attachments.len() != images.len() {
        return Err(DriverError::ImageAttachment(format!(
            "prepared {} provider images for {} public attachments",
            images.len(),
            attachments.len()
        )));
    }
    for (attachment, image) in attachments.into_iter().zip(images) {
        let media_type = attachment_media_type(attachment)?;
        if media_type != image.media_type {
            return Err(DriverError::ImageAttachment(format!(
                "prepared image MIME type `{}` does not match `{media_type}`",
                image.media_type
            )));
        }
    }
    Ok(())
}

pub(crate) async fn provider_images(
    input: &[PublicContentBlock],
) -> Result<PreparedImages, DriverError> {
    let mut images = Vec::new();
    for block in input {
        if let PublicContentBlock::Image { attachment } = block {
            images.push(provider_image_input(attachment).await?);
        }
    }
    Ok(PreparedImages(images))
}

async fn provider_image_input(attachment: &Value) -> Result<ImageInput, DriverError> {
    let media_type = attachment_media_type(attachment)?;
    let source = attachment.get("source");
    let data = if let Some(source) = source {
        match source.get("kind").and_then(Value::as_str) {
            Some("file") => encode_file_image(source, media_type).await?,
            Some("inline") => validated_inline_image(
                media_type,
                source.get("data").and_then(Value::as_str).ok_or_else(|| {
                    DriverError::ImageAttachment("missing inline image data".to_owned())
                })?,
            )?,
            Some(kind) => {
                return Err(DriverError::ImageAttachment(format!(
                    "unsupported image source kind `{kind}`"
                )));
            }
            None => {
                return Err(DriverError::ImageAttachment(
                    "missing image source kind".to_owned(),
                ));
            }
        }
    } else {
        validated_inline_image(
            media_type,
            attachment
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| DriverError::ImageAttachment("missing image source".to_owned()))?,
        )?
    };
    Ok(ImageInput {
        media_type: media_type.to_owned(),
        data,
    })
}

async fn encode_file_image(source: &Value, media_type: &str) -> Result<String, DriverError> {
    let path = source
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| DriverError::ImageAttachment("missing image file path".to_owned()))?;
    let path = PathBuf::from(path);
    let image = tokio::task::spawn_blocking(move || read_image(&path))
        .await
        .map_err(|error| DriverError::ImageAttachment(error.to_string()))?
        .map_err(|error| DriverError::ImageAttachment(error.to_string()))?;
    let actual_media_type = image.format.media_type();
    if media_type != actual_media_type {
        return Err(DriverError::ImageAttachment(format!(
            "image MIME type `{media_type}` does not match `{actual_media_type}`"
        )));
    }
    Ok(BASE64_STANDARD.encode(image.bytes))
}

/// The non-text blocks of a turn's input as its user entry keeps them.
///
/// An inline image is written once under the session's `attachments`
/// directory, named by the SHA-1 of its bytes, and the entry points at that
/// file instead of carrying the bytes. Without a session directory the image
/// stays inline. Reference `snapshot_image_bytes`
/// (`vibe/core/session/image_snapshot.py`).
pub(crate) async fn snapshot_attachments(
    input: &[PublicContentBlock],
    session_dir: Option<&Path>,
) -> Result<Vec<PublicContentBlock>, DriverError> {
    let mut attachments = Vec::new();
    for block in input {
        match block {
            PublicContentBlock::Text { .. } => {}
            PublicContentBlock::Image { attachment } => {
                let snapshot = match session_dir {
                    Some(session_dir) => snapshot_inline_image(attachment, session_dir).await?,
                    None => None,
                };
                attachments.push(PublicContentBlock::Image {
                    attachment: snapshot.unwrap_or_else(|| attachment.clone()),
                });
            }
            PublicContentBlock::Resource { .. } => attachments.push(block.clone()),
        }
    }
    Ok(attachments)
}

async fn snapshot_inline_image(
    attachment: &Value,
    session_dir: &Path,
) -> Result<Option<Value>, DriverError> {
    let Some(source) = attachment.get("source") else {
        return Ok(None);
    };
    if source.get("kind").and_then(Value::as_str) != Some("inline") {
        return Ok(None);
    }
    let media_type = attachment_media_type(attachment)?;
    let extension = match media_type {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        other => {
            return Err(DriverError::ImageAttachment(format!(
                "unsupported image MIME type `{other}`"
            )));
        }
    };
    let bytes = BASE64_STANDARD
        .decode(
            source
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
        .map_err(|error| DriverError::ImageAttachment(error.to_string()))?;
    validate_image_size(bytes.len())
        .map_err(|error| DriverError::ImageAttachment(error.to_string()))?;
    let digest = Sha1::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let directory = session_dir.join("attachments");
    let path = directory.join(format!("{digest}{extension}"));
    let io_error = |error: std::io::Error| DriverError::ImageAttachment(error.to_string());
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(io_error)?;
    if !tokio::fs::try_exists(&path).await.map_err(io_error)? {
        tokio::fs::write(&path, &bytes).await.map_err(io_error)?;
    }
    let path = tokio::fs::canonicalize(&path).await.map_err(io_error)?;
    let mut snapshot = attachment.clone();
    snapshot["source"] = json!({"kind": "file", "path": path.to_string_lossy()});
    Ok(Some(snapshot))
}

fn attachment_media_type(attachment: &Value) -> Result<&str, DriverError> {
    attachment
        .get("mimeType")
        .or_else(|| attachment.get("mediaType"))
        .and_then(Value::as_str)
        .ok_or_else(|| DriverError::ImageAttachment("missing image MIME type".to_owned()))
}

fn validated_inline_image(media_type: &str, data: &str) -> Result<String, DriverError> {
    validate_inline_image(media_type, data)?;
    Ok(data.to_owned())
}

fn validate_inline_image(media_type: &str, data: &str) -> Result<(), DriverError> {
    if ImageFormat::from_media_type(media_type).is_none() {
        return Err(DriverError::ImageAttachment(format!(
            "unsupported image MIME type `{media_type}`"
        )));
    }
    let encoded_limit = usize::try_from(MAX_IMAGE_BYTES)
        .unwrap_or(usize::MAX)
        .saturating_add(2)
        / 3
        * 4;
    if data.len() > encoded_limit {
        return Err(DriverError::ImageAttachment(
            "inline image exceeds the encoded size limit".to_owned(),
        ));
    }
    let bytes = BASE64_STANDARD
        .decode(data)
        .map_err(|error| DriverError::ImageAttachment(error.to_string()))?;
    validate_image_size(bytes.len())
        .map_err(|error| DriverError::ImageAttachment(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn canonical_inline_and_legacy_attachments_prepare() {
        for attachment in [
            json!({
                "source": {"kind": "inline", "data": "aW1hZ2U="},
                "alias": "image.png",
                "mimeType": "image/png",
            }),
            json!({"data": "aW1hZ2U=", "mediaType": "image/png"}),
        ] {
            let image = provider_image_input(&attachment)
                .await
                .expect("valid inline image");
            assert_eq!(image.media_type, "image/png");
            assert_eq!(image.data, "aW1hZ2U=");
        }
    }

    #[tokio::test]
    async fn canonical_file_attachment_prepares() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("image.png");
        std::fs::write(&path, b"image").expect("image fixture");
        let image = provider_image_input(&json!({
            "source": {"kind": "file", "path": path},
            "alias": "image.png",
            "mimeType": "image/png",
        }))
        .await
        .expect("canonical file image input");

        assert_eq!(image.media_type, "image/png");
        assert_eq!(
            BASE64_STANDARD
                .decode(image.data)
                .expect("base64 provider data"),
            b"image"
        );
    }

    #[tokio::test]
    async fn changed_files_fail_closed() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("image.png");
        std::fs::write(&path, b"image").expect("image fixture");
        let attachment = json!({
            "source": {"kind": "file", "path": path},
            "alias": "image.png",
            "mimeType": "image/png",
        });

        std::fs::remove_file(&path).expect("remove image fixture");
        assert!(provider_image_input(&attachment).await.is_err());

        let oversized = directory.path().join("oversized.png");
        let file = std::fs::File::create(&oversized).expect("oversized fixture");
        file.set_len(MAX_IMAGE_BYTES + 1)
            .expect("extend oversized fixture");
        let error = provider_image_input(&json!({
            "source": {"kind": "file", "path": oversized},
            "alias": "oversized.png",
            "mimeType": "image/png",
        }))
        .await
        .expect_err("oversized replacement must fail");
        assert!(error.to_string().contains("limit is"));
    }

    #[tokio::test]
    async fn mime_drift_fails_closed() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("image.png");
        std::fs::write(&path, b"image").expect("image fixture");

        let error = provider_image_input(&json!({
            "source": {"kind": "file", "path": path},
            "alias": "image.png",
            "mimeType": "image/jpeg",
        }))
        .await
        .expect_err("MIME drift must fail");

        assert!(error.to_string().contains("does not match"));
    }
}
