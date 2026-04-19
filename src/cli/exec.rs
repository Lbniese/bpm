//! Local-only project-bin command execution.

use std::env;
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};

use bpm::project::find_project_root;

const BIN_DIRECTORY: [&str; 2] = ["node_modules", ".bin"];

/// Execute one exact binary from the nearest npm project's local bin directory.
pub(super) fn run(command: &OsStr, args: &[OsString]) -> anyhow::Result<ExitCode> {
    validate_command_name(command)?;

    let cwd = env::current_dir().map_err(|error| {
        anyhow::anyhow!("could not determine the current directory for bpm exec: {error}")
    })?;
    let project = find_project_root(&cwd)?;
    let bin_directory = project.join(BIN_DIRECTORY[0]).join(BIN_DIRECTORY[1]);
    let executable = resolve_local_executable(&bin_directory, command).ok_or_else(|| {
        anyhow::anyhow!(
            "local command '{}' was not found in project bin directory {} (project {})",
            command.to_string_lossy(),
            bin_directory.display(),
            project.display()
        )
    })?;
    let path = local_first_path(&bin_directory)?;

    let status = Command::new(&executable)
        .args(args)
        .current_dir(&cwd)
        .env("PATH", path)
        .status()
        .map_err(|error| {
            anyhow::anyhow!(
                "failed to spawn local command '{}' at {} from {}: {error}",
                command.to_string_lossy(),
                executable.display(),
                cwd.display()
            )
        })?;

    match command_outcome(status) {
        CommandOutcome::Exit(code) => std::process::exit(code),
        #[cfg(unix)]
        CommandOutcome::Signal(signal) => reraise_signal(signal, command),
        CommandOutcome::Unknown => Err(anyhow::anyhow!(
            "local command '{}' ended without an exit code",
            command.to_string_lossy()
        )),
    }
}

fn validate_command_name(command: &OsStr) -> anyhow::Result<()> {
    let path = Path::new(command);
    let encoded = command.as_encoded_bytes();
    let invalid = encoded.is_empty()
        || path.is_absolute()
        || command == OsStr::new(".")
        || command == OsStr::new("..")
        || encoded.contains(&b'/')
        || encoded.contains(&b'\\');

    if invalid {
        anyhow::bail!(
            "invalid local command '{}': expected one non-empty command name without path separators",
            command.to_string_lossy()
        );
    }
    Ok(())
}

fn local_first_path(bin_directory: &Path) -> anyhow::Result<OsString> {
    let mut paths = vec![bin_directory.to_path_buf()];
    if let Some(inherited) = env::var_os("PATH") {
        paths.extend(env::split_paths(&inherited));
    }
    env::join_paths(paths)
        .map_err(|error| anyhow::anyhow!("could not construct PATH for local command: {error}"))
}

#[cfg(not(windows))]
fn resolve_local_executable(bin_directory: &Path, command: &OsStr) -> Option<PathBuf> {
    existing_file(bin_directory.join(command))
}

#[cfg(windows)]
fn resolve_local_executable(bin_directory: &Path, command: &OsStr) -> Option<PathBuf> {
    let exact = bin_directory.join(command);
    if let Some(path) = existing_file(exact.clone()) {
        return Some(path);
    }
    if exact.extension().is_some() {
        return None;
    }

    windows_extensions().into_iter().find_map(|extension| {
        let mut candidate = exact.clone().into_os_string();
        candidate.push(extension);
        existing_file(PathBuf::from(candidate))
    })
}

fn existing_file(candidate: PathBuf) -> Option<PathBuf> {
    candidate.is_file().then_some(candidate)
}

