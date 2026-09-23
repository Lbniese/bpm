//! Hermetic shell-invocation tests for the POSIX installer script.
//!
//! These tests use temporary directories with fake executables (curl, cargo)
//! so they never contact the public network, install a real binary, or
//! require sudo.

#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::path::{Path, PathBuf};
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

    stub_source_tools(dir.path());

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
        .current_dir(dir.path())
        .env_remove("BPM_VERSION")
        .env("BPM_VERSION_EXPLICIT", "1") // Internal state must ignore the environment.
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
    // or source build fallback. At a minimum, the latest-redirect URL
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
    let curl_args = std::fs::read_to_string(dir.path().join("curl_args.txt")).unwrap();
    assert!(curl_args.contains("/releases/latest/download/"));
    assert!(
        dir.path().join("git_args.txt").exists(),
        "latest mode must reach fake source fallback"
    );
}

#[test]
#[cfg(unix)]
fn api_discovered_version_still_allows_source_fallback() {
    let (fake, path) = setup_fake_environment();
    let curl = fake.path().join("bin/curl");
    let script = std::fs::read_to_string(&curl).unwrap().replace(
        "\nexit 1\n",
        "\ncase \"$*\" in *api.github.com*) echo '{\"tag_name\":\"v0.3.0\"}'; exit 0 ;; esac\nexit 1\n",
    );
    std::fs::write(&curl, script).unwrap();
    let install_dir = fake.path().join("install");
    std::fs::create_dir(&install_dir).unwrap();
    let out = installer_command(&path, Path::new(""), &install_dir)
        .env("BPM_VERSION", "")
        .env("BPM_VERSION_EXPLICIT", "1")
        .output()
        .unwrap();
    assert!(!out.status.success()); // Fake git refuses the source clone.
    let urls = std::fs::read_to_string(fake.path().join("curl_args.txt")).unwrap();
    assert!(urls.contains("/releases/download/v0.3.0/"), "{urls}");
    assert!(fake.path().join("git_args.txt").exists());
    assert!(!install_dir.join("bpm").exists());
}

#[test]
#[cfg(unix)]
fn explicit_version_uses_exact_asset_url() {
    let (dir, path) = setup_fake_environment();
    let install_dir = dir.path().join("install");
    std::fs::create_dir_all(&install_dir).expect("install dir");

    let output = Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .current_dir(dir.path())
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
    let args = std::fs::read_to_string(dir.path().join("curl_args.txt")).unwrap();
    assert!(
        args.contains("/releases/download/v0.0.1/"),
        "exact version URL: {args}"
    );
}

#[test]
#[cfg(unix)]
fn invalid_version_override_is_rejected() {
    let (dir, path) = setup_fake_environment();
    let install_dir = dir.path().join("install");
    std::fs::create_dir_all(&install_dir).expect("install dir");

    let output = Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .current_dir(dir.path())
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

// ── Release provenance verification ───────────────────────────
//
// Hermetic tests for install.sh's signed-checksum verification. A disposable
// ECDSA keypair is generated in a temp directory (never committed); the
// installer's `_BPM_TEST_PUBKEY_FILE` hook injects its public half. A fake
// `curl` serves the tarball/manifest/signature from local fixture files keyed
// by URL suffix. Real `openssl`/`tar` exercise the verify/inspect path. No
// test contacts a public URL, uses a production key, installs a real bpm, or
// needs sudo.

/// Mirror install.sh's platform detection so the manifest can list the exact
/// `bpm-<platform>.tar.gz` line the installer will request on this host.
#[cfg(unix)]
fn host_platform() -> String {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    match (arch, os) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        _ => "x86_64-unknown-linux-gnu",
    }
    .to_string()
}

/// Build a disposable signing keypair + a valid signed release fixture set
/// under `root/release/`: `key.pem`, `pubkey.pem`, the platform tarball,
/// `SHA256SUMS`, and `SHA256SUMS.sig`. Returns the fixture directory.
#[cfg(unix)]
fn build_signed_release(root: &Path) -> PathBuf {
    build_signed_release_with_binary(root, FAKE_BPM)
}

#[cfg(unix)]
const FAKE_BPM: &str = "#!/bin/sh\ncase \"$1\" in\n  fetch) echo '  --registry <url>' ;;\n  install) echo '  --global' ;;\n  --version) echo 'bpm 0.3.0' ;;\nesac\n";

