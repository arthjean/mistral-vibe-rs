use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use thiserror::Error;

pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_IMAGES_PER_MESSAGE: usize = 8;

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
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_IMAGE_BYTES {
        return Err(ImageReadError::TooLarge {
            path: path.to_path_buf(),
            actual: bytes.len(),
            maximum: usize::try_from(MAX_IMAGE_BYTES).unwrap_or(usize::MAX),
        });
    }
    Ok(ImageData { bytes, format })
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
        assert_eq!(ImageFormat::from_path(Path::new("notes.txt")), None);
    }
}
