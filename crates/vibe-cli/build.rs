//! Gives the `vibe` binary the main-thread stack Linux gives it.
//!
//! The main thread gets 8 MiB of stack on Linux and 1 MiB on Windows. An
//! unoptimized build of the legacy turn loop needs more than 1 MiB, so `vibe`
//! overflowed its stack in debug builds on Windows only. The linker reserves
//! the Linux default for this binary instead; every other crate and every
//! dependency builds unchanged.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
}
