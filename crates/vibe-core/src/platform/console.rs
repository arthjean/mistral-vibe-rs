//! Console windows for the children this process starts, and the environment it
//! hands them.
//!
//! On Windows a console program started by a process that has no console gets
//! a new one, window included. An editor that starts `vibe-acp` detached would
//! see a window flash for every shell, MCP server, or git call, so a child
//! started in the background takes `CREATE_NO_WINDOW` there. A process that
//! has a console, the `vibe` TUI or a `vibe-acp` an editor started with a
//! hidden one, keeps sharing it with its children as before.

/// `CREATE_NO_WINDOW`: the child gets a console of its own that has no window.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The creation flags of a child that never interacts with the user:
/// [`CREATE_NO_WINDOW`] when this process has no console to share, none when
/// it has one.
#[cfg(windows)]
#[must_use]
pub fn background_creation_flags() -> u32 {
    if has_console() { 0 } else { CREATE_NO_WINDOW }
}

/// Whether this process is attached to a console. `CONOUT$` names the active
/// screen buffer of the attached console and opens only when there is one,
/// whatever the standard handles were redirected to.
#[cfg(windows)]
fn has_console() -> bool {
    static ATTACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ATTACHED.get_or_init(|| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("CONOUT$")
            .is_ok()
    })
}

/// A command that starts a child in the background, which on Windows takes
/// [`background_creation_flags`] and elsewhere changes nothing.
pub trait BackgroundCommand {
    /// Keeps the child from opening a console window (see the module
    /// documentation).
    fn in_background(&mut self) -> &mut Self;
}

impl BackgroundCommand for std::process::Command {
    fn in_background(&mut self) -> &mut Self {
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(self, background_creation_flags());
        self
    }
}

impl BackgroundCommand for tokio::process::Command {
    fn in_background(&mut self) -> &mut Self {
        #[cfg(windows)]
        self.creation_flags(background_creation_flags());
        self
    }
}

/// The process environment without the variables whose name or value is not
/// Unicode, which [`std::env::vars`] panics on. Windows allows an unpaired
/// surrogate in either, and so does any byte string on Unix.
pub fn unicode_vars() -> impl Iterator<Item = (String, String)> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
}
