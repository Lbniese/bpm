//! Windows-only end-to-end CLI tests for frozen registry-package installs,
//! bin shims, archive symlinks, and local hardlink/copy project views.
//!
//! Modeled after `tests/install.rs` (Unix symlink relay) and
//! `tests/network_pipeline.rs` (local registry mock). Builds local npm-style
//! tarballs in memory, writes a canonical lock, and drives the real BPM binary.
//! Does not require Developer Mode, administrator rights, or network.

#![cfg(windows)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Path to the compiled `bpm` binary.
fn bpm_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bpm"))
}

/// Run `bpm` with `args` in `cwd`, using `store` as the store root.
fn run_bpm(args: &[&str], cwd: &Path, store: &Path) -> std::process::Output {
    Command::new(bpm_bin())
        .args(args)
        .current_dir(cwd)
        .env("BPM_STORE", store)
        .output()
        .expect("run bpm")
}

/// Copy of `build_tgz` for use on Windows without needing the common module's
/// generic `build`-callback signature.
fn build_tgz(files: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut buf = Vec::new();
    let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::none());
    let mut builder = tar::Builder::new(enc);
    for (path, data, mode) in files {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(data.len() as u64);
        header.set_mode(*mode);
        header.set_cksum();
        builder.append(&header, &data[..]).unwrap();
    }
    let enc = builder.into_inner().unwrap();
    enc.finish().unwrap();
    buf
}

fn integrity_of(bytes: &[u8]) -> String {
    use bpm::integrity::Sha512Digest;
    Sha512Digest::hash_bytes(bytes).to_npm_string()
}

#[test]
fn windows_frozen_install_materializes_packages_and_bins() {
    let tgz = build_tgz(&[
        (
            "package/package.json",
            br#"{"name":"demo","version":"1.0.0","bin":{"demo":"./cli.js"}}"# as &[u8],
            0o644,
        ),
        (
            "package/cli.js",
            b"#!/usr/bin/env node\nconsole.log('demo');\n",
            0o755,
        ),
    ]);
    let store = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let tarball_path = store.path().join("demo-1.0.0.tgz");
    fs::write(&tarball_path, &tgz).unwrap();
    let integrity = integrity_of(&tgz);
    let tarball_url = format!(
        "file:///{}",
        tarball_path.display().to_string().replace('\\', "/")
    );
    let lockfile_content = format!(
        r#"{{
            "lockfileVersion": 2,
            "generator": "bpm",
            "root": {{"dependencies":{{"demo":"^1.0.0"}}}},
            "resolution": {{"root":{{"dependencies":{{"demo":"^1.0.0"}}}}}},
            "packages": [
                {{
                    "path": "node_modules/demo",
                    "name": "demo",
                    "version": "1.0.0",
                    "resolved": "{}",
                    "integrity": "sha512-{}",
                    "bin": {{"demo":"./cli.js"}}
                }}
            ]
        }}"#,
        tarball_url, integrity
    );
    fs::write(project.path().join("bpm.lock"), &lockfile_content).unwrap();

    let out = run_bpm(&["install", "--frozen"], project.path(), store.path());
    assert!(
        out.status.success(),
        "frozen install failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let demo_dir = project.path().join("node_modules/demo");
    assert!(
        demo_dir.join("package.json").exists(),
        "package.json should be extracted"
    );
    assert!(
        demo_dir.join("cli.js").exists(),
        "cli.js should be extracted"
    );
    // Verify .cmd and .ps1 bin shims are generated.
    let cmd_shim = project.path().join("node_modules/.bin/demo.cmd");
    assert!(
        cmd_shim.exists(),
        "cmd shim should exist: {}",
        cmd_shim.display()
    );
    let ps1_shim = project.path().join("node_modules/.bin/demo.ps1");
    assert!(
        ps1_shim.exists(),
        "ps1 shim should exist: {}",
        ps1_shim.display()
    );
}
