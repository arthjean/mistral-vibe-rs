//! The `file_system` built-ins: reading, writing and editing text files.
//!
//! Reference `harness/runtimes/python/python/mistralai_vibe_local_harness/vibe/_file_tools.py`:
//! paths resolve against the session's working directory and must stay inside
//! its roots unless approvals are bypassed on the host shell, text decodes
//! from a byte-order mark, then UTF-8, cp1252 and Latin-1, a read stops at its
//! line limit or 50 KiB, and every write goes through a temporary file in the
//! same directory.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::state::ToolResult;

const SNIFF_BYTES: usize = 4096;
/// The page a read takes when the call names none.
pub const DEFAULT_LINE_LIMIT: u64 = 2000;
const MAX_READ_BYTES: usize = 50 * 1024;
const MAX_WRITE_BYTES: usize = 64_000;
const MAX_WRITE_PREVIOUS_CONTENT_BYTES: u64 = 64_000;
const MAX_EDIT_FILE_SIZE_BYTES: u64 = 512 * 1024 * 1024;
const SEARCH_REPLACE_ANNOTATION: &str = "mistralai.vibe.sdk.search_replace";
const WRITE_FILE_ANNOTATION: &str = "mistralai.vibe.sdk.write_file";

/// Where file tools may act.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub cwd: PathBuf,
    pub roots: Vec<PathBuf>,
    /// Whether the roots bound a path: not when approvals are bypassed on the
    /// host shell, where a command could read the file anyway.
    pub bounded: bool,
}

/// A tool failure as the runtime reports it.
fn failed(message: impl Into<String>) -> ToolResult {
    ToolResult::failed("tool_failed", message)
}

fn succeeded(structured: Value, meta: Option<Map<String, Value>>) -> ToolResult {
    ToolResult::Success {
        content: Vec::new(),
        structured_content: Some(structured),
        meta,
    }
}

/// Runs one `file_system` call.
#[must_use]
pub fn execute(name: &str, arguments: &Value, workspace: &Workspace) -> ToolResult {
    let outcome = match name {
        "file_system.read_file" => read_file(arguments, workspace),
        "file_system.write_file" => write_file(arguments, workspace),
        "file_system.search_replace" => search_replace(arguments, workspace),
        other => Err(format!("Unsupported file tool: {other}")),
    };
    outcome.unwrap_or_else(failed)
}

fn string_argument<'a>(arguments: &'a Value, key: &str) -> Result<&'a str, String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key}: Field required"))
}

/// Reference `_resolve_file_path`.
pub fn resolve_path(raw: &str, workspace: &Workspace) -> Result<PathBuf, String> {
    if raw.trim().is_empty() {
        return Err("Path cannot be empty".to_owned());
    }
    let expanded = expand_user(raw);
    let path = if expanded.is_absolute() {
        expanded
    } else {
        workspace.cwd.join(expanded)
    };
    let resolved = resolve_lexically(&path);
    if !workspace.bounded {
        return Ok(resolved);
    }
    let inside = workspace
        .roots
        .iter()
        .map(|root| resolve_lexically(&expand_user(&root.to_string_lossy())))
        .any(|root| resolved.starts_with(root));
    if !inside {
        return Err(format!(
            "Path is outside the workspace: {}",
            resolved.display()
        ));
    }
    Ok(resolved)
}

fn expand_user(raw: &str) -> PathBuf {
    if (raw == "~" || raw.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(raw.trim_start_matches('~').trim_start_matches('/'));
    }
    PathBuf::from(raw)
}

/// `Path.resolve()`: symlinks followed as far as the path exists, `..`
/// folded lexically past that.
fn resolve_lexically(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut out = fs::canonicalize(&existing).unwrap_or(existing);
    for part in rest.into_iter().rev() {
        if part == ".." {
            out.pop();
        } else if part != "." {
            out.push(part);
        }
    }
    out
}