#[cfg(windows)]
fn windows_extensions() -> Vec<OsString> {
    use std::collections::BTreeSet;

    const DEFAULT_EXTENSIONS: [&str; 4] = [".COM", ".EXE", ".BAT", ".CMD"];
    let configured = env::var_os("PATHEXT")
        .map(|value| {
            value
                .to_string_lossy()
                .split(';')
                .map(str::trim)
                .filter(|extension| {
                    extension.starts_with('.')
                        && extension.len() > 1
                        && !extension.contains(['/', '\\'])
                })
                .map(|extension| OsString::from(extension.to_ascii_uppercase()))
                .collect::<Vec<_>>()
        })
        .filter(|extensions| !extensions.is_empty())
        .unwrap_or_else(|| DEFAULT_EXTENSIONS.into_iter().map(OsString::from).collect());

    let mut seen = BTreeSet::new();
    configured
        .into_iter()
        .filter(|extension| seen.insert(extension.clone()))
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandOutcome {
    Exit(i32),
    #[cfg(unix)]
    Signal(std::ffi::c_int),
    Unknown,
}

fn command_outcome(status: ExitStatus) -> CommandOutcome {
    if let Some(code) = status.code() {
        return CommandOutcome::Exit(code);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if let Some(signal) = status.signal() {
            return CommandOutcome::Signal(signal);
        }
    }

    CommandOutcome::Unknown
}

#[cfg(unix)]
fn reraise_signal(signal_number: std::ffi::c_int, command: &OsStr) -> anyhow::Result<ExitCode> {
    const DEFAULT_HANDLER: usize = 0;
    const ERROR_HANDLER: usize = usize::MAX;

    unsafe extern "C" {
        fn signal(signal: std::ffi::c_int, handler: usize) -> usize;
        fn raise(signal: std::ffi::c_int) -> std::ffi::c_int;
    }

    if !has_unchangeable_disposition(signal_number) {
        // SAFETY: `signal_number` came from `ExitStatusExt::signal`. Resetting
        // that disposition is required before raising it in this process. No
        // Rust references cross the C call.
        let reset = unsafe { signal(signal_number, DEFAULT_HANDLER) };
        if reset == ERROR_HANDLER {
            return Err(signal_error(
                command,
                signal_number,
                "reset",
                io::Error::last_os_error(),
            ));
        }
    }

    // SAFETY: the signal number was supplied by the OS. Its disposition was
    // either reset above or is unchangeable. Raising SIGKILL terminates BPM
    // with the same signal as the child.
    let result = unsafe { raise(signal_number) };
    Err(signal_error(
        command,
        signal_number,
        "re-raise",
        if result == 0 {
            io::Error::other("signal was raised but did not terminate bpm")
        } else {
            io::Error::last_os_error()
        },
    ))
}

#[cfg(unix)]
fn has_unchangeable_disposition(signal_number: std::ffi::c_int) -> bool {
    const SIGKILL: std::ffi::c_int = 9;
    #[cfg(target_os = "linux")]
    const SIGSTOP: std::ffi::c_int = 19;
    #[cfg(target_os = "macos")]
    const SIGSTOP: std::ffi::c_int = 17;

    signal_number == SIGKILL || signal_number == SIGSTOP
}

#[cfg(unix)]
fn signal_error(command: &OsStr, signal: i32, action: &str, error: io::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "local command '{}' terminated by signal {signal}, but bpm could not {action} that signal: {error}",
        command.to_string_lossy()
    )
}

#[cfg(test)]
mod tests {
    use super::{command_outcome, CommandOutcome};

    #[cfg(unix)]
    #[test]
    fn classifies_normal_exit_without_narrowing() {
        use std::os::unix::process::ExitStatusExt;

        let status = std::process::ExitStatus::from_raw(231 << 8);
        assert_eq!(command_outcome(status), CommandOutcome::Exit(231));
    }

    #[cfg(windows)]
    #[test]
    fn preserves_windows_status_above_u8_range() {
        use std::os::windows::process::ExitStatusExt;

        let status = std::process::ExitStatus::from_raw(0x1234);
        assert_eq!(command_outcome(status), CommandOutcome::Exit(0x1234));
    }

    #[cfg(unix)]
    #[test]
    fn classifies_signal_for_exact_propagation() {
        use std::os::unix::process::ExitStatusExt;

        let status = std::process::ExitStatus::from_raw(15);
        assert_eq!(command_outcome(status), CommandOutcome::Signal(15));
    }
}
