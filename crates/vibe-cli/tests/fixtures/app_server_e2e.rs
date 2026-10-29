//! The app server over stdio, composed by the function `vibe` builds its own
//! with, for the e2e battery (`tasks/prd-e2e-test-battery.md`, US-020). It
//! takes `vibe`'s arguments, of which the reference's `vibe-app-server`
//! declares `--legacy-harness` and `--experimental-harness`.

use std::io::Write;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = match vibe_cli::argv::parse_arguments(std::env::args_os()) {
        Ok(arguments) => arguments,
        Err(failure) => {
            let _ = write!(std::io::stderr().lock(), "{}", failure.rendered);
            return ExitCode::from(failure.exit);
        }
    };
    vibe_cli::publish_dotenv(&arguments);
    vibe_cli::install_file_logging(&arguments);
    match vibe_cli::serve_app_server_stdio(arguments).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}
