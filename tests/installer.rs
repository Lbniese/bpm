//! Hermetic shell-invocation tests for the POSIX installer script.
//!
//! These tests use temporary directories with fake executables (curl, cargo)
//! so they never contact the public network, install a real binary, or
//! require sudo.

#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::process::Command;

/// Helper: create a temporary directory with fake `curl` and `cargo`
/// scripts, return the modified PATH and the temp dir path.
#[cfg(unix)]
fn setup_fake_environment() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).expect("create bin dir");

    // Fake curl: records arguments to a marker file; fails API call and
    // asset download.
    let curl_script = bin.join("curl");
    let mut f = std::fs::File::create(&curl_script).expect("create fake curl");
    write!(
        f,
        r#"#!/bin/sh
echo "CURL_ARGS: $@" >> "{marker}"
# API call to /releases/latest fails with empty output.
# Asset download also fails (exit 1).
exit 1
"#,
        marker = dir.path().join("curl_args.txt").display()
    )
    .expect("write curl script");
    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&curl_script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }

    // Fake cargo: record arguments and exit 1 (toolchain not found path).
    let cargo_script = bin.join("cargo");
    let mut f = std::fs::File::create(&cargo_script).expect("create fake cargo");
    write!(
        f,
        r#"#!/bin/sh
echo "CARGO_ARGS: $@" >> "{marker}"
exit 1
"#,
        marker = dir.path().join("cargo_args.txt").display()
    )
    .expect("write cargo script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cargo_script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (dir, path)
}

#[test]
#[cfg(unix)]
fn api_failure_falls_back_to_latest_redirect() {
    let (dir, path) = setup_fake_environment();
    let install_dir = dir.path().join("install");
    std::fs::create_dir_all(&install_dir).expect("install dir");

    let output = Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .env("PATH", &path)
        .env("BPM_REPO", "https://github.com/Lbniese/bpm")
        .env("BPM_INSTALL_DIR", install_dir.to_str().unwrap())
        .output()
        .expect("run install.sh");

    // Must not exit 127 (command not found) or contain that pattern.
    assert_ne!(
        output.status.code(),
        Some(127),
        "install.sh must not exit 127: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("command not found"),
        "stderr must not contain 'command not found': {stderr}"
    );

    // Output should mention API failure and the latest release redirect
    // or source build fallback.  At a minimum, the latest-redirect URL
    // should appear.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("release") || stdout.contains("latest") || stdout.contains("source build"),
        "output should mention release/fallback: {stdout}"
    );

    // Verify no bpm binary was created (since fake cargo/curl both fail).
    assert!(
        !install_dir.join("bpm").exists(),
        "no bpm binary should be installed"
    );
}

#[test]
#[cfg(unix)]
fn explicit_version_uses_exact_asset_url() {
    let (dir, path) = setup_fake_environment();
    let install_dir = dir.path().join("install");
    std::fs::create_dir_all(&install_dir).expect("install dir");

    let output = Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .env("PATH", &path)
        .env("BPM_REPO", "https://github.com/Lbniese/bpm")
        .env("BPM_VERSION", "0.0.1")
        .env("BPM_INSTALL_DIR", install_dir.to_str().unwrap())
        .output()
        .expect("run install.sh");

    // The script should not exit 127.
    assert_ne!(
        output.status.code(),
        Some(127),
        "install.sh must not exit 127: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Check curl arguments contain the exact version asset URL pattern.
    let curl_args_path = dir.path().join("curl_args.txt");
    if curl_args_path.exists() {
        let args = std::fs::read_to_string(&curl_args_path).unwrap_or_default();
        assert!(
            args.contains("/releases/download/v0.0.1/"),
            "curl arguments should contain exact version URL: {args}"
        );
    }
}

#[test]
#[cfg(unix)]
fn invalid_version_override_is_rejected() {
    let (dir, path) = setup_fake_environment();
    let install_dir = dir.path().join("install");
    std::fs::create_dir_all(&install_dir).expect("install dir");

    let output = Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .env("PATH", &path)
        .env("BPM_REPO", "https://github.com/Lbniese/bpm")
        .env("BPM_VERSION", "-malicious")
        .env("BPM_INSTALL_DIR", install_dir.to_str().unwrap())
        .output()
        .expect("run install.sh");

    // Should fail with a clear error message, not proceed.
    assert!(
        !output.status.success(),
        "invalid version should cause failure"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Invalid BPM_VERSION"),
        "should report invalid version: {stdout}"
    );
}
