use std::fs;
use std::path::{Path, PathBuf};

use vibe_app_server::startup::StartupHost;

use crate::Arguments;

use super::StartupError;
use super::dialog::{run_location_warning_dialog, run_trust_dialog};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustResolution {
    pub trusted: bool,
    pub cancelled: bool,
    pub dangerous_warning: Option<String>,
}

pub fn resolve_workspace_trust(
    arguments: &mut Arguments,
    host: &StartupHost,
) -> Result<TrustResolution, StartupError> {
    let dangerous_warning = dangerous_directory_warning(
        arguments
            .workdir
            .as_deref()
            .unwrap_or_else(|| Path::new(".")),
    );
    // `--setup` no longer suppresses the dialog: the reference decides trust
    // through the pre-session dialog, and the onboarding flow never asks, so
    // an undecided workspace is prompted even when setup was requested.
    if arguments.check_upgrade || arguments.trust {
        return Ok(TrustResolution {
            trusted: arguments.trust,
            cancelled: false,
            dangerous_warning,
        });
    }
    let inspection = host.inspect_workspace_trust()?;
    if inspection.trusted {
        arguments.trust = true;
        return Ok(TrustResolution {
            trusted: true,
            cancelled: false,
            dangerous_warning,
        });
    }
    let Some(prompt) = inspection.prompt else {
        return Ok(TrustResolution {
            trusted: false,
            cancelled: false,
            dangerous_warning,
        });
    };
    let Some(decision) = run_trust_dialog(&prompt)? else {
        return Ok(TrustResolution {
            trusted: false,
            cancelled: true,
            dangerous_warning,
        });
    };
    let trusted = host.decide_workspace_trust(&prompt, decision)?;
    arguments.trust = trusted;
    Ok(TrustResolution {
        trusted,
        cancelled: false,
        dangerous_warning,
    })
}

pub fn resolve_location_safety(warning: Option<&str>) -> Result<bool, StartupError> {
    warning.map_or(Ok(true), run_location_warning_dialog)
}

#[must_use]
pub fn dangerous_directory_warning(path: &Path) -> Option<String> {
    let path = fs::canonicalize(path).ok()?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|path| fs::canonicalize(path).ok());
    let mut dangerous = Vec::new();
    if let Some(home) = home {
        dangerous.extend([
            (home.clone(), "home directory"),
            (home.join("Documents"), "Documents folder"),
            (home.join("Desktop"), "Desktop folder"),
            (home.join("Downloads"), "Downloads folder"),
            (home.join("Pictures"), "Pictures folder"),
            (home.join("Movies"), "Movies folder"),
            (home.join("Music"), "Music folder"),
            (home.join("Library"), "Library folder"),
        ]);
    }
    dangerous.extend([
        (PathBuf::from("/Applications"), "Applications folder"),
        (PathBuf::from("/System"), "System folder"),
        (PathBuf::from("/Library"), "System Library folder"),
        (PathBuf::from("/usr"), "System usr folder"),
        (PathBuf::from("/private"), "System private folder"),
    ]);
    dangerous.into_iter().find_map(|(candidate, description)| {
        (path == candidate).then(|| {
            format!(
                "WARNING: You are in the {description}\n\nRunning in this location is not recommended."
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn setup_mode_no_longer_suppresses_trust_resolution() {
        let home = tempfile::tempdir().expect("temporary home");
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let canonical = fs::canonicalize(workspace.path()).expect("canonical workspace");
        let vibe_home = home.path().join(".vibe");
        fs::create_dir_all(&vibe_home).expect("vibe home");
        fs::write(
            vibe_home.join("trusted_folders.toml"),
            format!("trusted = [{:?}]\nuntrusted = []\n", canonical.display()),
        )
        .expect("trust settings");
        let session_root = vibe_home.join("sessions");
        let mut arguments = crate::Arguments::try_parse_from([
            "vibe",
            "--setup",
            "--workdir",
            &workspace.path().display().to_string(),
            "--session-root",
            &session_root.display().to_string(),
        ])
        .expect("arguments parse");
        let host = super::super::startup_host(&arguments, workspace.path());
        let resolution =
            resolve_workspace_trust(&mut arguments, &host).expect("trust resolution runs");
        // Before EP-055 the `--setup` flag returned early with `trusted:
        // false`; the decided workspace must now resolve without a dialog.
        assert!(resolution.trusted);
        assert!(!resolution.cancelled);
        assert!(arguments.trust);
    }

    #[test]
    fn dangerous_home_directory_has_a_text_warning() {
        let home = std::env::var_os("HOME").expect("test home");
        let warning = dangerous_directory_warning(Path::new(&home)).expect("home warning");
        assert!(warning.contains("WARNING"));
        assert!(warning.contains("home directory"));
    }
}
