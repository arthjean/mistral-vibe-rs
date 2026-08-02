use vibe_app_server::startup::StartupHost;

use crate::Arguments;

use super::StartupError;
use super::dialog::run_resume_dialog;

const MAX_STARTUP_SESSIONS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeResolution {
    Unchanged,
    StartNew,
    Resume(String),
    Abort,
}

pub fn resolve_bare_resume(
    arguments: &Arguments,
    host: &StartupHost,
) -> Result<ResumeResolution, StartupError> {
    if arguments.resume.as_deref() != Some("") {
        return Ok(ResumeResolution::Unchanged);
    }
    let sessions = host.saved_sessions(MAX_STARTUP_SESSIONS)?;
    if sessions.is_empty() {
        return Ok(ResumeResolution::StartNew);
    }
    run_resume_dialog(
        arguments
            .workdir
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(".")),
        &sessions,
    )
}

#[cfg(test)]
mod tests {
    use vibe_app_server::release3::Release3Paths;

    use super::*;

    #[test]
    fn bare_resume_without_saved_sessions_starts_new() {
        let root = tempfile::tempdir().expect("workspace");
        let mut arguments = crate::arguments_for_test();
        arguments.workdir = Some(root.path().to_path_buf());
        arguments.resume = Some(String::new());
        let host = StartupHost::new(Release3Paths {
            vibe_home: root.path().join("vibe-home"),
            working_directory: root.path().to_path_buf(),
            session_root: root.path().join("vibe-home/sessions"),
        });
        assert_eq!(
            resolve_bare_resume(&arguments, &host).expect("resume resolution"),
            ResumeResolution::StartNew
        );
    }
}