#[cfg(unix)]
fn build_signed_release_with_binary(root: &Path, binary: &str) -> PathBuf {
    let release = root.join("release");
    std::fs::create_dir_all(&release).unwrap();
    let key = release.join("key.pem");
    let pubkey = release.join("pubkey.pem");
    // Disposable ECDSA P-256 keypair; the private key lives only in temp and
    // is never committed or logged.
    let status = Command::new("openssl")
        .args([
            "ecparam",
            "-genkey",
            "-name",
            "prime256v1",
            "-noout",
            "-out",
        ])
        .arg(&key)
        .status()
        .unwrap();
    assert!(status.success(), "generate test keypair");
    let status = Command::new("openssl")
        .args(["ec", "-in"])
        .arg(&key)
        .args(["-pubout", "-out"])
        .arg(&pubkey)
        .status()
        .unwrap();
    assert!(status.success(), "extract test public key");

    // Fake bpm with the real CLI's stable capability and version tokens.
    let staging = root.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let bpm = staging.join("bpm");
    std::fs::write(&bpm, binary).unwrap();
    make_executable(&bpm);

    let platform = host_platform();
    let tarball_name = format!("bpm-{platform}.tar.gz");
    let tarball = release.join(&tarball_name);
    let status = Command::new("tar")
        .args(["-czf"])
        .arg(&tarball)
        .args(["-C"])
        .arg(&staging)
        .arg("bpm")
        .status()
        .unwrap();
    assert!(status.success(), "build test tarball");

    // Sorted SHA256SUMS: one conventional `<64 hex> <basename>` line.
    let hash = sha256_of(&tarball);
    let manifest = release.join("SHA256SUMS");
    std::fs::write(&manifest, format!("{hash}  {tarball_name}\n")).unwrap();

    // Detached signature over the exact manifest bytes.
    let sig = release.join("SHA256SUMS.sig");
    let status = Command::new("openssl")
        .args(["dgst", "-sha256", "-sign"])
        .arg(&key)
        .args(["-out"])
        .arg(&sig)
        .arg(&manifest)
        .status()
        .unwrap();
    assert!(status.success(), "sign test manifest");
    release
}

#[cfg(unix)]
fn sha256_of(path: &Path) -> String {
    let out = Command::new("openssl")
        .args(["dgst", "-sha256"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .last()
        .unwrap()
        .to_ascii_lowercase()
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[cfg(unix)]
fn stub_source_tools(root: &Path) {
    for tool in ["cargo", "git"] {
        let script = root.join("bin").join(tool);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}/{tool}_args.txt'\nexit 1\n",
                root.display()
            ),
        )
        .unwrap();
        make_executable(&script);
    }
}

#[cfg(unix)]
fn assert_no_source_fallback(root: &Path) {
    for tool in ["cargo", "git"] {
        assert!(
            !root.join(format!("{tool}_args.txt")).exists(),
            "unexpected {tool} invocation"
        );
    }
}

/// Build a PATH with a fake `curl` that serves the signed-release fixtures by
/// URL suffix, an optional fake `openssl` (when `fake_openssl` is true), and
/// the rest of the real PATH (for real tar/install/openssl).
#[cfg(unix)]
fn release_path(fixture_dir: &Path, fake_openssl: bool) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();

    let curl_script = bin.join("curl");
    let fixture = fixture_dir.to_path_buf();
    std::fs::write(
        &curl_script,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{marker}"
# Parse `-o <dest>` and the trailing URL from a minimal curl subset.
dest=""
url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) dest="$2"; shift 2 ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
case "$url" in
  *api.github.com*) exit 1 ;;           # API resolution fails -> latest redirect
  *.sig) cp "{fix}/SHA256SUMS.sig" "$dest" 2>/dev/null && exit 0 || exit 1 ;;
  *SHA256SUMS) cp "{fix}/SHA256SUMS" "$dest" 2>/dev/null && exit 0 || exit 1 ;;
  *.tar.gz) cp "{fix}/$(basename "$url")" "$dest" 2>/dev/null && exit 0 || exit 1 ;;
  *) exit 1 ;;
esac
"#,
            fix = fixture.display(),
            marker = dir.path().join("curl_args.txt").display()
        ),
    )
    .unwrap();
    make_executable(&curl_script);

    stub_source_tools(dir.path());

    if fake_openssl {
        let openssl_script = bin.join("openssl");
        std::fs::write(&openssl_script, "#!/bin/sh\nexit 1\n").unwrap();
        make_executable(&openssl_script);
    }

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (dir, path)
}

#[cfg(unix)]
fn run_installer(path: &str, pubkey: &Path, install_dir: &Path) -> std::process::Output {
    installer_command(path, pubkey, install_dir)
        .output()
        .expect("run install.sh")
}

