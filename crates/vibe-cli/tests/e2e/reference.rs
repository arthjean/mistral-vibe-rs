//! The pinned reference as an e2e subject. The `vibe-acp` battery compiles
//! this module too, through a `#[path]` attribute.

use std::path::PathBuf;

/// The pinned checkout with its environment, or why it cannot be the subject.
pub fn pinned() -> Result<PathBuf, String> {
    let root = vibe_core::parity::reference_root();
    if let Some(reason) = vibe_core::parity::off_pin_reason(&root, "e2e reference subject") {
        // The reason is worded for a live probe that skips; keep what it found.
        let found = reason
            .split_once("probe: ")
            .map_or(reason.as_str(), |(_, found)| found);
        return Err(found.to_owned());
    }
    if vibe_core::parity::pinned_interpreter(&root).is_none() {
        return Err(format!(
            "{} has no virtual environment; run `uv sync --frozen` there",
            root.display()
        ));
    }
    Ok(root)
}
