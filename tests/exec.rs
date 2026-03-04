//! End-to-end tests for the local-only `bpm exec` contract.

#![cfg(unix)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn bpm() -> String {
    std::env::var("CARGO_BIN_EXE_bpm").expect("CARGO_BIN_EXE_bpm")
}

fn make_project() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    fs::write(project.path().join("package.json"), r#"{"name":"fixture"}"#).unwrap();
    fs::create_dir_all(project.path().join("node_modules/.bin")).unwrap();
    project
}

fn write_command(project: &Path, name: &str, body: &str) -> PathBuf {
    let command = project.join("node_modules/.bin").join(name);
    fs::create_dir_all(command.parent().unwrap()).unwrap();
    fs::write(&command, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    fs::set_permissions(&command, fs::Permissions::from_mode(0o755)).unwrap();
    command
}

fn run(project: &Path, command: &OsStr, args: &[OsString]) -> std::process::Output {
    Command::new(bpm())
        .arg("exec")
        .arg(command)
        .args(args)
        .current_dir(project)
        .output()
        .unwrap()
}

#[test]
fn nested_cwd_selects_nearest_project_and_keeps_caller_cwd() {
    let outer = make_project();
    write_command(outer.path(), "which-project", "printf 'outer|%s' \"$PWD\"");

    let inner = outer.path().join("packages/app");
    fs::create_dir_all(inner.join("deep/src")).unwrap();
    fs::write(inner.join("package.json"), r#"{"name":"inner"}"#).unwrap();
    write_command(&inner, "which-project", "printf 'inner|%s' \"$PWD\"");
    let cwd = inner.join("deep/src");

    let output = run(&cwd, OsStr::new("which-project"), &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("inner|{}", cwd.canonicalize().unwrap().display())
    );
}

#[test]
fn exact_local_command_wins_over_caller_path_and_missing_never_falls_back() {
    let project = make_project();
    write_command(project.path(), "collision", "printf local");
    let global = tempfile::tempdir().unwrap();
    let global_command = global.path().join("collision");
    fs::write(&global_command, "#!/bin/sh\nprintf global\n").unwrap();
    fs::set_permissions(&global_command, fs::Permissions::from_mode(0o755)).unwrap();

    let local = Command::new(bpm())
        .args(["exec", "collision"])
        .current_dir(project.path())
        .env("PATH", global.path())
        .output()
        .unwrap();
    assert!(local.status.success());
    assert_eq!(local.stdout, b"local");

    let missing = Command::new(bpm())
        .args(["exec", "global-only"])
        .current_dir(project.path())
        .env("PATH", global.path())
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not found in project bin directory"));
}

#[test]
fn preserves_native_arguments_including_empty_flags_unicode_and_non_utf8() {
    let project = make_project();
    write_command(
        project.path(),
        "argv",
        "for arg in \"$@\"; do printf '%s\\0' \"$arg\"; done",
    );
    let non_utf8 = OsString::from_vec(vec![b'n', 0x80, b'v']);
    let args = [
        OsString::from("argument with spaces"),
        OsString::new(),
        OsString::from("--leading-flag"),
        OsString::from("héllo"),
        non_utf8.clone(),
    ];

    let output = run(project.path(), OsStr::new("argv"), &args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut expected = Vec::new();
    for argument in args {
        expected.extend(argument.into_vec());
        expected.push(0);
    }
    assert_eq!(output.stdout, expected);
}

#[test]
fn preserves_environment_and_piped_standard_streams() {
    let project = make_project();
    write_command(
        project.path(),
        "streams",
        "printf 'env:%s\\n' \"$BPM_EXEC_MARKER\"; cat; printf 'child-stderr' >&2",
    );
    let mut child = Command::new(bpm())
        .args(["exec", "streams"])
        .current_dir(project.path())
        .env("BPM_EXEC_MARKER", "inherited")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"child-stdin")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert_eq!(output.stdout, b"env:inherited\nchild-stdin");
    assert_eq!(output.stderr, b"child-stderr");
}

#[test]
fn rejects_empty_dot_absolute_and_separator_command_names() {
    let project = make_project();
    for command in [
        OsString::new(),
        OsString::from("."),
        OsString::from(".."),
        OsString::from("nested/tool"),
        OsString::from("nested\\tool"),
        OsString::from("/absolute"),
    ] {
        let output = run(project.path(), &command, &[]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("invalid local command"),
            "command {:?}: {}",
            command,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn distinguishes_missing_spawn_failure_and_child_nonzero() {
    let project = make_project();
    write_command(project.path(), "nonzero-one", "exit 1");
    write_command(project.path(), "nonzero", "exit 231");
    let blocked = write_command(project.path(), "not-executable", "exit 0");
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o644)).unwrap();

    let missing = run(project.path(), OsStr::new("missing"), &[]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not found in project bin directory"));

    let spawn_failure = run(project.path(), OsStr::new("not-executable"), &[]);
    assert_eq!(spawn_failure.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&spawn_failure.stderr).contains("failed to spawn local command")
    );

    for (command, expected) in [("nonzero-one", 1), ("nonzero", 231)] {
        let nonzero = run(project.path(), OsStr::new(command), &[]);
        assert_eq!(nonzero.status.code(), Some(expected));
        assert!(nonzero.stderr.is_empty());
    }
}

#[test]
fn reraises_sigterm_and_sigint_from_local_child() {
    for (name, signal) in [("term", 15), ("interrupt", 2)] {
        let project = make_project();
        write_command(project.path(), name, &format!("kill -{signal} $$"));
        let status = Command::new(bpm())
            .args(["exec", name])
            .current_dir(project.path())
            .status()
            .unwrap();
        assert_eq!(
            status.signal(),
            Some(signal),
            "{name} must preserve its signal"
        );
    }
}

#[test]
fn reraises_sigkill_without_attempting_invalid_disposition_reset() {
    let project = make_project();
    write_command(project.path(), "kill9", "kill -9 $$");
    let status = Command::new(bpm())
        .args(["exec", "kill9"])
        .current_dir(project.path())
        .status()
        .unwrap();
    // SIGKILL cannot be caught, so the parent always sees signal 9.
    assert_eq!(
        status.signal(),
        Some(9),
        "SIGKILL must propagate as signal 9"
    );
}
