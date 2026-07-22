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
        "file://{}",
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
                    "integrity": "{}",
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

/// `attach_project_local_with_backend(.., Reflink)` must succeed on Windows by
/// ignoring the reflink backend (unsupported on NTFS, no `clonefile`/`FICLONE`)
/// and hardlinking each package tree into the project, matching the
/// established `BPM_PROJECT_VIEW=local` shape. This is the safety net for the
/// current Windows fallback; when real junction/ReFS reflink support lands in
/// Plan 006 Phase 5, an additional test will cover that path.
#[test]
fn windows_attach_with_reflink_backend_falls_back_to_hardlink() {
    use bpm::materializer::{MaterializeBackend, MaterializeStats};
    use bpm::volume::{attach_project_local_with_backend, VolumeRef};

    let store = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let volume = store.path().join("node_modules");
    fs::create_dir_all(volume.join("demo")).unwrap();
    let payload: &[u8] = br#"{"name":"demo","version":"1.0.0"}"#;
    fs::write(volume.join("demo/package.json"), payload).unwrap();

    let volume_ref = VolumeRef {
        path: store.path().to_path_buf(),
        cached: false,
        stats: MaterializeStats::default(),
    };

    let stats =
        attach_project_local_with_backend(project.path(), &volume_ref, MaterializeBackend::Reflink)
            .expect("reflink backend must not error on windows (hardlink fallback)");
    assert_eq!(
        stats.stats.relays_created, 1,
        "one package should be attached"
    );
    // Plan 011: attachment records the entry it actually created. On the
    // NTFS hardlink/copy fallback this is a real directory recorded as
    // `local` with a versioned tree fingerprint.
    assert_eq!(
        stats.owned.len(),
        1,
        "attachment must record the owned entry"
    );
    let owned = &stats.owned[0];
    assert_eq!(owned.path, "node_modules/demo");
    assert!(
        !owned.identity.is_empty(),
        "owned entry must carry an identity"
    );
    assert_eq!(owned.mode, "local", "hardlink fallback records local mode");
    assert!(owned.identity.starts_with("tree-blake3-v1:"));

    let pkg = project.path().join("node_modules/demo/package.json");
    assert!(
        pkg.exists(),
        "hardlinked package should exist: {}",
        pkg.display()
    );
    assert_eq!(
        fs::read(&pkg).unwrap(),
        payload,
        "hardlinked content must match the volume source"
    );
}