// --------------------------------------------------------------------------
// Decoding
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Utf8Sig,
    Utf16,
    Utf32,
    Utf8,
    Cp1252,
    Latin1,
}

fn candidate_encodings(raw: &[u8]) -> Vec<Encoding> {
    let bom = if raw.starts_with(b"\xef\xbb\xbf") {
        Some(Encoding::Utf8Sig)
    } else if raw.starts_with(b"\xff\xfe\x00\x00") || raw.starts_with(b"\x00\x00\xfe\xff") {
        Some(Encoding::Utf32)
    } else if raw.starts_with(b"\xff\xfe") || raw.starts_with(b"\xfe\xff") {
        Some(Encoding::Utf16)
    } else {
        None
    };
    let mut out: Vec<Encoding> = bom.into_iter().collect();
    for encoding in [Encoding::Utf8, Encoding::Cp1252, Encoding::Latin1] {
        if !out.contains(&encoding) {
            out.push(encoding);
        }
    }
    out
}

/// The cp1252 bytes Python's codec leaves undefined.
const CP1252_UNDEFINED: [u8; 5] = [0x81, 0x8d, 0x8f, 0x90, 0x9d];

const CP1252_HIGH: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

fn decode(raw: &[u8], encoding: Encoding) -> Option<String> {
    match encoding {
        Encoding::Utf8 => String::from_utf8(raw.to_vec()).ok(),
        Encoding::Utf8Sig => {
            String::from_utf8(raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw).to_vec()).ok()
        }
        Encoding::Utf16 => {
            let (body, big) = if let Some(rest) = raw.strip_prefix(b"\xfe\xff") {
                (rest, true)
            } else {
                (raw.strip_prefix(b"\xff\xfe").unwrap_or(raw), false)
            };
            if body.len() % 2 != 0 {
                return None;
            }
            let units: Vec<u16> = body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| {
                    if big {
                        u16::from_be_bytes([pair[0], pair[1]])
                    } else {
                        u16::from_le_bytes([pair[0], pair[1]])
                    }
                })
                .collect();
            String::from_utf16(&units).ok()
        }
        Encoding::Utf32 => {
            let (body, big) = if let Some(rest) = raw.strip_prefix(b"\x00\x00\xfe\xff") {
                (rest, true)
            } else {
                (raw.strip_prefix(b"\xff\xfe\x00\x00").unwrap_or(raw), false)
            };
            if body.len() % 4 != 0 {
                return None;
            }
            body.as_chunks::<4>()
                .0
                .iter()
                .map(|quad| {
                    let bytes = [quad[0], quad[1], quad[2], quad[3]];
                    char::from_u32(if big {
                        u32::from_be_bytes(bytes)
                    } else {
                        u32::from_le_bytes(bytes)
                    })
                })
                .collect()
        }
        Encoding::Cp1252 => raw
            .iter()
            .map(|byte| match byte {
                0x80..=0x9f if CP1252_UNDEFINED.contains(byte) => None,
                0x80..=0x9f => Some(CP1252_HIGH[usize::from(byte - 0x80)]),
                other => Some(char::from(*other)),
            })
            .collect(),
        Encoding::Latin1 => Some(raw.iter().map(|byte| char::from(*byte)).collect()),
    }
}

