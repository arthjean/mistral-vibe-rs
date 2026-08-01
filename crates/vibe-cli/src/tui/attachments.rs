use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use vibe_app_server::client::{PublicContentBlock, TurnRequest};
use vibe_core::images::{ImageFormat, ImageReadError, MAX_IMAGES_PER_MESSAGE, read_image};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDraft {
    text: String,
    transient_images: Vec<PathBuf>,
}

impl PromptDraft {
    #[must_use]
    pub fn new<'a>(
        workspace: &Path,
        text: impl Into<String>,
        tracked_images: impl IntoIterator<Item = &'a PathBuf>,
    ) -> Self {
        let text = text.into();
        let tracked = tracked_images.into_iter().cloned().collect::<HashSet<_>>();
        let transient_images = mention_tokens(&text)
            .into_iter()
            .filter_map(|token| resolve_owned_candidate(workspace, &token.value, &tracked))
            .fold(Vec::new(), |mut paths, path| {
                if !paths.contains(&path) {
                    paths.push(path);
                }
                paths
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

    #[must_use]
    pub fn transient_images(&self) -> &[PathBuf] {
        &self.transient_images
    }

    #[must_use]
    pub fn into_text(self) -> String {
        self.text
    }
}

#[must_use]
pub fn normalize_pasted_text(pasted: &str) -> String {
    let trimmed = pasted.trim();
    if !trimmed.is_empty() && !trimmed.contains(['\n', '\r']) && !trimmed.starts_with('@') {
        let candidate = unescaped_path_candidate(trimmed);
        if candidate.starts_with(['/', '~']) && is_image_file(&candidate) {
            return format!("@{}", quote_path_if_needed(&candidate));
        }
    }

    rewrite_bare_image_paths(pasted)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionStats {
    pub count: usize,
    pub context_types: BTreeMap<String, usize>,
    pub file_extensions: BTreeMap<String, usize>,
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
    let payload = build_path_prompt_payload(workspace, draft.text())?;
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
    for path in draft.transient_images() {
        if !images.iter().any(|resource| resource.path == *path) {
            return Err(SubmissionError::TransientImageChanged { path: path.clone() });
        }
    }

    let mut input = Vec::with_capacity(images.len().saturating_add(1));
    input.push(PublicContentBlock::Text {
        text: draft.text().to_owned(),
    });
    let mut cleanup_paths = Vec::new();
    for resource in images {
        let image = read_image(&resource.path).map_err(|error| match error {
            ImageReadError::TooLarge {
                actual, maximum, ..
            } => SubmissionError::ImageTooLarge {
                path: resource.alias.clone(),
                bytes: actual,
                limit: maximum,
            },
            error => SubmissionError::ImageRead(error.to_string()),
        })?;
        input.push(PublicContentBlock::Image {
            attachment: json!({
                "uri": format!("file://{}", resource.path.to_string_lossy()),
                "name": resource.alias,
                "bytes": image.bytes.len(),
                "mediaType": image.format.media_type(),
                "data": BASE64_STANDARD.encode(&image.bytes),
            }),
        });
        if draft.transient_images().contains(&resource.path) {
            cleanup_paths.push(resource.path.clone());
        }
    }

    let turn = TurnRequest {
        prompt: draft.text().to_owned(),
        input,
        client_user_message_id: None,
        auto_title: None,
        user_display_content: Some(json!({"type": "text", "text": draft.text()})),
        mention_stats: Some(serde_json::to_value(&mention_stats).map_err(SubmissionError::Json)?),
    };
    Ok(PreparedSubmission {
        turn,
        mention_stats,
        cleanup_paths,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathResourceKind {
    File,
    Folder,
    Image,
}

impl PathResourceKind {
    const fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Folder => "folder",
            Self::Image => "image",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathResource {
    alias: String,
    path: PathBuf,
    kind: PathResourceKind,
}

struct PathPromptPayload {
    resources: Vec<PathResource>,
    all_resources: Vec<PathResource>,
}

impl PathPromptPayload {
    fn mention_stats(&self) -> MentionStats {
        let mut stats = MentionStats {
            count: self.all_resources.len(),
            ..MentionStats::default()
        };
        for resource in &self.all_resources {
            *stats
                .context_types
                .entry(resource.kind.label().to_owned())
                .or_default() += 1;
            if resource.kind == PathResourceKind::File {
                let extension = resource
                    .path
                    .extension()
                    .map_or_else(String::new, |extension| {
                        format!(".{}", extension.to_string_lossy())
                    });
                *stats.file_extensions.entry(extension).or_default() += 1;
            }
        }
        stats
    }
}

fn build_path_prompt_payload(
    workspace: &Path,
    message: &str,
) -> Result<PathPromptPayload, SubmissionError> {
    let canonical_workspace = fs::canonicalize(workspace)
        .map_err(|error| SubmissionError::Workspace(error.to_string()))?;
    let all_resources = mention_tokens(message)
        .into_iter()
        .filter_map(|token| path_resource(&canonical_workspace, &token))
        .collect::<Vec<_>>();

    let mut seen = HashSet::new();
    let resources = all_resources
        .iter()
        .filter(|resource| seen.insert(resource.path.clone()))
        .cloned()
        .collect();
    Ok(PathPromptPayload {
        resources,
        all_resources,
    })
}

fn path_resource(workspace: &Path, token: &ScannedPath) -> Option<PathResource> {
    let path = resolve_candidate(workspace, &token.value)?;
    let metadata = fs::metadata(&path).ok()?;
    let kind = if metadata.is_dir() {
        PathResourceKind::Folder
    } else if metadata.is_file() && ImageFormat::from_path(&path).is_some() {
        PathResourceKind::Image
    } else {
        PathResourceKind::File
    };
    Some(PathResource {
        alias: token.value.clone(),
        path,
        kind,
    })
}

fn resolve_candidate(workspace: &Path, candidate: &str) -> Option<PathBuf> {
    let candidate = expand_tilde_path(candidate);
    let path = if candidate.is_absolute() {
        candidate
    } else {
        workspace.join(candidate)
    };
    fs::canonicalize(path).ok()
}

fn resolve_owned_candidate(
    workspace: &Path,
    candidate: &str,
    tracked: &HashSet<PathBuf>,
) -> Option<PathBuf> {
    let candidate = expand_tilde_path(candidate);
    let path = if candidate.is_absolute() {
        candidate
    } else {
        workspace.join(candidate)
    };
    if tracked.contains(&path) {
        return Some(path);
    }
    fs::canonicalize(path)
        .ok()
        .filter(|path| tracked.contains(path))
}

#[derive(Debug, Clone, Copy)]
enum PathSyntax {
    Mention,
    Pasted,
}

struct ScannedPath {
    value: String,
}

fn mention_tokens(text: &str) -> Vec<ScannedPath> {
    let mut tokens = Vec::new();
    let mut cursor = 0usize;
    while cursor < text.len() {
        let Some(relative_start) = text[cursor..].find('@') else {
            break;
        };
        let start = cursor.saturating_add(relative_start);
        let boundary = text[..start].chars().next_back();
        if boundary.is_some_and(|character| character.is_alphanumeric() || character == '_') {
            cursor = start.saturating_add(1);
            continue;
        }
        let value_start = start.saturating_add(1);
        let Some((value, end)) = scan_path_value(text, value_start, PathSyntax::Mention) else {
            cursor = start.saturating_add(1);
            continue;
        };
        tokens.push(ScannedPath { value });
        cursor = end;
    }
    tokens
}

fn scan_path_value(text: &str, start: usize, syntax: PathSyntax) -> Option<(String, usize)> {
    let head = text.get(start..)?.chars().next()?;
    if matches!(head, '\'' | '"') {
        let content_start = start.saturating_add(head.len_utf8());
        let relative_end = text.get(content_start..)?.find(head)?;
        let content_end = content_start.saturating_add(relative_end);
        return Some((
            text[content_start..content_end].to_owned(),
            content_end.saturating_add(head.len_utf8()),
        ));
    }
    if matches!(syntax, PathSyntax::Pasted) && !matches!(head, '/' | '~') {
        return None;
    }

    let mut value = String::new();
    let mut end = start;
    while end < text.len() {
        let character = text.get(end..)?.chars().next()?;
        if matches!(syntax, PathSyntax::Pasted)
            && character == '\\'
            && text[end + character.len_utf8()..].starts_with(' ')
        {
            value.push(' ');
            end = end.saturating_add(character.len_utf8() + 1);
            continue;
        }
        let accepted = match syntax {
            PathSyntax::Mention => is_mention_path_character(character),
            PathSyntax::Pasted => !character.is_whitespace(),
        };
        if !accepted {
            break;
        }
        value.push(character);
        end = end.saturating_add(character.len_utf8());
    }
    (!value.is_empty()).then_some((value, end))
}

fn unescaped_path_candidate(value: &str) -> String {
    let unquoted = ['\'', '"']
        .into_iter()
        .find_map(|quote| {
            value
                .strip_prefix(quote)
                .and_then(|inner| inner.strip_suffix(quote))
        })
        .unwrap_or(value);
    unquoted.replace("\\ ", " ")
}

fn is_image_file(candidate: &str) -> bool {
    let path = expand_tilde_path(candidate);
    path.is_absolute() && ImageFormat::from_path(&path).is_some() && path.is_file()
}

fn quote_path_if_needed(path: &str) -> String {
    if path.contains(' ') {
        format!("'{path}'")
    } else {
        path.to_owned()
    }
}

fn rewrite_bare_image_paths(text: &str) -> String {
    if !text.contains(['/', '~', '\'', '"']) {
        return text.to_owned();
    }
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while cursor < text.len() {
        if is_path_token_boundary(text, cursor)
            && let Some((candidate, end)) = scan_path_value(text, cursor, PathSyntax::Pasted)
            && is_image_file(&candidate)
        {
            output.push('@');
            output.push_str(&quote_path_if_needed(&candidate));
            cursor = end;
            continue;
        }
        let Some(character) = text[cursor..].chars().next() else {
            break;
        };
        output.push(character);
        cursor = cursor.saturating_add(character.len_utf8());
    }
    output
}

fn is_path_token_boundary(text: &str, byte: usize) -> bool {
    if byte == 0 {
        return true;
    }
    let previous = text[..byte].chars().next_back();
    previous != Some('@')
        && previous.is_some_and(|character| character.is_whitespace() || "(<[".contains(character))
}

fn is_mention_path_character(character: char) -> bool {
    character.is_alphanumeric() || "._/\\-()[]{}~".contains(character)
}

fn expand_tilde_path(value: &str) -> PathBuf {
    let path = Path::new(value);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(first)) if first == "~") {
        return path.to_path_buf();
    }
    let Some(mut home) = user_home_directory() else {
        return path.to_path_buf();
    };
    home.extend(components);
    home
}

fn user_home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .or_else(|| {
                let mut home = PathBuf::from(std::env::var_os("HOMEDRIVE")?);
                home.push(std::env::var_os("HOMEPATH")?);
                Some(home)
            })
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
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
    #[error("Too many image attachments (got {actual}, max {maximum})")]
    TooManyImages { actual: usize, maximum: usize },
    #[error("Image `{path}` contains {bytes} bytes; limit is {limit}")]
    ImageTooLarge {
        path: String,
        bytes: usize,
        limit: usize,
    },
    #[error("Clipboard image changed before it could be attached: `{path}`")]
    TransientImageChanged { path: PathBuf },
    #[error("Image could not be attached: {0}")]
    ImageRead(String),
    #[error("mention statistics could not be encoded: {0}")]
    Json(serde_json::Error),
}

#[cfg(test)]
#[path = "attachments/tests.rs"]
mod tests;
