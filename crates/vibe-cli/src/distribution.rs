/// The repository `[workspace.package] repository` declares. Both installers
/// and every release asset live under it, so nothing here writes an owner out
/// by hand.
pub const REPOSITORY_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// The branch an upgrade fetches the installer from. A release is installed by
/// tag, but the installer that fetches it is always the published one.
const INSTALLER_BRANCH: &str = "main";

/// The raw URL of one script under `scripts/`, or `None` when the declared
/// repository is not a GitHub repository this can address.
#[must_use]
fn installer_script_url(script: &str) -> Option<String> {
    let slug = REPOSITORY_URL
        .strip_prefix("https://github.com/")?
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let segments: Vec<&str> = slug.split('/').collect();
    (segments.len() == 2 && segments.iter().all(|segment| !segment.is_empty())).then(|| {
        format!("https://raw.githubusercontent.com/{slug}/{INSTALLER_BRANCH}/scripts/{script}")
    })
}

/// Reference `UPDATE_COMMANDS`: what the update action runs, in order.
///
/// The reference reruns the package managers it publishes under. This port
/// publishes one installer per platform instead, so an upgrade reruns the
/// installer that produced the running binary, which is also what
/// [`crate::tui::updates::update_failed_message`] names when every command
/// fails.
#[must_use]
pub fn upgrade_commands() -> Vec<String> {
    let command = if cfg!(windows) {
        installer_script_url("install.ps1").map(|url| {
            format!("powershell -NoProfile -ExecutionPolicy Bypass -Command \"irm {url} | iex\"")
        })
    } else {
        installer_script_url("install.sh").map(|url| format!("curl -fsSL {url} | sh"))
    };
    command.into_iter().collect()
}

#[cfg(test)]
mod action_parity_tests;
#[cfg(test)]
mod completion_parity_tests;
#[cfg(test)]
mod release_parity_tests;