fn encode(text: &str, encoding: Encoding) -> Option<Vec<u8>> {
    match encoding {
        Encoding::Utf8 => Some(text.as_bytes().to_vec()),
        Encoding::Utf8Sig => {
            let mut out = b"\xef\xbb\xbf".to_vec();
            out.extend_from_slice(text.as_bytes());
            Some(out)
        }
        Encoding::Utf16 => {
            let mut out = b"\xff\xfe".to_vec();
            for unit in text.encode_utf16() {
                out.extend_from_slice(&unit.to_le_bytes());
            }
            Some(out)
        }
        Encoding::Utf32 => {
            let mut out = b"\xff\xfe\x00\x00".to_vec();
            for character in text.chars() {
                out.extend_from_slice(&u32::from(character).to_le_bytes());
            }
            Some(out)
        }
        Encoding::Cp1252 => text
            .chars()
            .map(|character| {
                let code = u32::from(character);
                if code < 0x80 || (0xa0..=0xff).contains(&code) {
                    u8::try_from(code).ok()
                } else {
                    CP1252_HIGH
                        .iter()
                        .position(|candidate| *candidate == character)
                        .filter(|index| !CP1252_UNDEFINED.contains(&(0x80 + *index as u8)))
                        .map(|index| 0x80 + index as u8)
                }
            })
            .collect(),
        Encoding::Latin1 => text
            .chars()
            .map(|character| u8::try_from(u32::from(character)).ok())
            .collect(),
    }
}

/// `str.splitlines(keepends=True)`: Python's line boundaries.
fn split_lines_keep(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        let end = match character {
            '\r' => {
                if let Some((_, '\n')) = chars.peek() {
                    chars.next();
                    index + 2
                } else {
                    index + 1
                }
            }
            '\n' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}'
            | '\u{2029}' => index + character.len_utf8(),
            _ => continue,
        };
        lines.push(&text[start..end]);
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Lines as a text-mode reader with `newline=""` yields them: split after
/// `\n`, `\r` or `\r\n` only.
fn reader_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                lines.push(&text[start..=index]);
                start = index + 1;
            }
            b'\r' => {
                let end = if bytes.get(index + 1) == Some(&b'\n') {
                    index + 1
                } else {
                    index
                };
                lines.push(&text[start..=end]);
                start = end + 1;
                index = end;
            }
            _ => {}
        }
        index += 1;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

fn open_regular(path: &Path) -> Result<(fs::File, fs::Metadata), String> {
    let link = fs::symlink_metadata(path);
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("File not found at: {}", path.display()));
        }
        Err(error) => return Err(os_error(&error, path)),
    };
    if link.is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(format!(
            "[Errno 40] Too many levels of symbolic links: '{}'",
            path.display()
        ));
    }
    let metadata = file.metadata().map_err(|error| os_error(&error, path))?;
    if metadata.is_dir() {
        return Err(format!(
            "Path is a directory, not a file: {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!("Path is not a regular file: {}", path.display()));
    }
    Ok((file, metadata))
}

/// An `OSError` as Python renders it.
fn os_error(error: &std::io::Error, path: &Path) -> String {
    let code = error.raw_os_error().unwrap_or(0);
    let text = error.to_string();
    let text = text.split(" (os error").next().unwrap_or(&text).to_owned();
    format!("[Errno {code}] {text}: '{}'", path.display())
}

// --------------------------------------------------------------------------
// The three tools
// --------------------------------------------------------------------------

fn read_file(arguments: &Value, workspace: &Workspace) -> Result<ToolResult, String> {
    let raw_path = string_argument(arguments, "path")?;
    let offset = match arguments.get("offset") {
        None => 0,
        Some(value) => value
            .as_i64()
            .ok_or("offset: Input should be a valid integer")?,
    };
    let limit = match arguments.get("limit") {
        None => Some(DEFAULT_LINE_LIMIT),
        Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|limit| *limit >= 1)
                .ok_or("limit: Input should be greater than or equal to 1")?,
        ),
    };
    let path = resolve_path(raw_path, workspace)?;
    let (mut file, metadata) = open_regular(&path)?;
    let mut raw = Vec::new();
    file.read_to_end(&mut raw)
        .map_err(|error| os_error(&error, &path))?;
    let prefix = &raw[..raw.len().min(SNIFF_BYTES)];
    for encoding in candidate_encodings(prefix) {
        let Some(text) = decode(&raw, encoding) else {
            continue;
        };
        let lines = reader_lines(&text);
        let start = if offset >= 0 {
            usize::try_from(offset).unwrap_or(usize::MAX)
        } else if offset == -1 {
            lines.len().saturating_sub(1)
        } else {
            return Err(
                "offset must be greater than or equal to 0, or -1 to read the last line".to_owned(),
            );
        };
        let (content, truncated) = read_page(&lines, start, limit);
        let returned = content.len();
        let lines_read = split_lines_keep(&content).len();
        return Ok(succeeded(
            json!({
                "path": path.to_string_lossy(),
                "content": content,
                "file_size_bytes": metadata.len(),
                "returned_bytes": returned,
                "offset": start,
                "lines_read": lines_read,
                "was_truncated": truncated,
            }),
            None,
        ));
    }
    Err(format!(
        "Could not decode text file with supported encodings: {}",
        path.display()
    ))
}

