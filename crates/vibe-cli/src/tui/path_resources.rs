use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use vibe_core::images::ImageFormat;

use super::path_mentions::{mention_values, resolve_candidate};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionStats {
    pub count: usize,
    pub context_types: BTreeMap<String, usize>,
    pub file_extensions: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PathResourceKind {
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
pub(super) struct PathResource {
    pub(super) alias: String,
    pub(super) path: PathBuf,
    pub(super) kind: PathResourceKind,
}

pub(super) struct PathPromptPayload {
    pub(super) resources: Vec<PathResource>,
    all_resources: Vec<PathResource>,
}

impl PathPromptPayload {
    pub(super) fn mention_stats(&self) -> MentionStats {
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

pub(super) fn build_path_prompt_payload(workspace: &Path, message: &str) -> PathPromptPayload {
    let all_resources = mention_values(message)
        .into_iter()
        .filter_map(|alias| path_resource(workspace, &alias))
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    let resources = all_resources
        .iter()
        .filter(|resource| seen.insert(resource.path.clone()))
        .cloned()
        .collect();
    PathPromptPayload {
        resources,
        all_resources,
    }
}

fn path_resource(workspace: &Path, alias: &str) -> Option<PathResource> {
    let path = resolve_candidate(workspace, alias)?;
    let metadata = fs::metadata(&path).ok()?;
    let kind = if metadata.is_dir() {
        PathResourceKind::Folder
    } else if ImageFormat::from_path(&path).is_some() {
        PathResourceKind::Image
    } else {
        PathResourceKind::File
    };
    Some(PathResource {
        alias: alias.to_owned(),
        path,
        kind,
    })
}
