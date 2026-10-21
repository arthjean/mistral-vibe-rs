//! What a unified turn's input gains before the harness takes it.
//!
//! Reference `_prepared_turn_params` (`vibe/app_server/_unified_harness_backend_adapter.py`):
//! the bodies of the skills the prompt invokes, the scratchpad notes the
//! model no longer reads, the files the prompt mentions, then the `/loop`
//! instructions.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use vibe_core::vision::ImageToDescribe;
use vibe_harness::invocation;

use super::{AppServer, ProtocolErrorCode, ProtocolFault, UnifiedSession};

/// Reference `_workspace.py`: how much of one mentioned file is inlined, and
/// how many files one message may mention.
const MENTIONED_FILE_MAX_BYTES: usize = 50 * 1024;
const MENTIONED_FILE_LINE_LIMIT: usize = 2000;
const MENTIONED_FILE_MAX_FILES: usize = 8;
const TRUNCATED_FILE_NOTE: &str = "\n\n[File mention truncated to fit context limits.]";

/// One skill a user may name, with the body an invocation appends.
struct InvocableSkill {
    name: String,
    user_invocable: bool,
    payload: String,
}

impl AppServer {
    /// The input a turn or a steer hands the harness, with the display
    /// content that records the skills it invoked.
    pub(crate) fn prepare_unified_input(
        &self,
        unified: &UnifiedSession,
        content: Vec<Value>,
        display: Option<Value>,
        inject_skill: bool,
    ) -> Result<(Vec<Value>, Option<Value>), ProtocolFault> {
        let display = display
            .map(|display| invocation::without_display(&display))
            .filter(|display| !display.is_null());
        let content = self
            .harness_images(unified, content)
            .map_err(|message| ProtocolFault::plain(ProtocolErrorCode::InvalidParams, message))?;
        let text = text_of(&content);
        let invoked = if inject_skill {
            self.invoked_skill_blocks(unified, &text)
        } else {
            Vec::new()
        };
        let scratchpad = {
            let core = unified.core();
            vibe_harness::tools::vibe::scratchpad_block_to_restate(
                &core.vibe.scratchpad,
                &core.projector.state.entries,
            )
        };
        let mut content = content;
        content.extend(
            mentioned_file_blocks(&text, Path::new(&unified.cwd)).map_err(|message| {
                ProtocolFault::plain(ProtocolErrorCode::InvalidParams, message)
            })?,
        );
        let names: Vec<String> = invoked.iter().map(|(name, _)| name.clone()).collect();
        content.extend(
            invoked
                .into_iter()
                .map(|(_, body)| json!({"type": "text", "text": body})),
        );
        if let Some(block) = scratchpad {
            content.push(json!({"type": "text", "text": block}));
        }
        let display = invocation::with_display(display, &names);
        vibe_harness::loops::prepare_loop_content(&mut content);
        Ok((content, display))
    }

