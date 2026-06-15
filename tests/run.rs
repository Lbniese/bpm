//! End-to-end tests for the `bpm run <script>` command-execution contract.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn bpm() -> String {
    std::env::var("CARGO_BIN_EXE_bpm").expect("CARGO_BIN_EXE_bpm")
}

/// A temp project with a `package.json` carrying the given `scripts` map.
fn make_project(scripts: &str) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    let json = format!(r#"{{"name":"run-fixture","version":"9.9.9","scripts":{scripts}}}"#);
    fs::write(project.path().join("package.json"), json).unwrap();
    project
}

fn run_script(project: &Path, script: &str) -> std::process::Output {
    Command::new(bpm())
        .args(["run", script])
        .current_dir(project)
        .output()
        .unwrap()
}

#[test]
fn injects_npm_compatible_environment() {
    // Use a script that prints env vars to stdout so we can
    // assert on each one without shell-format-string ambiguity.
    let scripts = r#"{"echo-env":"printf 'npm_lifecycle_event=%s\nnpm_package_name=%s\nnpm_package_version=%s\nnpm_config_user_agent=%s\nnpm_execpath=%s\nINIT_CWD=%s\n' \"$npm_lifecycle_event\" \"$npm_package_name\" \"$npm_package_version\" \"$npm_config_user_agent\" \"$npm_execpath\" \"$INIT_CWD\""}"#;
    let project = make_project(scripts);

    let output = run_script(project.path(), "echo-env");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let cwd = project.path().canonicalize().unwrap();

    let get = |key: &str| -> String {
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix(key) {
                if let Some(rest) = rest.strip_prefix('=') {
                    return rest.to_string();
                }
            }
        }
        panic!("missing {key} in:\n{stdout}");
    };

    assert_eq!(get("npm_lifecycle_event"), "echo-env");
    assert_eq!(get("npm_package_name"), "run-fixture");
    assert_eq!(get("npm_package_version"), "9.9.9");
    assert!(
        get("npm_config_user_agent").starts_with("bpm/"),
        "npm_config_user_agent must start with bpm/, got: {}",
        get("npm_config_user_agent")
    );
    assert_eq!(get("npm_execpath"), "bpm");
    assert_eq!(get("INIT_CWD"), cwd.to_str().unwrap());
}

#[test]
fn prepends_node_modules_bin_to_path() {
    let scripts = r#"{"use-bin":"bpm-fixture-bin"}"#;
    let project = make_project(scripts);

    let bin = project.path().join("node_modules/.bin/bpm-fixture-bin");
    fs::create_dir_all(bin.parent().unwrap()).unwrap();
    fs::write(&bin, "#!/bin/sh\nprintf 'ran-local-bin'\n").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

    let output = run_script(project.path(), "use-bin");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"ran-local-bin");
}

#[test]
fn missing_script_errors_with_nonzero_exit() {
    let project = make_project(r#"{"other":"true"}"#);
    let output = run_script(project.path(), "nope");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("is not defined in package.json"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn propagates_nonzero_child_exit() {
    let project = make_project(r#"{"boom":"exit 42"}"#);
    let output = run_script(project.path(), "boom");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("boom"),
        "stderr should name the script: {stderr}"
    );
    assert!(
        stderr.contains("42"),
        "stderr should name the child exit code: {stderr}"
    );
}