fn read_page(lines: &[&str], offset: usize, limit: Option<u64>) -> (String, bool) {
    let mut out = String::new();
    let mut yielded: u64 = 0;
    for line in lines.iter().skip(offset) {
        if limit.is_some_and(|limit| yielded >= limit) {
            return (out, true);
        }
        if out.len() + line.len() <= MAX_READ_BYTES {
            out.push_str(line);
            yielded += 1;
            continue;
        }
        let remaining = MAX_READ_BYTES - out.len();
        let mut cut = remaining.min(line.len());
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push_str(&line[..cut]);
        return (out, true);
    }
    (out, false)
}

fn write_file(arguments: &Value, workspace: &Workspace) -> Result<ToolResult, String> {
    let raw_path = string_argument(arguments, "path")?;
    let content = string_argument(arguments, "content")?;
    if let Some(extra) = arguments.as_object().and_then(|object| {
        object
            .keys()
            .find(|key| *key != "path" && *key != "content")
    }) {
        return Err(format!("{extra}: Extra inputs are not permitted"));
    }
    let path = resolve_path(raw_path, workspace)?;
    if content.len() > MAX_WRITE_BYTES {
        return Err(format!("Content exceeds {MAX_WRITE_BYTES} bytes limit"));
    }
    if path.is_dir() {
        return Err(format!(
            "Path is a directory, not a file: {}",
            path.display()
        ));
    }
    let existed = path.exists();
    let previous = existed
        .then(|| {
            let metadata = fs::metadata(&path).ok()?;
            if metadata.len() > MAX_WRITE_PREVIOUS_CONTENT_BYTES {
                return None;
            }
            fs::read_to_string(&path).ok()
        })
        .flatten();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| os_error(&error, parent))?;
    }
    atomic_write(&path, content.as_bytes())?;
    let meta = previous.map(|previous| {
        let mut meta = Map::new();
        meta.insert(
            WRITE_FILE_ANNOTATION.to_owned(),
            json!({"previous_content": previous}),
        );
        meta
    });
    Ok(succeeded(
        json!({
            "path": path.to_string_lossy(),
            "bytes_written": content.len(),
            "file_existed": existed,
        }),
        meta,
    ))
}

fn looks_binary(text: &str, raw: &[u8], encoding: Encoding) -> bool {
    if raw.contains(&0) && !matches!(encoding, Encoding::Utf16 | Encoding::Utf32) {
        return true;
    }
    text.chars().take(SNIFF_BYTES).any(|character| {
        !"\t\n\r\u{0b}\u{0c}\u{1c}\u{1d}\u{1e}\u{85}".contains(character)
            && (u32::from(character) < 0x20 || (0x7f..=0x9f).contains(&u32::from(character)))
    })
}

