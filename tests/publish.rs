#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::process::Command;

mod common;

fn bpm_bin() -> &'static str {
    env!("CARGO_BIN_EXE_bpm")
}

#[test]
fn publish_rejects_symlink_pointing_outside_project() {
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("outside.txt");
    fs::write(&outside_file, "nope").unwrap();

    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    fs::write(project.path().join("index.js"), "module.exports = 1;\n").unwrap();
    symlink(&outside_file, project.path().join("outside-link")).unwrap();

    let server = common::MiniServer::start(Vec::new());

    let out = Command::new(bpm_bin())
        .arg("publish")
        .arg("--registry")
        .arg(server.url(""))
        .arg("--access")
        .arg("public")
        .current_dir(project.path())
        .output()
        .expect("run bpm publish");

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "publish must fail when symlink escapes project root: {stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("outside project root") || stdout.contains("outside project root"),
        "expected outside-scope symlink failure message: {stderr}\n{stdout}"
    );
    assert_eq!(
        server.requests().len(),
        0,
        "publish should fail before network request"
    );
}

#[test]
fn publish_rejects_all_symlinks_even_in_project_tree() {
    let project = tempfile::tempdir().unwrap();

    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    fs::write(project.path().join("index.js"), "module.exports = 1;\n").unwrap();
    let linked_target = project.path().join("index.js");
    symlink(&linked_target, project.path().join("local-link.js")).unwrap();

    let server = common::MiniServer::start(Vec::new());

    let out = Command::new(bpm_bin())
        .arg("publish")
        .arg("--registry")
        .arg(server.url(""))
        .arg("--access")
        .arg("public")
        .current_dir(project.path())
        .output()
        .expect("run bpm publish");

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "publish must fail for symlink entries: {stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("does not support symlink")
            || stdout.contains("does not support symlink")
            || stderr.contains("outside project root")
            || stdout.contains("outside project root"),
        "expected symlink failure message: {stderr}\n{stdout}"
    );
    assert_eq!(
        server.requests().len(),
        0,
        "publish should fail before network request"
    );
}
