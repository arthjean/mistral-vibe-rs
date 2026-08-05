//! `vibe mcp`, the one MCP operation the interactive client cannot perform.
//!
//! The reference intercepts `mcp` before its own argument parser runs
//! (`vibe/cli/entrypoint.py:258`) and routes it to a small command of its own,
//! where `remove` drops a server from the user configuration. Adding a server
//! belongs to `/mcp add` in the session, which is where this port implements it.

use std::io::Write;
use std::path::{Path, PathBuf};

use vibe_app_server::release3::Release3Service;

const USAGE: &str = "Usage: vibe mcp remove <name>";

/// Whether `arguments` is a `vibe mcp` invocation, which is decided before the
/// interactive parser sees them because `mcp` is not a prompt.
#[must_use]
pub fn intercepts(arguments: &[String]) -> bool {
    arguments.first().map(String::as_str) == Some("mcp")
}

/// Runs `vibe mcp <subcommand>`, writing its outcome to `output`.
///
/// # Errors
///
/// Returns the message to print on the error stream when the subcommand is
/// unknown, its arguments are wrong, or the configuration cannot be written.
pub fn run(arguments: &[String], output: &mut dyn Write) -> Result<(), String> {
    match arguments {
        [subcommand, name] if subcommand == "remove" => {
            let working_directory =
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let removal = store(&working_directory)
                .persist_mcp_remove(name)
                .map_err(|error| error.to_string())?;
            let message = if removal.removed {
                format!("Removed MCP server `{}`", removal.name)
            } else {
                format!("MCP server `{}` is not configured", removal.name)
            };
            writeln!(output, "{message}").map_err(|error| error.to_string())
        }
        [subcommand] if subcommand == "remove" => Err(USAGE.to_owned()),
        [subcommand, ..] if subcommand == "add" => Err(
            "vibe mcp add is not implemented by this runtime; add a server with `/mcp add <url>` in the session"
                .to_owned(),
        ),
        _ => Err(USAGE.to_owned()),
    }
}

/// The configuration store a one-shot command writes through.
///
/// The project file is only ever writable while the workspace is trusted, and a
/// command that never opened the workspace holds no trust decision, so the
/// removal lands in the user file exactly as `vibe mcp remove` does upstream.
fn store(working_directory: &Path) -> vibe_core::config::LayeredConfig {
    let vibe_home = std::env::var_os("VIBE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".vibe"))
        })
        .unwrap_or_else(|| working_directory.join(".vibe"));
    Release3Service::for_runtime_session_root(vibe_home.join("sessions"), working_directory)
        .layered_config()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_mcp_argument_is_intercepted() {
        assert!(intercepts(&["mcp".to_owned(), "remove".to_owned()]));
        assert!(!intercepts(&["--help".to_owned()]));
        assert!(!intercepts(&[]));
    }

    #[test]
    fn an_unknown_subcommand_reports_the_usage_instead_of_removing_anything() {
        let mut output = Vec::new();
        assert_eq!(
            run(&["list".to_owned()], &mut output),
            Err(USAGE.to_owned())
        );
        assert!(output.is_empty());
    }

    /// Removal is addressed by name, and a name nothing carries is answered
    /// rather than raised, so asking twice is not an error.
    #[test]
    fn removing_a_server_twice_reports_it_gone_the_second_time() {
        let temporary = tempfile::tempdir().expect("temporary home");
        let home = temporary.path().join("home/.vibe");
        std::fs::create_dir_all(&home).expect("user configuration directory");
        std::fs::write(
            home.join("config.toml"),
            "[[mcp_servers]]\nname = \"docs\"\ntransport = \"streamable-http\"\nurl = \"https://docs.example/mcp\"\n",
        )
        .expect("user configuration");
        let store = Release3Service::for_runtime_session_root(
            home.join("sessions"),
            temporary.path().join("project"),
        )
        .layered_config();

        let first = store.persist_mcp_remove("docs").expect("first removal");
        let second = store.persist_mcp_remove("docs").expect("second removal");

        assert!(first.removed);
        assert!(!second.removed);
        assert!(
            !std::fs::read_to_string(home.join("config.toml"))
                .expect("configuration reads back")
                .contains("docs"),
            "the entry is gone from the file the write landed in"
        );
    }
}
