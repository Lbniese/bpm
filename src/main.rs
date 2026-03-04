//! `bpm` command-line entry point.

mod cli;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}