    /// Reference `SessionImageDescriber.described_blocks`: when the session's
    /// model reads no image and another of its configuration does, each
    /// image becomes the description that model gives of it. Answers the
    /// blocks with the warning frames of the images left undescribed.
    pub(crate) fn describe_unified_images(
        &self,
        unified: &UnifiedSession,
        content: Vec<Value>,
    ) -> Result<(Vec<Value>, Vec<Vec<u8>>), ProtocolFault> {
        let count = content
            .iter()
            .filter(|block| block["type"] == "image")
            .count();
        if count == 0 {
            return Ok((content, Vec::new()));
        }
        if count > MAX_IMAGES_PER_MESSAGE {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::InvalidParams,
                format!("Too many image attachments (got {count}, max {MAX_IMAGES_PER_MESSAGE})."),
            ));
        }
        let Some(route) = self.unified_vision(unified) else {
            return Ok((content, Vec::new()));
        };
        let images: Vec<Option<Result<ImageToDescribe, String>>> = content
            .iter()
            .map(|block| {
                (block["type"] == "image").then(|| image_to_describe(&block["attachment"]))
            })
            .collect();
        let instruction = content
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let readable: Vec<ImageToDescribe> = images
            .iter()
            .flatten()
            .filter_map(|image| image.as_ref().ok())
            .cloned()
            .collect();
        let mut failures = crate::plugins::block_on_own_runtime(unified.describer.describe_all(
            &readable,
            &route,
            &instruction,
        ))
        .unwrap_or_default();
        for (block, image) in content.iter().zip(&images) {
            if let Some(Err(reason)) = image {
                failures.push(vibe_core::vision::FailedDescription {
                    alias: block["attachment"]["alias"]
                        .as_str()
                        .unwrap_or("image")
                        .to_owned(),
                    reason: reason.clone(),
                });
            }
        }
        let frames = failures
            .iter()
            .map(|failure| {
                super::encode_notification(
                    "warning",
                    super::result_map([(
                        "warning",
                        json!({
                            "code": "warning",
                            "message": format!(
                                "{} could not describe {}: {}",
                                route.model.alias, failure.alias, failure.reason
                            ),
                            "details": null,
                        }),
                    )]),
                )
            })
            .collect();
        let described = content
            .into_iter()
            .zip(images)
            .map(|(block, image)| match image {
                Some(Ok(image)) => json!({"type": "text", "text": unified.describer.image_text(&image)}),
                Some(Err(_)) => {
                    let alias = block["attachment"]["alias"].as_str().unwrap_or("image");
                    json!({
                        "type": "text",
                        "text": format!("<image alias=\"{alias}\">\n{}\n</image>", vibe_core::vision::UNREADABLE_IMAGE),
                    })
                }
                None => block,
            })
            .collect();
        Ok((described, frames))
    }

    /// The model that describes images to `unified`'s model, when it needs one.
    fn unified_vision(&self, unified: &UnifiedSession) -> Option<vibe_core::vision::VisionRoute> {
        let scope = {
            let sessions = self.lock_sessions();
            crate::server::turns::prompt_scope(sessions.get(&unified.id)?, None)
        };
        let (model, provider, api) = self.workspace.session_vision(&scope)?;
        Some(vibe_core::vision::VisionRoute {
            model,
            provider,
            context: vibe_core::llm::BackendContext::ambient(api, self.workspace.credentials()),
            metadata: vibe_core::vision::request_metadata(Some(&unified.id)),
        })
    }

    /// Reference `_rust_content_blocks` for images: each becomes the harness's
    /// own block, its bytes inline; one read from a file keeps the link a
    /// model that reads no image is handed instead.
    fn harness_images(
        &self,
        unified: &UnifiedSession,
        content: Vec<Value>,
    ) -> Result<Vec<Value>, String> {
        if !content.iter().any(|block| block["type"] == "image") {
            return Ok(content);
        }
        let mut roots = vec![
            vibe_core::platform::canonicalize(&unified.cwd)
                .unwrap_or_else(|_| PathBuf::from(&unified.cwd)),
        ];
        if let Some(dir) = unified
            .host()
            .store
            .as_ref()
            .map(|store| store.dir().to_path_buf())
        {
            roots.push(dir);
        }
        content
            .into_iter()
            .map(|block| {
                if block["type"] != "image" || block.get("attachment").is_none() {
                    return Ok(block);
                }
                harness_image(&block["attachment"], &roots)
            })
            .collect()
    }

    /// Reference `_invoked_skill_blocks`: each skill the prompt invokes, as its
    /// body or, when the conversation already carries it, the note saying so.
    fn invoked_skill_blocks(&self, unified: &UnifiedSession, text: &str) -> Vec<(String, String)> {
        let resolved = self.resolved_invoked_skills(unified, text);
        if resolved.is_empty() {
            return resolved;
        }
        let history: Vec<Value> = unified
            .core()
            .projector
            .state
            .entries
            .iter()
            .flat_map(vibe_harness::wire::wire_entries)
            .collect();
        resolved
            .into_iter()
            .map(|(name, body)| {
                if history.iter().any(|entry| loaded_skill(entry, &name)) {
                    let note = vibe_core::skills::skill_already_loaded_message(&name);
                    (name, note)
                } else {
                    (name, body)
                }
            })
            .collect()
    }

    /// Reference `_resolved_invoked_skills`: the name and body of each skill
    /// the prompt invokes, in prompt order. Unknown names, skills the user
    /// may not invoke, and `/loop` prompts invoke nothing.
    pub(super) fn resolved_invoked_skills(
        &self,
        unified: &UnifiedSession,
        text: &str,
    ) -> Vec<(String, String)> {
        if vibe_harness::loops::is_loop_prompt(text) {
            return Vec::new();
        }
        let skills = self.unified_invocable_skills(&unified.id);
        invocation::invoked_skill_names(text)
            .iter()
            .filter_map(|typed| {
                skills
                    .iter()
                    .find(|skill| skill.name.to_lowercase() == *typed)
                    .filter(|skill| skill.user_invocable)
                    .map(|skill| (skill.name.clone(), skill.payload.clone()))
            })
            .collect()
    }

    /// Every skill the session lists: its plugins', then its own roots'.
    fn unified_invocable_skills(&self, session_id: &str) -> Vec<InvocableSkill> {
        let mut skills: Vec<InvocableSkill> = self
            .session_plugins(session_id)
            .map(|plugins| {
                plugins
                    .runtime_skills
                    .iter()
                    .map(|(alias, skill)| InvocableSkill {
                        name: alias.clone(),
                        user_invocable: skill.user_invocable,
                        payload: vibe_core::tools::builtins::skill_payload(skill, alias),
                    })
                    .collect()
            })
            .unwrap_or_default();
        for skill in self.workspace.unified_root_skills() {
            if skills.iter().any(|known| known.name == skill.name) {
                continue;
            }
            skills.push(InvocableSkill {
                name: skill.name.clone(),
                user_invocable: skill.user_invocable,
                payload: vibe_core::tools::builtins::skill_payload(&skill, &skill.name),
            });
        }
        skills
    }
}

