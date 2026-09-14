//! `td`, the name the binary is installed under.
//!
//! Cargo wants a distinct entry point per `[[bin]]`, so this is it: `main.rs`
//! compiled a second time, with no behavior of its own to drift.

#[path = "main.rs"]
mod cli;

fn main() -> std::process::ExitCode {
    cli::main()
}
