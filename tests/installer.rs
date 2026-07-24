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

// ── Release provenance verification (plan 020) ───────────────────────────
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

    // Fake bpm that satisfies verify_binary (--registry on fetch, bin
    // directory on install).
    let staging = root.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let bpm = staging.join("bpm");
    std::fs::write(
        &bpm,
        "#!/bin/sh\ncase \"$1\" in\n  fetch) echo '  --registry <url>' ;;\n  install) echo '  bin directory' ;;\nesac\n",
    )
    .unwrap();
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

    // Sorted SHA256SUMS: one conventional `<64 hex>  <basename>` line.
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
            fix = fixture.display()
        ),
    )
    .unwrap();
    make_executable(&curl_script);

    let cargo_script = bin.join("cargo");
    std::fs::write(&cargo_script, "#!/bin/sh\nexit 1\n").unwrap();
    make_executable(&cargo_script);

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
    Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .env("PATH", path)
        .env("BPM_REPO", "https://github.com/Lbniese/bpm")
        .env("BPM_INSTALL_DIR", install_dir.to_str().unwrap())
        .env("_BPM_TEST_PUBKEY_FILE", pubkey)
        .output()
        .expect("run install.sh")
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
    std::fs::write(
        &bpm,
        "#!/bin/sh\ncase \"$1\" in\n  fetch) echo '  --registry <url>' ;;\n  install) echo '  bin directory' ;;\nesac\n",
    )
    .unwrap();
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

    let (_fake, path) = release_path(&fixture, false);
    let install_dir = root.path().join("install");
    std::fs::create_dir_all(&install_dir).unwrap();
    let out = run_installer(&path, &pubkey, &install_dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("archive shape unsafe"),
        "expected unsafe-archive rejection; stdout: {stdout}"
    );
    assert!(!install_dir.join("bpm").exists());
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
