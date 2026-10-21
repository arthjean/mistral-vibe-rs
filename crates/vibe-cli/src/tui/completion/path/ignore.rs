//! The index outside a git work tree: the workspace walk and the ignore rules
//! it skips entries by, reference `IgnoreRules`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use vibe_core::matching::fnmatch;
use vibe_core::pycompat;
use vibe_core::workspace::text_file;

use super::{IndexedPath, indexed_path};

pub(super) fn walk_workspace(
    directory: &Path,
    prefix: &str,
    rules: &IgnoreRules,
    output: &mut BTreeMap<String, IndexedPath>,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let is_directory = file_type.is_dir();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if rules.should_ignore(&rel, &name, is_directory) {
            continue;
        }
        output.insert(rel.clone(), indexed_path(rel.clone(), name, is_directory));
        // Never follow symlinks. This includes the link as a file-like entry
        // while making cycles impossible, matching `follow_symlinks=False`.
        if is_directory && !file_type.is_symlink() {
            walk_workspace(&entry.path(), &rel, rules, output);
        }
    }
}

pub(super) const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    ".git/",
    "__pycache__/",
    "node_modules/",
    ".DS_Store",
    "*.pyc",
    "*.log",
    ".vscode/",
    ".idea/",
    "/build/",
    "dist/",
    "target/",
    ".next/",
    ".nuxt/",
    "coverage/",
    ".nyc_output/",
    "*.egg-info",
    ".pytest_cache/",
    ".tox/",
    "vendor/",
    "third_party/",
    "deps/",
    "*.min.js",
    "*.min.css",
    "*.bundle.js",
    "*.chunk.js",
    ".cache/",
    "tmp/",
    "temp/",
    "logs/",
    ".uv-cache/",
    ".ruff_cache/",
    ".venv/",
    "venv/",
    ".mypy_cache/",
    "htmlcov/",
    ".coverage",
];

pub(super) struct IgnoreRule {
    pub(super) pattern: String,
    pub(super) excludes: bool,
    pub(super) directory_only: bool,
    pub(super) name_only: bool,
    pub(super) anchored_at_root: bool,
}

#[derive(Default)]
pub(super) struct IgnoreRules {
    rules: Vec<IgnoreRule>,
    pub(super) root: Option<PathBuf>,
}

impl std::fmt::Debug for IgnoreRules {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IgnoreRules")
            .field("root", &self.root)
            .field("rules", &self.rules.len())
            .finish()
    }
}

impl IgnoreRules {
    /// Compiles the defaults and the root's `.gitignore` once per root, the way
    /// the reference's `ensure_for_root` does.
    pub(super) fn ensure_for_root(&mut self, root: &Path) {
        if self.root.as_deref() == Some(root) {
            return;
        }
        *self = Self {
            rules: Self::load(root).rules,
            root: Some(root.to_path_buf()),
        };
    }

    pub(super) fn reset(&mut self) {
        self.rules.clear();
        self.root = None;
    }

    /// Reference `IgnoreRules._build_patterns`: the defaults, then the root's
    /// `.gitignore` decoded as `read_safe` decodes it and split where
    /// `str.splitlines` splits, each line stripped of what `str.strip` strips.
    pub(super) fn load(root: &Path) -> Self {
        let mut rules = DEFAULT_IGNORE_PATTERNS
            .iter()
            .filter_map(|pattern| IgnoreRule::parse(pattern, true))
            .collect::<Vec<_>>();
        if let Ok(bytes) = fs::read(root.join(".gitignore")) {
            let contents = text_file::decode(&bytes).text;
            for line in pycompat::splitlines(&contents, false) {
                let mut raw = pycompat::strip(line);
                if raw.is_empty() || raw.starts_with('#') {
                    continue;
                }
                if let Some((before, _)) = raw.split_once('#') {
                    raw = before.trim_end_matches(pycompat::isspace);
                }
                if raw.is_empty() {
                    continue;
                }
                let excludes = !raw.starts_with('!');
                if let Some(negated) = raw.strip_prefix('!') {
                    raw = negated.trim_start_matches(pycompat::isspace);
                }
                if let Some(rule) = IgnoreRule::parse(raw, excludes) {
                    rules.push(rule);
                }
            }
        }
        Self {
            rules,
            root: Some(root.to_path_buf()),
        }
    }

    pub(super) fn should_ignore(&self, rel: &str, name: &str, is_directory: bool) -> bool {
        let mut ignored = false;
        for rule in &self.rules {
            if rule.matches(rel, name, is_directory) {
                ignored = rule.excludes;
            }
        }
        ignored
    }
}

impl IgnoreRule {
    pub(super) fn parse(raw: &str, excludes: bool) -> Option<Self> {
        let anchored_at_root = raw.starts_with('/');
        let raw = raw.strip_prefix('/').unwrap_or(raw);
        let directory_only = raw.ends_with('/');
        let pattern = raw.trim_end_matches('/');
        if pattern.is_empty() {
            return None;
        }
        Some(Self {
            pattern: pattern.to_owned(),
            excludes,
            directory_only,
            name_only: !pattern.contains('/'),
            anchored_at_root,
        })
    }

    fn matches(&self, rel: &str, name: &str, is_directory: bool) -> bool {
        if self.directory_only && !is_directory {
            return false;
        }
        let target = if self.name_only {
            if self.anchored_at_root && rel.contains('/') {
                return false;
            }
            name
        } else {
            rel
        };
        fnmatch(&self.pattern, target)
    }
}