#[cfg(unix)]
fn installer_command(path: &str, pubkey: &Path, install_dir: &Path) -> Command {
    let mut command = Command::new("sh");
    command
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .current_dir(install_dir.parent().unwrap())
        .env_remove("BPM_VERSION")
        .env("PATH", path)
        .env("BPM_REPO", "https://github.com/Lbniese/bpm")
        .env("BPM_INSTALL_DIR", install_dir.to_str().unwrap())
        .env("_BPM_TEST_PUBKEY_FILE", pubkey);
    command
}

#[test]
#[cfg(unix)]
fn explicit_version_normalization() {
    for requested in [
        "0.3.0",
        "v0.3.0",
        "1.2.3-0",
        "v1.2.3-rc.1",
        "1.2.3-rc01",
        "1.2.3-x-1",
    ] {
        let version = requested.strip_prefix('v').unwrap_or(requested);
        let root = tempfile::tempdir().unwrap();
        let fixture =
            build_signed_release_with_binary(root.path(), &FAKE_BPM.replace("0.3.0", version));
        let (fake, path) = release_path(&fixture, false);
        let install_dir = root.path().join("install");
        std::fs::create_dir(&install_dir).unwrap();
        let out = installer_command(&path, &fixture.join("pubkey.pem"), &install_dir)
            .env("BPM_VERSION", requested)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{requested}: {} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let urls = std::fs::read_to_string(fake.path().join("curl_args.txt")).unwrap();
        assert_eq!(urls.lines().count(), 3);
        assert!(
            urls.lines()
                .all(|line| line.contains(&format!("/releases/download/v{version}/"))),
            "{urls}"
        );
        let installed = Command::new(install_dir.join("bpm"))
            .arg("--version")
            .output()
            .unwrap();
        assert!(installed.status.success());
        assert_eq!(
            String::from_utf8(installed.stdout).unwrap(),
            format!("bpm {version}\n")
        );
        assert_no_source_fallback(fake.path());
    }
}

#[test]
#[cfg(unix)]
fn explicit_version_rejects_invalid_grammar_before_tools() {
    for version in [
        "vv1.2.3",
        "V1.2.3",
        "v",
        "1.2",
        "1.2.3.4",
        "01.2.3",
        "1.02.3",
        "1.2.03",
        "1.2.3-01",
        "1.2.3-rc.01",
        "1.2.3-",
        "1.2.3-.rc",
        "1.2.3-rc.",
        "1.2.3-rc..1",
        "1.2.3+build",
        " 1.2.3",
        "1.2.3 ",
        "1.2.3\n",
        "1.2.3\r",
        "1.2.3\t",
        "1.2.3\n4.5.6",
        "1.2.3/path",
        "1.2.3;true",
        "1.2.3-å",
        "1.2.3-rc_1",
    ] {
        let (fake, path) = setup_fake_environment();
        let install_dir = fake.path().join("install");
        std::fs::create_dir(&install_dir).unwrap();
        let out = installer_command(&path, &fake.path().join("unused.pem"), &install_dir)
            .env("BPM_VERSION", version)
            .output()
            .unwrap();
        assert!(!out.status.success(), "accepted {version:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("Invalid BPM_VERSION"),
            "{version:?}"
        );
        assert!(
            !fake.path().join("curl_args.txt").exists(),
            "download attempted for {version:?}"
        );
        assert_no_source_fallback(fake.path());
        assert!(!install_dir.join("bpm").exists());
    }
}

#[test]
#[cfg(unix)]
fn explicit_version_failures_never_fall_back() {
    for failure in [
        "missing",
        "platform",
        "signature",
        "checksum",
        "openssl",
        "capability",
        "help-status",
        "version",
        "version-status",
    ] {
        let root = tempfile::tempdir().unwrap();
        let binary = match failure {
            "capability" => FAKE_BPM.replace("--global", "--obsolete"),
            "help-status" => FAKE_BPM.replace("echo '  --global'", "echo '  --global'; exit 1"),
            "version" => FAKE_BPM.replace("0.3.0", "0.3.1"),
            "version-status" => FAKE_BPM.replace("echo 'bpm 0.3.0'", "echo 'bpm 0.3.0'; exit 1"),
            _ => FAKE_BPM.to_string(),
        };
        let fixture = build_signed_release_with_binary(root.path(), &binary);
        let tarball = fixture.join(format!("bpm-{}.tar.gz", host_platform()));
        match failure {
            "missing" => std::fs::remove_file(&tarball).unwrap(),
            "signature" => std::fs::write(fixture.join("SHA256SUMS.sig"), b"invalid").unwrap(),
            "checksum" => std::fs::write(&tarball, b"invalid").unwrap(),
            _ => {}
        }
        let (fake, path) = release_path(&fixture, failure == "openssl");
        if failure == "platform" {
            let uname = fake.path().join("bin/uname");
            std::fs::write(&uname, "#!/bin/sh\necho unsupported\n").unwrap();
            make_executable(&uname);
        }
        let install_dir = root.path().join("install");
        std::fs::create_dir(&install_dir).unwrap();
        let out = installer_command(&path, &fixture.join("pubkey.pem"), &install_dir)
            .env("BPM_VERSION", "v0.3.0")
            .env("BPM_VERSION_EXPLICIT", "0")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(!out.status.success(), "accepted {failure}: {stdout}");
        assert!(
            stdout.contains("v0.3.0") && stdout.contains("source fallback is refused"),
            "{failure}: {stdout}"
        );
        assert!(
            !stdout.contains("falling back to source"),
            "{failure}: {stdout}"
        );
        assert!(!install_dir.join("bpm").exists(), "installed {failure}");
        assert_no_source_fallback(fake.path());
    }
}

