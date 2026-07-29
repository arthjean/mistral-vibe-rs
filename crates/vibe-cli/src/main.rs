#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use clap::Parser;

#[derive(Parser)]
#[command(name = "vibe", version, about = "Mistral Vibe RS")]
struct Arguments {}

fn main() {
    let _arguments = Arguments::parse();
}
