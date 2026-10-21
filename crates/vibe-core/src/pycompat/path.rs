//! The path normalizations of `posixpath` and `ntpath`, which work on the
//! spelling alone: the reference compares paths it never resolves, a pattern
//! from an approval or a command from a plugin manifest, so the answer must not
//! depend on the host or the filesystem the way [`std::path`] does.

/// Python `ntpath.splitroot` over a path whose separators are backslashes.
pub(crate) fn windows_splitroot(path: &str) -> (&str, &str, &str) {
    if let Some(rest) = path.strip_prefix('\\') {
        if !rest.starts_with('\\') {
            return ("", "\\", rest);
        }
        // A UNC or device drive: `\\server\share`, or `\\?\UNC\server\share`.
        let start = if path
            .get(..8)
            .is_some_and(|head| head.eq_ignore_ascii_case("\\\\?\\UNC\\"))
        {
            8
        } else {
            2
        };
        let Some(index) = path.get(start..).and_then(|tail| tail.find('\\')) else {
            return (path, "", "");
        };
        let index = start.saturating_add(index);
        let Some(second) = path
            .get(index.saturating_add(1)..)
            .and_then(|tail| tail.find('\\'))
        else {
            return (path, "", "");
        };
        let second = index.saturating_add(1).saturating_add(second);
        return (
            path.get(..second).unwrap_or(path),
            path.get(second..second.saturating_add(1)).unwrap_or(""),
            path.get(second.saturating_add(1)..).unwrap_or(""),
        );
    }
    let mut characters = path.char_indices();
    if let (Some(_), Some((colon, ':'))) = (characters.next(), characters.next()) {
        let drive_end = colon.saturating_add(1);
        let drive = path.get(..drive_end).unwrap_or("");
        let rest = path.get(drive_end..).unwrap_or("");
        return match rest.strip_prefix('\\') {
            Some(tail) => (drive, "\\", tail),
            None => (drive, "", rest),
        };
    }
    ("", "", path)
}

/// Python `ntpath.normpath`.
pub(crate) fn windows_normpath(path: &str) -> String {
    let path = path.replace('/', "\\");
    let (drive, root, tail) = windows_splitroot(&path);
    let mut components: Vec<&str> = Vec::new();
    for component in tail.split('\\') {
        match component {
            "" | "." => {}
            ".." => match components.last() {
                Some(&last) if last != ".." => {
                    components.pop();
                }
                None if !root.is_empty() => {}
                _ => components.push(component),
            },
            _ => components.push(component),
        }
    }
    let prefix = format!("{drive}{root}");
    if prefix.is_empty() && components.is_empty() {
        return ".".to_owned();
    }
    format!("{prefix}{}", components.join("\\"))
}

/// Python `posixpath.normpath`.
pub(crate) fn posix_normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let mut components: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." if initial.is_empty() && components.last().is_none_or(|last| *last == "..") => {
                components.push(component);
            }
            ".." => {
                components.pop();
            }
            _ => components.push(component),
        }
    }
    let normalized = format!("{initial}{}", components.join("/"));
    if normalized.is_empty() {
        ".".to_owned()
    } else {
        normalized
    }
}