fn search_replace(arguments: &Value, workspace: &Workspace) -> Result<ToolResult, String> {
    let raw_path = string_argument(arguments, "file_path")?.trim();
    if raw_path.is_empty() {
        return Err("file_path: String should have at least 1 character".to_owned());
    }
    let blocks = arguments
        .get("content")
        .and_then(Value::as_array)
        .filter(|blocks| !blocks.is_empty())
        .ok_or("content: List should have at least 1 item after validation, not 0")?;
    let path = resolve_path(raw_path, workspace)?;
    let (mut file, metadata) = open_regular(&path)?;
    if metadata.len() > MAX_EDIT_FILE_SIZE_BYTES {
        return Err(format!(
            "File exceeds {MAX_EDIT_FILE_SIZE_BYTES} byte edit limit: {}",
            path.display()
        ));
    }
    let mut raw = Vec::new();
    file.read_to_end(&mut raw)
        .map_err(|error| os_error(&error, &path))?;
    drop(file);
    let prefix = &raw[..raw.len().min(SNIFF_BYTES)];
    let mut decoded = None;
    for encoding in candidate_encodings(prefix) {
        if let Some(text) = decode(&raw, encoding) {
            if looks_binary(&text, &raw, encoding) {
                return Err(format!(
                    "Binary files are not supported: {}",
                    path.display()
                ));
            }
            decoded = Some((text, encoding));
            break;
        }
    }
    let (original, encoding) = decoded.ok_or_else(|| {
        format!(
            "Could not decode text file with supported encodings: {}",
            path.display()
        )
    })?;
    let mut updated = original.clone();
    let mut previews = Vec::new();
    let mut lines_changed = 0usize;
    let mut warnings = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let old = block["old_str"].as_str().unwrap_or_default();
        let new = block["new_str"].as_str().unwrap_or_default();
        let replace_all = block["replace_all"].as_bool().unwrap_or(false);
        if old == new {
            return Err(format!("block {index}: old_str and new_str must differ"));
        }
        let matches = updated.matches(old).count();
        if matches == 0 {
            return Err(format!(
                "block {index}: old_str not found in {}",
                path.display()
            ));
        }
        if matches > 1 && !replace_all {
            return Err(format!(
                "block {index}: old_str is not unique; found {matches} matches in {}",
                path.display()
            ));
        }
        let count = if replace_all { matches } else { 1 };
        let old_lines = split_lines_keep(old);
        let new_lines = split_lines_keep(new);
        let mut delta: i64 = 0;
        for start in updated
            .match_indices(old)
            .take(count)
            .map(|(start, _)| start)
        {
            let old_start = updated[..start].matches('\n').count() + 1;
            previews.push(json!({
                "old_start_line": old_start,
                "new_start_line": i64::try_from(old_start).unwrap_or(0) + delta,
                "old_lines": old_lines,
                "new_lines": new_lines,
            }));
            delta += i64::try_from(new_lines.len()).unwrap_or(0)
                - i64::try_from(old_lines.len()).unwrap_or(0);
        }
        lines_changed += old_lines.len().max(new_lines.len()) * count;
        updated = updated.replacen(old, new, count);
    }
    if updated == original {
        warnings.push("search/replace blocks leave the file unchanged".to_owned());
    } else {
        let bytes = encode(&updated, encoding)
            .ok_or_else(|| format!("'{encoding:?}' codec can't encode the edited text"))?;
        atomic_write(&path, &bytes)?;
    }
    let mut meta = Map::new();
    meta.insert(
        SEARCH_REPLACE_ANNOTATION.to_owned(),
        json!({"blocks": previews}),
    );
    Ok(succeeded(
        json!({
            "file": path.to_string_lossy(),
            "lines_changed": lines_changed,
            "warnings": warnings,
        }),
        Some(meta),
    ))
}

/// Reference `_atomic_write_text`: a temporary sibling, synced, then renamed
/// over the target with the target's permissions.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|error| os_error(&error, parent))?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| os_error(&error, path))?;
    if let Ok(metadata) = fs::metadata(path) {
        let _ = fs::set_permissions(temporary.path(), metadata.permissions());
    }
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| os_error(&error.error, path))
}
