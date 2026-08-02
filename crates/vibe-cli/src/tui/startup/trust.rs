use std::fs;
use std::path::{Path, PathBuf};

use vibe_app_server::startup::StartupHost;

use crate::Arguments;

use super::StartupError;
use super::dialog::run_trust_dialog;

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
    if arguments.setup || arguments.check_upgrade || arguments.trust {
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
    use super::*;

    #[test]
    fn dangerous_home_directory_has_a_text_warning() {
        let home = std::env::var_os("HOME").expect("test home");
        let warning = dangerous_directory_warning(Path::new(&home)).expect("home warning");
        assert!(warning.contains("WARNING"));
        assert!(warning.contains("home directory"));
    }
}