#[test]
#[cfg(unix)]
fn real_cli_exposes_installer_capability_tokens() {
    for (command, token) in [("fetch", "--registry"), ("install", "--global")] {
        let output = Command::new(env!("CARGO_BIN_EXE_bpm"))
            .args([command, "--help"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains(token));
    }
}

#[test]
#[cfg(unix)]
fn valid_signed_release_installs_candidate() {
    let root = tempfile::tempdir().unwrap();
    let fixture = build_signed_release(root.path());
    let pubkey = fixture.join("pubkey.pem");
    let (_fake, path) = release_path(&fixture, false);
    let install_dir = root.path().join("install");
    std::fs::create_dir_all(&install_dir).unwrap();

    let out = run_installer(&path, &pubkey, &install_dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "install should succeed; stderr: {stderr}\nstdout: {stdout}"
    );
    assert!(
        install_dir.join("bpm").exists(),
        "candidate must be installed; stdout: {stdout}"
    );
    assert!(
        stdout.contains("Installing"),
        "expected install; stdout: {stdout}"
    );
}

#[test]
#[cfg(unix)]
fn tampered_tarball_falls_back_before_execution() {
    let root = tempfile::tempdir().unwrap();
    let fixture = build_signed_release(root.path());
    let pubkey = fixture.join("pubkey.pem");
    // Corrupt the tarball so its checksum no longer matches the signed manifest.
    let platform = host_platform();
    let tarball = fixture.join(format!("bpm-{platform}.tar.gz"));
    let mut bytes = std::fs::read(&tarball).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&tarball, bytes).unwrap();

    let (_fake, path) = release_path(&fixture, false);
    let install_dir = root.path().join("install");
    std::fs::create_dir_all(&install_dir).unwrap();
    let out = run_installer(&path, &pubkey, &install_dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Checksum mismatch must fail BEFORE extraction; no candidate is installed.
    assert!(
        stdout.contains("checksum mismatch"),
        "expected checksum mismatch; stdout: {stdout}"
    );
    assert!(
        !install_dir.join("bpm").exists(),
        "tampered tarball must not be installed"
    );
}

#[test]
#[cfg(unix)]
fn tampered_signature_falls_back_before_extraction() {
    let root = tempfile::tempdir().unwrap();
    let fixture = build_signed_release(root.path());
    let pubkey = fixture.join("pubkey.pem");
    let sig = fixture.join("SHA256SUMS.sig");
    let mut bytes = std::fs::read(&sig).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(&sig, bytes).unwrap();

    let (_fake, path) = release_path(&fixture, false);
    let install_dir = root.path().join("install");
    std::fs::create_dir_all(&install_dir).unwrap();
    let out = run_installer(&path, &pubkey, &install_dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("signature invalid"),
        "expected signature failure; stdout: {stdout}"
    );
    assert!(!install_dir.join("bpm").exists());
}

#[test]
#[cfg(unix)]
fn unsafe_archive_with_extra_file_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let fixture = build_signed_release(root.path());
    let pubkey = fixture.join("pubkey.pem");
    // Rebuild the tarball with an extra entry and re-sign a matching manifest.
    let staging = root.path().join("unsafe-staging");
    std::fs::create_dir_all(&staging).unwrap();
    let bpm = staging.join("bpm");
    std::fs::write(&bpm, FAKE_BPM).unwrap();
    make_executable(&bpm);
    std::fs::write(staging.join("evil"), "pwned").unwrap();
    let platform = host_platform();
    let tarball_name = format!("bpm-{platform}.tar.gz");
    let tarball = fixture.join(&tarball_name);
    let status = Command::new("tar")
        .args(["-czf"])
        .arg(&tarball)
        .args(["-C"])
        .arg(&staging)
        .args(["bpm", "evil"])
        .status()
        .unwrap();
    assert!(status.success());
    let hash = sha256_of(&tarball);
    std::fs::write(
        fixture.join("SHA256SUMS"),
        format!("{hash}  {tarball_name}\n"),
    )
    .unwrap();
    let status = Command::new("openssl")
        .args(["dgst", "-sha256", "-sign"])
        .arg(fixture.join("key.pem"))
        .args(["-out"])
        .arg(fixture.join("SHA256SUMS.sig"))
        .arg(fixture.join("SHA256SUMS"))
        .status()
        .unwrap();
    assert!(status.success());

    let (fake, path) = release_path(&fixture, false);
    let install_dir = root.path().join("install");
    std::fs::create_dir_all(&install_dir).unwrap();
    let out = installer_command(&path, &pubkey, &install_dir)
        .env("BPM_VERSION", "0.3.0")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("archive shape unsafe"),
        "expected unsafe-archive rejection; stdout: {stdout}"
    );
    assert!(!out.status.success());
    assert!(stdout.contains("source fallback is refused"));
    assert!(!install_dir.join("bpm").exists());
    assert_no_source_fallback(fake.path());
}

