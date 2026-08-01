use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_IMAGES_PER_MESSAGE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImageDigest([u8; 32]);

impl ImageDigest {
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("image contains {actual} bytes; limit is {maximum}")]
pub struct ImageSizeError {
    pub actual: usize,
    pub maximum: usize,
}

pub fn validate_image_size(actual: usize) -> Result<(), ImageSizeError> {
    let maximum = usize::try_from(MAX_IMAGE_BYTES).unwrap_or(usize::MAX);
    if actual > maximum {
        return Err(ImageSizeError { actual, maximum });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Gif,
    Jpeg,
    Png,
    Webp,
}

impl ImageFormat {
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?;
        if extension.eq_ignore_ascii_case("png") {
            Some(Self::Png)
        } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
            Some(Self::Jpeg)
        } else if extension.eq_ignore_ascii_case("gif") {
            Some(Self::Gif)
        } else if extension.eq_ignore_ascii_case("webp") {
            Some(Self::Webp)
        } else {
            None
        }
    }

    #[must_use]
    pub fn from_media_type(media_type: &str) -> Option<Self> {
        match media_type {
            "image/gif" => Some(Self::Gif),
            "image/jpeg" => Some(Self::Jpeg),
            "image/png" => Some(Self::Png),
            "image/webp" => Some(Self::Webp),
            _ => None,
        }
    }

    #[must_use]
    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Gif => "image/gif",
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ImageData {
    pub bytes: Vec<u8>,
    pub format: ImageFormat,
    pub digest: ImageDigest,
}

pub fn read_image(path: &Path) -> Result<ImageData, ImageReadError> {
    let format = ImageFormat::from_path(path)
        .ok_or_else(|| ImageReadError::Unsupported(path.to_path_buf()))?;
    let mut file = File::open(path).map_err(|source| ImageReadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| ImageReadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(ImageReadError::NotFile(path.to_path_buf()));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_IMAGE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| ImageReadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if let Err(error) = validate_image_size(bytes.len()) {
        return Err(ImageReadError::TooLarge {
            path: path.to_path_buf(),
            actual: error.actual,
            maximum: error.maximum,
        });
    }
    let digest = ImageDigest::of(&bytes);
    Ok(ImageData {
        bytes,
        format,
        digest,
    })
}

#[derive(Debug, Error)]
pub enum ImageReadError {
    #[error("unsupported image attachment `{0}`")]
    Unsupported(PathBuf),
    #[error("image attachment is not a regular file: `{0}`")]
    NotFile(PathBuf),
    #[error("image `{path}` contains {actual} bytes; limit is {maximum}")]
    TooLarge {
        path: PathBuf,
        actual: usize,
        maximum: usize,
    },
    #[error("image I/O failed at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_formats_are_case_insensitive_and_canonical() {
        assert_eq!(
            ImageFormat::from_path(Path::new("image.JPEG")),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(ImageFormat::Jpeg.media_type(), "image/jpeg");
        assert_eq!(
            ImageFormat::from_media_type("image/jpeg"),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(ImageFormat::from_media_type("image/svg+xml"), None);
        assert_eq!(ImageFormat::from_path(Path::new("notes.txt")), None);
        assert_eq!(ImageDigest::of(b"same"), ImageDigest::of(b"same"));
        assert_ne!(ImageDigest::of(b"same"), ImageDigest::of(b"other"));
        assert_eq!(
            validate_image_size(
                usize::try_from(MAX_IMAGE_BYTES).expect("10 MiB fits supported targets")
            ),
            Ok(())
        );
    }
}