/// Reference `MAX_IMAGES_PER_MESSAGE`.
const MAX_IMAGES_PER_MESSAGE: usize = 8;
/// Reference `_MAX_IMAGE_BYTES` (the harness's session).
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// One public image attachment as the describer takes it, or why its bytes
/// could not be read.
fn image_to_describe(attachment: &Value) -> Result<ImageToDescribe, String> {
    let alias = attachment["alias"].as_str().unwrap_or("image");
    let mime_type = attachment["mimeType"].as_str().unwrap_or_default();
    let source = &attachment["source"];
    match source["kind"].as_str() {
        Some("file") => {
            let path = PathBuf::from(source["path"].as_str().unwrap_or_default());
            let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
            Ok(ImageToDescribe::file(
                alias,
                mime_type,
                &BASE64.encode(bytes),
                &path,
            ))
        }
        _ => Ok(ImageToDescribe::inline(
            alias,
            mime_type,
            source["data"].as_str().unwrap_or_default(),
        )),
    }
}

/// One public image attachment as the harness holds it.
fn harness_image(attachment: &Value, roots: &[PathBuf]) -> Result<Value, String> {
    let mime_type = attachment["mimeType"].clone();
    let source = &attachment["source"];
    match source["kind"].as_str() {
        Some("inline") => {
            let data = source["data"].as_str().unwrap_or_default();
            let raw = BASE64
                .decode(data)
                .map_err(|_| "Unified Harness image data is not valid base64".to_owned())?;
            check_image_size(raw.len())?;
            Ok(json!({"type": "image", "data": data, "mimeType": mime_type}))
        }
        Some("file") => {
            let path = PathBuf::from(source["path"].as_str().unwrap_or_default());
            let resolved =
                vibe_core::platform::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if !roots.iter().any(|root| resolved.starts_with(root)) {
                return Err(format!(
                    "Image file is outside the workspace or session attachments: {}",
                    resolved.display()
                ));
            }
            let raw = std::fs::read(&resolved).map_err(|error| {
                format!("Failed to read image file {}: {error}", resolved.display())
            })?;
            check_image_size(raw.len())?;
            let uri = file_uri(&resolved);
            let name = attachment["alias"]
                .as_str()
                .filter(|alias| !alias.is_empty())
                .map(ToOwned::to_owned)
                .or_else(|| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| uri.clone());
            Ok(json!({
                "type": "image",
                "data": BASE64.encode(&raw),
                "mimeType": mime_type,
                "_meta": {vibe_harness::engine::FILE_IMAGE_LINK: {
                    "uri": uri,
                    "name": name,
                    "mimeType": mime_type,
                    "size": raw.len(),
                }},
            }))
        }
        other => Err(format!(
            "Unified Harness message input does not support image source kind: {}",
            other.map_or_else(|| "None".to_owned(), |kind| format!("'{kind}'"))
        )),
    }
}

fn check_image_size(size: usize) -> Result<(), String> {
    if size > MAX_IMAGE_BYTES {
        return Err(format!("Image is too large: {size} > {MAX_IMAGE_BYTES}"));
    }
    Ok(())
}

