use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub mod library;

#[cfg(test)]
mod library_tests;

use crate::images::{ImageFormat, MAX_IMAGE_BYTES, MAX_IMAGES_PER_MESSAGE};

const MAX_TEXT_RESOURCE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserResourceKind {
    Text,
    Image,
    File,
    Directory,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserResource {
    pub kind: UserResourceKind,
    pub path: Option<PathBuf>,
    pub text: Option<String>,
    pub mime_type: Option<String>,
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelContent {
    Text {
        text: String,
    },
    Image {
        path: PathBuf,
        mime_type: String,
    },
    Resource {
        kind: UserResourceKind,
        path: Option<PathBuf>,
        metadata: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayResource {
    pub kind: UserResourceKind,
    pub label: String,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedUserPrompt {
    pub model_content: Vec<ModelContent>,
    pub display_content: Vec<DisplayResource>,
}

pub fn prepare_user_resources(
    resources: &[UserResource],
    allowed_roots: &[PathBuf],
    supports_images: bool,
) -> Result<PreparedUserPrompt, PromptError> {
    let roots = allowed_roots
        .iter()
        .map(|root| canonical_existing(root))
        .collect::<Result<Vec<_>, _>>()?;
    let image_count = resources
        .iter()
        .filter(|resource| resource.kind == UserResourceKind::Image)
        .count();
    if image_count > MAX_IMAGES_PER_MESSAGE {
        return Err(PromptError::TooManyImages {
            actual: image_count,
            maximum: MAX_IMAGES_PER_MESSAGE,
        });
    }

    let mut model_content = Vec::with_capacity(resources.len());
    let mut display_content = Vec::with_capacity(resources.len());
    for resource in resources {
        let resolved = resource
            .path
            .as_deref()
            .map(canonical_existing)
            .transpose()?;
        if let Some(path) = &resolved
            && !roots.iter().any(|root| path.starts_with(root))
        {
            return Err(PromptError::OutOfPolicyPath(path.clone()));
        }
        let label = resolved
            .as_deref()
            .map(|path| path.display().to_string())
            .or_else(|| resource.text.clone())
            .unwrap_or_else(|| format!("{:?}", resource.kind).to_ascii_lowercase());
        let model = match resource.kind {
            UserResourceKind::Text => ModelContent::Text {
                text: resource.text.clone().unwrap_or_default(),
            },
            UserResourceKind::Image => {
                if !supports_images {
                    return Err(PromptError::ImagesUnsupported);
                }
                let path = resolved
                    .clone()
                    .ok_or(PromptError::MissingResourcePath(UserResourceKind::Image))?;
                let metadata = fs::metadata(&path).map_err(|source| PromptError::Io {
                    path: path.clone(),
                    source,
                })?;
                if metadata.len() > MAX_IMAGE_BYTES {
                    return Err(PromptError::ImageTooLarge(path));
                }
                let mime_type = resource
                    .mime_type
                    .clone()
                    .or_else(|| {
                        ImageFormat::from_path(&path)
                            .map(ImageFormat::media_type)
                            .map(str::to_owned)
                    })
                    .ok_or_else(|| PromptError::UnsupportedImage(path.clone()))?;
                ModelContent::Image { path, mime_type }
            }
            UserResourceKind::File => {
                let path = resolved
                    .clone()
                    .ok_or(PromptError::MissingResourcePath(UserResourceKind::File))?;
                let metadata = fs::metadata(&path).map_err(|source| PromptError::Io {
                    path: path.clone(),
                    source,
                })?;
                if metadata.len() > MAX_TEXT_RESOURCE_BYTES {
                    return Err(PromptError::TextResourceTooLarge(path));
                }
                let content = fs::read_to_string(&path).map_err(|source| PromptError::Io {
                    path: path.clone(),
                    source,
                })?;
                ModelContent::Text {
                    text: format!("Contents of {}:\n\n{content}", path.display()),
                }
            }
            UserResourceKind::Directory => {
                let path = resolved.clone().ok_or(PromptError::MissingResourcePath(
                    UserResourceKind::Directory,
                ))?;
                let mut entries = fs::read_dir(&path)
                    .map_err(|source| PromptError::Io {
                        path: path.clone(),
                        source,
                    })?
                    .map(|entry| {
                        entry
                            .map(|entry| entry.file_name().to_string_lossy().into_owned())
                            .map_err(|source| PromptError::Io {
                                path: path.clone(),
                                source,
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                entries.sort();
                ModelContent::Text {
                    text: format!("Directory {}:\n{}", path.display(), entries.join("\n")),
                }
            }
            UserResourceKind::Other => ModelContent::Resource {
                kind: resource.kind,
                path: resolved.clone(),
                metadata: resource.metadata.clone(),
            },
        };
        model_content.push(model);
        display_content.push(DisplayResource {
            kind: resource.kind,
            label,
            path: resolved,
        });
    }
    Ok(PreparedUserPrompt {
        model_content,
        display_content,
    })
}

fn canonical_existing(path: &Path) -> Result<PathBuf, PromptError> {
    fs::canonicalize(path).map_err(|source| PromptError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("resource path is outside the trusted roots: `{0}`")]
    OutOfPolicyPath(PathBuf),
    #[error("resource kind `{0:?}` requires a path")]
    MissingResourcePath(UserResourceKind),
    #[error("the active model does not support image attachments")]
    ImagesUnsupported,
    #[error("unsupported image attachment `{0}`")]
    UnsupportedImage(PathBuf),
    #[error("image attachment exceeds the 10 MiB limit: `{0}`")]
    ImageTooLarge(PathBuf),
    #[error("text resource exceeds the 2 MiB limit: `{0}`")]
    TextResourceTooLarge(PathBuf),
    #[error("message contains {actual} images; maximum is {maximum}")]
    TooManyImages { actual: usize, maximum: usize },
    #[error("prompt I/O failed at `{path}`: {source}")]
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
    fn model_and_display_resources_remain_separately_typed_and_policy_bounded() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let root = temporary.path().join("project");
        fs::create_dir_all(&root).expect("project root");
        let file = root.join("note.txt");
        let image = root.join("image.png");
        fs::write(&file, "content").expect("file fixture");
        fs::write(&image, [137, 80, 78, 71]).expect("image fixture");
        let prepared = prepare_user_resources(
            &[
                UserResource {
                    kind: UserResourceKind::Text,
                    path: None,
                    text: Some("hello".to_owned()),
                    mime_type: None,
                    metadata: Value::Null,
                },
                UserResource {
                    kind: UserResourceKind::File,
                    path: Some(file),
                    text: None,
                    mime_type: None,
                    metadata: Value::Null,
                },
                UserResource {
                    kind: UserResourceKind::Image,
                    path: Some(image),
                    text: None,
                    mime_type: None,
                    metadata: Value::Null,
                },
            ],
            std::slice::from_ref(&root),
            true,
        )
        .expect("resources prepare");
        assert_eq!(prepared.model_content.len(), 3);
        assert_eq!(prepared.display_content.len(), 3);
        assert!(matches!(
            prepared.model_content[2],
            ModelContent::Image { .. }
        ));

        let outside = temporary.path().join("outside.txt");
        fs::write(&outside, "secret").expect("outside fixture");
        assert!(matches!(
            prepare_user_resources(
                &[UserResource {
                    kind: UserResourceKind::File,
                    path: Some(outside),
                    text: None,
                    mime_type: None,
                    metadata: Value::Null,
                }],
                &[root],
                true,
            ),
            Err(PromptError::OutOfPolicyPath(_))
        ));
    }
}
