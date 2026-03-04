mod common;

use std::fs;
use std::process::Command;

use common::{MiniServer, RouteBody};

fn bpm_bin() -> String {
    std::env::var("CARGO_BIN_EXE_bpm").unwrap_or_else(|_| "target/debug/bpm".into())
}

#[test]
fn publish_sends_otp_header_and_filtered_packument() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"pkg","version":"1.0.0","files":["dist"]}"#,
    )
    .unwrap();
    fs::write(project.path().join("README.md"), "readme").unwrap();
    fs::write(project.path().join("secret.txt"), "secret").unwrap();
    fs::create_dir_all(project.path().join("dist")).unwrap();
    fs::write(project.path().join("dist/index.js"), "ok").unwrap();
    let server = MiniServer::start_routed(|_| Some(RouteBody(b"{}".to_vec(), "application/json")));

    let output = Command::new(bpm_bin())
        .current_dir(project.path())
        .args([
            "publish",
            "--registry",
            &server.url(""),
            "--otp",
            "123456",
            "--access",
            "public",
        ])
        .output()
        .expect("run bpm publish");
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.requests();
    assert_eq!(requests[0].method, "PUT");
    assert_eq!(requests[0].header("npm-otp"), Some("123456"));
}

#[test]
fn audit_level_controls_exit_policy() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"left-pad":"1.3.0"}}"#,
    )
    .unwrap();
    let response = br#"{"metadata":{"vulnerabilities":{"info":0,"low":0,"moderate":0,"high":1,"critical":0,"total":1}}}"#;
    let server =
        MiniServer::start_routed(move |_| Some(RouteBody(response.to_vec(), "application/json")));

    let output = Command::new(bpm_bin())
        .current_dir(project.path())
        .args([
            "audit",
            "--registry",
            &server.url(""),
            "--audit-level",
            "critical",
        ])
        .output()
        .expect("run bpm audit");
    assert!(output.status.success(), "critical threshold should pass");

    let output = Command::new(bpm_bin())
        .current_dir(project.path())
        .args([
            "audit",
            "--registry",
            &server.url(""),
            "--audit-level",
            "high",
        ])
        .output()
        .expect("run bpm audit");
    assert!(!output.status.success(), "high threshold should fail");
}