/// The text blocks of `content`, joined as the reference joins them.
fn text_of(content: &[Value]) -> String {
    content
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Reference `_entry_loaded_skill`: a public entry that carries the body of
/// `name`, typed into a message or loaded by the `skill` tool.
fn loaded_skill(entry: &Value, name: &str) -> bool {
    let marker = vibe_core::skills::skill_content_marker(name);
    if entry["type"] == "message" {
        return entry["content"]
            .as_array()
            .is_some_and(|blocks| text_of(blocks).contains(&marker));
    }
    entry["type"] == "effect"
        && entry["detail"]["kind"] == "skill"
        && entry["detail"]["input"]["name"] == name
        && entry["state"]["status"] == "completed"
        && entry["state"]["outputText"]
            .as_str()
            .is_some_and(|output| output.contains(&marker))
}

/// Reference `mentioned_file_content_blocks_async`: each file the message
/// mentions inside the session's root, inlined as a resource.
pub(super) fn mentioned_file_blocks(text: &str, cwd: &Path) -> Result<Vec<Value>, String> {
    let root = vibe_core::platform::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let payload = vibe_core::path_resources::build_path_prompt_payload(&root, text);
    let files: Vec<_> = payload
        .resources
        .iter()
        .filter(|resource| resource.kind == vibe_core::path_resources::PathResourceKind::File)
        .filter(|resource| {
            vibe_core::platform::canonicalize(&resource.path)
                .unwrap_or_else(|_| resource.path.clone())
                .starts_with(&root)
        })
        .collect();
    if files.len() > MENTIONED_FILE_MAX_FILES {
        return Err(format!(
            "Too many file mentions: {MENTIONED_FILE_MAX_FILES} maximum"
        ));
    }
    files
        .into_iter()
        .map(|resource| {
            let (lines, truncated) = read_lines_bounded(&resource.path)
                .map_err(|error| format!("Failed to attach file {}: {error}", resource.alias))?;
            let mut text = lines.join("\n");
            if truncated {
                text.push_str(TRUNCATED_FILE_NOTE);
            }
            Ok(json!({
                "type": "resource",
                "resource": {
                    "uri": file_uri(&resource.path),
                    "mediaType": null,
                    "kind": "text",
                    "text": text,
                },
            }))
        })
        .collect()
}

/// Reference `read_lines_safe`: up to the line limit, cut at the byte limit,
/// with whether anything was left out.
fn read_lines_bounded(path: &Path) -> std::io::Result<(Vec<String>, bool)> {
    let bytes = std::fs::read(path)?;
    let mut kept: Vec<u8> = Vec::new();
    let mut truncated = false;
    for (lines, line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        if lines >= MENTIONED_FILE_LINE_LIMIT {
            truncated = true;
            break;
        }
        if kept.len() + line.len() > MENTIONED_FILE_MAX_BYTES {
            let remaining = MENTIONED_FILE_MAX_BYTES - kept.len();
            kept.extend_from_slice(&line[..remaining]);
            truncated = true;
            break;
        }
        kept.extend_from_slice(line);
    }
    let text = String::from_utf8_lossy(&kept)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    Ok((text.lines().map(ToOwned::to_owned).collect(), truncated))
}

/// `Path.as_uri()`: a prefix, then the path with its bytes outside the
/// unreserved set and `/` percent-encoded. A POSIX path takes `file://`. On
/// Windows the path is written with forward slashes: a drive path takes
/// `file:///C:` and keeps the rest, any other prefixed path (UNC) takes
/// `file:`.
fn file_uri(path: &Path) -> String {
    let (prefix, rest) = uri_parts(path);
    format!("{prefix}{}", vibe_core::pyurl::quote(&rest, "/"))
}

#[cfg(not(windows))]
fn uri_parts(path: &Path) -> (String, String) {
    ("file://".to_owned(), path.to_string_lossy().into_owned())
}

#[cfg(windows)]
fn uri_parts(path: &Path) -> (String, String) {
    use std::path::{Component, Prefix};

    let posix = path.to_string_lossy().replace('\\', "/");
    match path.components().next() {
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_)) => {
            let rest = posix.get(2..).unwrap_or_default().to_owned();
            let drive = posix.get(..2).unwrap_or_default();
            (format!("file:///{drive}"), rest)
        }
        Some(Component::Prefix(_)) => ("file:".to_owned(), posix),
        _ => ("file://".to_owned(), path.to_string_lossy().into_owned()),
    }
}

#[cfg(test)]
mod prepare_tests;