#[test]
#[cfg(unix)]
fn missing_openssl_falls_back_to_source() {
    let root = tempfile::tempdir().unwrap();
    let fixture = build_signed_release(root.path());
    let pubkey = fixture.join("pubkey.pem");
    let (_fake, path) = release_path(&fixture, true); // fake openssl exits 1
    let install_dir = root.path().join("install");
    std::fs::create_dir_all(&install_dir).unwrap();
    let out = run_installer(&path, &pubkey, &install_dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("signature invalid") || stdout.contains("openssl unavailable"),
        "expected signature/availability fallback; stdout: {stdout}"
    );
    assert!(!install_dir.join("bpm").exists());
}

/// Extract the first `-----BEGIN PUBLIC KEY-----... -----END PUBLIC KEY-----`
/// block from `text` (the installer embeds the release key in a heredoc).
#[cfg(unix)]
fn extract_pubkey_block(text: &str) -> Option<String> {
    let header = "-----BEGIN PUBLIC KEY-----";
    let footer = "-----END PUBLIC KEY-----";
    let start = text.find(header)?;
    let end = text[start..].find(footer)? + start + footer.len();
    Some(text[start..end].to_string())
}

/// Normalize a PEM block to its base64 body (header/footer/whitespace
/// stripped). Equal bodies imply equal DER key material.
#[cfg(unix)]
fn normalize_pubkey_body(pem: &str) -> String {
    pem.lines()
        .filter(|line| !line.starts_with("-----"))
        .flat_map(|line| line.chars())
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// Convert a PEM public-key block to raw DER bytes via `openssl`. The
/// installer already depends on openssl for signature verification, so this
/// is available wherever the installer runs.
#[cfg(unix)]
fn pubkey_to_der(pem: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pub.pem");
    std::fs::write(&path, pem).expect("write pem");
    let out = Command::new("openssl")
        .args(["pkey", "-pubin", "-outform", "DER"])
        .arg("-in")
        .arg(&path)
        .output()
        .expect("run openssl pkey");
    assert!(
        out.status.success(),
        "openssl pkey failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Regression guard: the production public key embedded in
/// `install.sh` must stay byte-aligned with `.github/release-signing-public.pem`
/// (the key the release workflow signs and verifies with). A future key
/// rotation fails this test until the installer is updated to match.
#[test]
#[cfg(unix)]
fn embedded_release_pubkey_matches_checked_in_key() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let installer_src =
        std::fs::read_to_string(manifest.join("install.sh")).expect("read install.sh");
    let checked_in_src =
        std::fs::read_to_string(manifest.join(".github/release-signing-public.pem"))
            .expect("read release-signing-public.pem");

    let embedded =
        extract_pubkey_block(&installer_src).expect("install.sh embeds a release public key");
    let checked_in =
        extract_pubkey_block(&checked_in_src).expect("checked-in key is a PEM public key");

    assert_eq!(
        normalize_pubkey_body(&embedded),
        normalize_pubkey_body(&checked_in),
        "install.sh embedded key body must match .github/release-signing-public.pem"
    );
    assert_eq!(
        pubkey_to_der(&embedded),
        pubkey_to_der(&checked_in),
        "embedded and checked-in key DER fingerprints must match"
    );
}
