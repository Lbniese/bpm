//! Small platform primitives shared by lifecycle and CLI execution.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Find an executable using an explicit PATH value.  On Windows the sanitized
/// PATHEXT list is consulted by the caller's environment; the exact filename
/// is always tried first.
pub fn find_executable(name: &OsStr, inherited_path: Option<&OsStr>) -> Option<PathBuf> {
    if name.is_empty()
        || Path::new(name).is_absolute()
        || name == OsStr::new(".")
        || name == OsStr::new("..")
    {
        return None;
    }
    let path_value = inherited_path
        .map(OsString::from)
        .or_else(|| std::env::var_os("PATH"))?;
    for directory in std::env::split_paths(&path_value) {
        let exact = directory.join(name);
        if exact.is_file() {
            return Some(exact);
        }
        #[cfg(windows)]
        if Path::new(name).extension().is_none() {
            for extension in pathext() {
                let mut candidate = exact.clone().into_os_string();
                candidate.push(extension);
                let candidate = PathBuf::from(candidate);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Construct the npm-compatible command used for a lifecycle/root script.
pub fn script_command(script: &str) -> Command {
    #[cfg(windows)]
    {
        let shell = std::env::var_os("COMSPEC")
            .filter(|value| Path::new(value).is_file())
            .unwrap_or_else(|| OsString::from("cmd.exe"));
        let mut command = Command::new(shell);
        command.args(["/D", "/S", "/C", script]);
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }
}

/// Compare file identities where the platform has a stable identity API.
pub fn same_file_identity(a: &Path, b: &Path) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let left = std::fs::metadata(a)?;
        let right = std::fs::metadata(b)?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
    #[cfg(windows)]
    {
        // Stable identity check on Windows: two paths name the same file when
        // they canonicalize to the same path. The precise volume-serial /
        // file-index comparison needs the unstable `windows_by_handle`
        // feature, so it is avoided here to keep the build on stable Rust.
        let normalize = |p: &Path| std::fs::canonicalize(p).ok();
        Ok(normalize(a) == normalize(b))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (a, b);
        Ok(false)
    }
}

#[cfg(windows)]
fn pathext() -> Vec<OsString> {
    use std::collections::BTreeSet;
    let defaults = [".COM", ".EXE", ".BAT", ".CMD"];
    let values = std::env::var_os("PATHEXT")
        .map(|v| {
            v.to_string_lossy()
                .split(';')
                .map(str::trim)
                .map(str::to_ascii_uppercase)
                .collect::<Vec<_>>()
        })
        .filter(|v: &Vec<String>| !v.is_empty())
        .unwrap_or_else(|| defaults.iter().map(|v| (*v).to_string()).collect());
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .filter_map(|v| {
            if v.len() > 1
                && v.starts_with('.')
                && !v.contains(['/', '\\'])
                && seen.insert(v.clone())
            {
                Some(OsString::from(v))
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn script_has_platform_shell_arguments() {
        let command = script_command("echo ok");
        let args: Vec<_> = command.get_args().collect();
        #[cfg(unix)]
        assert_eq!(args, vec![OsStr::new("-c"), OsStr::new("echo ok")]);
        #[cfg(windows)]
        assert_eq!(
            args,
            vec![
                OsStr::new("/D"),
                OsStr::new("/S"),
                OsStr::new("/C"),
                OsStr::new("echo ok")
            ]
        );
    }
}
