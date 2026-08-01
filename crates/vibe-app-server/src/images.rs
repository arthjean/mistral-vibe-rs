use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::Value;
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
