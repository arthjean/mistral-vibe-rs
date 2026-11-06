//! The pinned reference as an e2e subject. The `vibe-acp` battery compiles
//! this module too, through a `#[path]` attribute.

use std::path::PathBuf;

/// The attested environment of the pin, laid out like a checkout with its
/// `.venv`, or why it cannot be the subject.
pub fn pinned() -> Result<PathBuf, String> {
    let root = vibe_core::parity::live::pinned_environment().map_err(|refusal| refusal.detail)?;
    if vibe_core::parity::pinned_interpreter(&root).is_none() {
        return Err(format!(
            "{} has no virtual environment; rerun `{} {}`",
            root.display(),
            vibe_core::parity::live::MATERIALIZE,
            vibe_core::parity::REFERENCE_COMMIT
        ));
    }
    Ok(root)
}
