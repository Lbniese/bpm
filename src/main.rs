//! `bpm` command-line entry point.

mod cli;

use std::process::ExitCode;

/// Run the CLI on a worker thread with an explicit stack size.
///
/// Windows defaults the main thread's stack to 1 MiB, while Unix uses 8 MiB.
/// bpm's recursive dependency resolution needs more than 1 MiB on non-trivial
/// graphs, so running on the main thread overflowed the stack on Windows
/// (masked on Unix by the larger default). Spawning an 8 MiB worker thread
/// restores cross-platform parity. The default panic hook still reports any
/// panic; `join` maps a panicked worker to a failure exit code.
fn main() -> ExitCode {
    const STACK_SIZE: usize = 8 * 1024 * 1024;
    let handle = std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(cli::run)
        .expect("spawn cli worker thread");
    handle.join().unwrap_or(ExitCode::FAILURE)
}
