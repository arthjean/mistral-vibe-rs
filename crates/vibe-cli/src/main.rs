#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use vibe_cli::Arguments;

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = Arguments::parse();
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    match vibe_cli::run(arguments, &mut stdout, &mut stderr).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            ExitCode::from(error.exit_code())
        }
    }
}
