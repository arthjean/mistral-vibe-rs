//! Editing the prompt in the operator's own editor, reference
//! `ExternalEditor`: `$VISUAL`, then `$EDITOR`, then `nano`.

use std::fs;
use std::process::{Command, ExitStatus};

pub trait ExternalEditorPort {
    /// Edits `initial` and answers the edited text, or `None` when the editor
    /// ran and failed. An editor that cannot run at all is an error naming it.
    fn edit(&mut self, initial: &str) -> Result<Option<String>, String>;
}

pub struct SystemExternalEditor {
    command: String,
}

impl SystemExternalEditor {
    #[must_use]
    pub fn from_environment() -> Self {
        Self::from_sources(std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok())
    }

    pub(super) fn from_sources(visual: Option<String>, editor: Option<String>) -> Self {
        Self {
            command: visual
                .filter(|value| !value.trim().is_empty())
                .or_else(|| editor.filter(|value| !value.trim().is_empty()))
                .unwrap_or_else(|| "nano".to_owned()),
        }
    }

    pub(super) fn command_parts(&self) -> Result<Vec<String>, String> {
        let parts = shlex::split(&self.command)
            .filter(|parts| !parts.is_empty())
            .ok_or_else(|| "External editor command is invalid".to_owned())?;
        Ok(parts)
    }

    pub fn edit_file(&mut self, path: &std::path::Path) -> Result<String, String> {
        let command = self.command_parts()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        if !path.exists() {
            vibe_core::atomic_file::create_private_file(path).map_err(|error| error.to_string())?;
        }
        // Reference `ExternalEditor.edit_file(check=False)`: only an editor
        // that cannot start fails; its exit status is not read.
        run_editor(&command, path).map_err(|error| unstartable(&command[0], &error))?;
        fs::read_to_string(path).map_err(|error| error.to_string())
    }
}

/// Runs the editor `command` on `path` and waits for it.
///
/// std hands a `.cmd` or `.bat` program to `cmd.exe`, which starts even when
/// the script is missing, so an editor named by a path that holds no file
/// cannot start, whatever its extension.
fn run_editor(command: &[String], path: &std::path::Path) -> std::io::Result<ExitStatus> {
    let program = std::path::Path::new(&command[0]);
    if program.components().count() > 1 && !program.is_file() {
        return Err(std::io::ErrorKind::NotFound.into());
    }
    Command::new(program).args(&command[1..]).arg(path).status()
}

fn unstartable(program: &str, error: &std::io::Error) -> String {
    format!("External editor `{program}` could not start: {error}")
}

impl ExternalEditorPort for SystemExternalEditor {
    fn edit(&mut self, initial: &str) -> Result<Option<String>, String> {
        let command = self.command_parts()?;
        let stamp = vibe_core::clock::now_nanos();
        let path = std::env::temp_dir().join(format!(
            "mistral-vibe-rs-editor-{}-{stamp}.md",
            std::process::id()
        ));
        let mut file = vibe_core::atomic_file::create_private_file(&path)
            .map_err(|error| error.to_string())?;
        std::io::Write::write_all(&mut file, initial.as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        let result = run_editor(&command, &path);
        // Reference `ExternalEditor.edit`: an editor that exits with a failure
        // leaves the prompt as it was.
        let edited = match result {
            Ok(status) if status.success() => fs::read_to_string(&path)
                .map(|content| Some(content.trim_end().to_owned()))
                .map_err(|error| error.to_string()),
            Ok(_) => Ok(None),
            Err(error) => Err(unstartable(&command[0], &error)),
        };
        let _ = fs::remove_file(&path);
        edited
    }
}
