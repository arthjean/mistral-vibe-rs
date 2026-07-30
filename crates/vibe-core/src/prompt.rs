use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const MAX_TEXT_RESOURCE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_IMAGES: usize = 8;
const AGENTS_FILE: &str = "AGENTS.md";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentSummary {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionDocument {
    pub directory: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptComposition {
    pub base: String,
    pub headless: bool,
    pub commit_policy: Option<String>,
    pub model_info: Option<String>,
    pub os_tool_guidance: Option<String>,
    pub skills: Vec<SkillSummary>,
    pub subagents: Vec<SubagentSummary>,
    pub scratchpad: Option<PathBuf>,
    pub project_context: Option<String>,
    pub project_context_stale: bool,
    pub additional_directories: Vec<PathBuf>,
    pub user_instructions: Option<(PathBuf, String)>,
    pub project_instructions: Vec<InstructionDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposedPrompt {
    pub text: String,
    pub section_names: Vec<String>,
    pub notices: Vec<String>,
}

impl PromptComposition {
    #[must_use]
    pub fn compose(&self) -> ComposedPrompt {
        let mut sections = Vec::new();
        let mut names = Vec::new();
        let mut notices = Vec::new();
        push_section(&mut sections, &mut names, "base", self.base.trim());
        if self.headless {
            push_section(
                &mut sections,
                &mut names,
                "headless",
                "# Headless Mode\n\nNo human is available for interactive callbacks. Resolve ordinary ambiguity autonomously and complete the task in one pass.",
            );
        }
        if let Some(policy) = self.commit_policy.as_deref() {
            push_section(&mut sections, &mut names, "commit_policy", policy.trim());
        }
        if let Some(model) = self.model_info.as_deref() {
            push_section(
                &mut sections,
                &mut names,
                "model_info",
                &format!("Your model name is: `{}`", model.trim()),
            );
        }
        if let Some(guidance) = self.os_tool_guidance.as_deref() {
            push_section(
                &mut sections,
                &mut names,
                "os_tool_guidance",
                guidance.trim(),
            );
        }
        if !self.skills.is_empty() {
            let mut skills = self.skills.clone();
            skills.sort_by(|left, right| left.name.cmp(&right.name));
            let mut section = String::from("# Available Skills\n\n<available_skills>");
            for skill in skills {
                section.push_str("\n  <skill>\n    <name>");
                section.push_str(&escape_xml(&skill.name));
                section.push_str("</name>\n    <description>");
                section.push_str(&escape_xml(&skill.description));
                section.push_str("</description>");
                if let Some(path) = skill.path {
                    section.push_str("\n    <path>");
                    section.push_str(&escape_xml(&path.to_string_lossy()));
                    section.push_str("</path>");
                }
                section.push_str("\n  </skill>");
            }
            section.push_str("\n</available_skills>");
            push_section(&mut sections, &mut names, "skills", &section);
        }
        if !self.subagents.is_empty() {
            let mut subagents = self.subagents.clone();
            subagents.sort_by(|left, right| left.name.cmp(&right.name));
            let mut section = String::from(
                "# Available Subagents\n\nThe following subagents can be delegated work:",
            );
            for agent in subagents {
                section.push_str("\n- **");
                section.push_str(&agent.name);
                section.push_str("**: ");
                section.push_str(&agent.description);
            }
            push_section(&mut sections, &mut names, "subagents", &section);
        }
        if let Some(scratchpad) = &self.scratchpad {
            push_section(
                &mut sections,
                &mut names,
                "scratchpad",
                &format!(
                    "# Scratchpad Directory\n\nSession-scoped temporary files belong at: `{}`",
                    scratchpad.display()
                ),
            );
        }
        if let Some(context) = self.project_context.as_deref() {
            push_section(&mut sections, &mut names, "project_context", context.trim());
            if self.project_context_stale {
                notices.push(
                    "Project context may be stale; verify Git state before relying on it."
                        .to_owned(),
                );
            }
        }
        if !self.additional_directories.is_empty() {
            let mut roots = self.additional_directories.clone();
            roots.sort();
            roots.dedup();
            let rendered = roots
                .into_iter()
                .map(|path| format!(" - {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n");
            push_section(
                &mut sections,
                &mut names,
                "additional_directories",
                &format!(
                    "Additional working directories (with the same file-access policy as the primary workspace):\n{rendered}"
                ),
            );
        }
        let mut documents = Vec::new();
        if let Some((path, content)) = &self.user_instructions
            && !content.trim().is_empty()
        {
            documents.push(format!(
                "## User instructions\n\nContents of {}:\n\n{}",
                path.display(),
                content.trim()
            ));
        }
        if !self.project_instructions.is_empty() {
            documents.push("## Project instructions (checked into the codebase)".to_owned());
            for document in &self.project_instructions {
                if !document.content.trim().is_empty() {
                    documents.push(format!(
                        "Contents of {}/{}:\n\n{}",
                        document.directory.display(),
                        AGENTS_FILE,
                        document.content.trim()
                    ));
                }
            }
        }
        if !documents.is_empty() {
            push_section(
                &mut sections,
                &mut names,
                "instructions",
                &documents.join("\n\n"),
            );
        }
        ComposedPrompt {
            text: sections.join("\n\n"),
            section_names: names,
            notices,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptSource {
    Project,
    User,
    Builtin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedPrompt {
    pub id: String,
    pub content: String,
    pub source: PromptSource,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct PromptResolver {
    project_directories: Vec<PathBuf>,
    user_directories: Vec<PathBuf>,
    builtins: BTreeMap<String, PathBuf>,
    project_trusted: bool,
}

impl PromptResolver {
    #[must_use]
    pub fn new(
        project_directories: Vec<PathBuf>,
        user_directories: Vec<PathBuf>,
        builtins: BTreeMap<String, PathBuf>,
        project_trusted: bool,
    ) -> Self {
        Self {
            project_directories,
            user_directories,
            builtins,
            project_trusted,
        }
    }

    pub fn resolve(&self, prompt_id: &str) -> Result<ResolvedPrompt, PromptError> {
        validate_prompt_id(prompt_id)?;
        let filename = format!("{prompt_id}.md");
        if self.project_trusted {
            for directory in &self.project_directories {
                if let Some(prompt) = read_prompt_candidate(
                    prompt_id,
                    PromptSource::Project,
                    directory.join(&filename),
                )? {
                    return Ok(prompt);
                }
            }
        }
        for directory in &self.user_directories {
            if let Some(prompt) =
                read_prompt_candidate(prompt_id, PromptSource::User, directory.join(&filename))?
            {
                return Ok(prompt);
            }
        }
        if let Some(path) = self.builtins.get(&prompt_id.to_ascii_lowercase())
            && let Some(prompt) =
                read_prompt_candidate(prompt_id, PromptSource::Builtin, path.clone())?
        {
            return Ok(prompt);
        }
        Err(PromptError::MissingPrompt {
            id: prompt_id.to_owned(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct InstructionLoader {
    user_home: PathBuf,
    project_roots: Vec<(PathBuf, PathBuf)>,
}

impl InstructionLoader {
    #[must_use]
    pub fn new(user_home: PathBuf, project_roots: Vec<(PathBuf, PathBuf)>) -> Self {
        Self {
            user_home,
            project_roots,
        }
    }

    pub fn user_document(&self) -> Result<Option<(PathBuf, String)>, PromptError> {
        let path = self.user_home.join(AGENTS_FILE);
        read_optional_trimmed(&path).map(|content| content.map(|content| (path, content)))
    }

    pub fn project_documents(&self) -> Result<Vec<InstructionDocument>, PromptError> {
        let mut by_directory = BTreeMap::new();
        for (root, trust_root) in &self.project_roots {
            let root = canonical_existing(root)?;
            let trust_root = canonical_existing(trust_root)?;
            if !root.starts_with(&trust_root) {
                return Err(PromptError::OutOfPolicyPath(root));
            }
            let mut directories = root
                .ancestors()
                .take_while(|directory| directory.starts_with(&trust_root))
                .map(Path::to_path_buf)
                .collect::<Vec<_>>();
            directories.reverse();
            for directory in directories {
                let path = directory.join(AGENTS_FILE);
                if let Some(content) = read_optional_trimmed(&path)? {
                    by_directory
                        .entry(directory.clone())
                        .or_insert(InstructionDocument { directory, content });
                }
            }
        }
        Ok(by_directory.into_values().collect())
    }

    pub fn lazy_documents_for(
        &self,
        resource_path: &Path,
    ) -> Result<Vec<InstructionDocument>, PromptError> {
        let resource = canonical_existing(resource_path)?;
        for (root, _) in &self.project_roots {
            let root = canonical_existing(root)?;
            if resource.starts_with(&root) {
                let start = if resource.is_dir() {
                    resource
                } else {
                    resource
                        .parent()
                        .map(Path::to_path_buf)
                        .ok_or_else(|| PromptError::OutOfPolicyPath(resource.clone()))?
                };
                let mut directories = start
                    .ancestors()
                    .take_while(|directory| **directory != root)
                    .map(Path::to_path_buf)
                    .collect::<Vec<_>>();
                directories.reverse();
                let mut documents = Vec::new();
                for directory in directories {
                    if let Some(content) = read_optional_trimmed(&directory.join(AGENTS_FILE))? {
                        documents.push(InstructionDocument { directory, content });
                    }
                }
                return Ok(documents);
            }
        }
        Err(PromptError::OutOfPolicyPath(resource))
    }
}

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
    if image_count > MAX_IMAGES {
        return Err(PromptError::TooManyImages {
            actual: image_count,
            maximum: MAX_IMAGES,
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
                    .or_else(|| image_mime(&path).map(str::to_owned))
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

fn push_section(sections: &mut Vec<String>, names: &mut Vec<String>, name: &str, content: &str) {
    if !content.is_empty() {
        sections.push(content.to_owned());
        names.push(name.to_owned());
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn validate_prompt_id(prompt_id: &str) -> Result<(), PromptError> {
    let valid = !prompt_id.is_empty()
        && prompt_id != "."
        && prompt_id != ".."
        && !prompt_id.contains('/')
        && !prompt_id.contains('\\')
        && prompt_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    if valid {
        Ok(())
    } else {
        Err(PromptError::InvalidPromptId(prompt_id.to_owned()))
    }
}

fn read_prompt_candidate(
    prompt_id: &str,
    source: PromptSource,
    path: PathBuf,
) -> Result<Option<ResolvedPrompt>, PromptError> {
    match fs::read_to_string(&path) {
        Ok(content) => Ok(Some(ResolvedPrompt {
            id: prompt_id.to_owned(),
            content: content.trim().to_owned(),
            source,
            path,
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(PromptError::Io { path, source }),
    }
}

fn read_optional_trimmed(path: &Path) -> Result<Option<String>, PromptError> {
    match fs::read_to_string(path) {
        Ok(content) => Ok((!content.trim().is_empty()).then(|| content.trim().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(PromptError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn canonical_existing(path: &Path) -> Result<PathBuf, PromptError> {
    fs::canonicalize(path).map_err(|source| PromptError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn image_mime(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("invalid prompt ID `{0}`")]
    InvalidPromptId(String),
    #[error("prompt `{id}` was not found")]
    MissingPrompt { id: String },
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
    fn composition_preserves_section_order_and_instruction_precedence() {
        let composition = PromptComposition {
            base: "base".to_owned(),
            headless: true,
            commit_policy: Some("commit policy".to_owned()),
            model_info: Some("model".to_owned()),
            os_tool_guidance: Some("os guidance".to_owned()),
            skills: vec![SkillSummary {
                name: "beta".to_owned(),
                description: "B".to_owned(),
                path: None,
            }],
            subagents: vec![SubagentSummary {
                name: "explore".to_owned(),
                description: "read only".to_owned(),
            }],
            scratchpad: Some(PathBuf::from("/scratch")),
            project_context: Some("project context".to_owned()),
            project_context_stale: true,
            additional_directories: vec![PathBuf::from("/extra")],
            user_instructions: Some((PathBuf::from("/home/.vibe/AGENTS.md"), "user".to_owned())),
            project_instructions: vec![InstructionDocument {
                directory: PathBuf::from("/project"),
                content: "project".to_owned(),
            }],
        };
        let composed = composition.compose();
        assert_eq!(
            composed.section_names,
            [
                "base",
                "headless",
                "commit_policy",
                "model_info",
                "os_tool_guidance",
                "skills",
                "subagents",
                "scratchpad",
                "project_context",
                "additional_directories",
                "instructions",
            ]
        );
        assert!(
            composed.text.find("## User").expect("user section")
                < composed.text.find("## Project").expect("project section")
        );
        assert_eq!(composed.notices.len(), 1);
    }

    #[test]
    fn custom_prompt_precedence_is_project_then_user_then_builtin() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let project = temporary.path().join("project");
        let user = temporary.path().join("user");
        let builtin = temporary.path().join("builtin.md");
        fs::create_dir_all(&project).expect("project prompt directory");
        fs::create_dir_all(&user).expect("user prompt directory");
        fs::write(project.join("probe.md"), "project").expect("project prompt");
        fs::write(user.join("probe.md"), "user").expect("user prompt");
        fs::write(&builtin, "builtin").expect("builtin prompt");
        let resolver = PromptResolver::new(
            vec![project.clone()],
            vec![user.clone()],
            BTreeMap::from([("probe".to_owned(), builtin)]),
            true,
        );
        assert_eq!(
            resolver.resolve("probe").expect("project wins").source,
            PromptSource::Project
        );
        fs::remove_file(project.join("probe.md")).expect("project prompt removed");
        assert_eq!(
            resolver.resolve("probe").expect("user wins").source,
            PromptSource::User
        );
        assert!(matches!(
            resolver.resolve("../escape"),
            Err(PromptError::InvalidPromptId(_))
        ));
    }

    #[test]
    fn instruction_loader_orders_outermost_first_and_supports_lazy_subdirectories() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let root = temporary.path().join("project");
        let nested = root.join("src/nested");
        fs::create_dir_all(&nested).expect("nested directories");
        fs::write(root.join(AGENTS_FILE), "root").expect("root instructions");
        fs::write(root.join("src").join(AGENTS_FILE), "src").expect("src instructions");
        fs::write(nested.join("file.rs"), "fn main() {}").expect("resource");
        let loader = InstructionLoader::new(
            temporary.path().join("home"),
            vec![(root.clone(), root.clone())],
        );
        assert_eq!(
            loader
                .project_documents()
                .expect("project documents")
                .iter()
                .map(|document| document.content.as_str())
                .collect::<Vec<_>>(),
            ["root"]
        );
        assert_eq!(
            loader
                .lazy_documents_for(&nested.join("file.rs"))
                .expect("lazy documents")
                .iter()
                .map(|document| document.content.as_str())
                .collect::<Vec<_>>(),
            ["src"]
        );
    }

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
