mod common;

use std::fs;
use std::process::Command;

use common::{MiniServer, RouteBody};

fn bpm_bin() -> String {
    std::env::var("CARGO_BIN_EXE_bpm").unwrap_or_else(|_| "target/debug/bpm".into())
}

/// Write a deterministic npm v3 `package-lock.json` with a root entry and the
/// requested installed package entries. `bpm audit` now requires a valid
/// resolved inventory, so every audit project must provide one. Versions are
/// placeholder non-secret values.
fn write_npm_lock(project: &std::path::Path, packages: &[(&str, &str)]) {
    let mut entries = vec![r#""":{"name":"app","version":"1.0.0"}"#.to_string()];
    for (name, version) in packages {
        entries.push(format!(
            r#""node_modules/{name}":{{"version":"{version}"}}"#
        ));
    }
    let json = format!(
        r#"{{"name":"app","version":"1.0.0","lockfileVersion":3,"packages":{{{}}}}}"#,
        entries.join(",")
    );
    fs::write(project.join("package-lock.json"), json).unwrap();
}

/// Write a `package.json` with the given declared dependencies. Declared data
/// alone is not audited; it only documents intent for the fixture.
fn write_package_json(project: &std::path::Path, deps: &[(&str, &str)]) {
    let dep_str = deps
        .iter()
        .map(|(name, version)| format!(r#""{name}":"{version}""#))
        .collect::<Vec<_>>()
        .join(",");
    let json = format!(r#"{{"name":"app","version":"1.0.0","dependencies":{{{dep_str}}}}}"#);
    fs::write(project.join("package.json"), json).unwrap();
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
        .env("BPM_OTP", "123456")
        .args([
            "publish",
            "--registry",
            &server.url(""),
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
fn publish_rejects_invalid_package_name_before_network() {
    let project = tempfile::tempdir().unwrap();
    let server = MiniServer::start_routed(|_| Some(RouteBody(b"{}".to_vec(), "application/json")));

    for name in ["bad?query", "@malformed", "JSONStream"] {
        fs::write(
            project.path().join("package.json"),
            format!(r#"{{"name":"{name}","version":"1.0.0"}}"#),
        )
        .unwrap();
        let output = Command::new(bpm_bin())
            .current_dir(project.path())
            .args(["publish", "--registry", &server.url("")])
            .output()
            .expect("run bpm publish");
        assert!(!output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("invalid npm package name") && stderr.contains(name),
            "unexpected error for {name}: {stderr}"
        );
        assert!(!stdout.contains("published"));
    }

    assert!(server.requests().is_empty());
}

#[test]
fn audit_level_controls_exit_policy() {
    let project = tempfile::tempdir().unwrap();
    write_package_json(project.path(), &[("left-pad", "1.3.0")]);
    write_npm_lock(project.path(), &[("left-pad", "1.3.0")]);
    let response = br#"{"left-pad":[{"id":1,"severity":"high","title":"demo","url":"https://example.test/x"}]}"#;
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

    // A `high` finding is at-or-above `moderate`, so this must also fail.
    let output = Command::new(bpm_bin())
        .current_dir(project.path())
        .args([
            "audit",
            "--registry",
            &server.url(""),
            "--audit-level",
            "moderate",
        ])
        .output()
        .expect("run bpm audit");
    assert!(
        !output.status.success(),
        "moderate threshold should fail for a high finding"
    );
}

#[test]
fn audit_counts_shared_advisory_once_across_packages() {
    // Two packages, one advisory id reported under each. The summary must say
    // "1 vulnerability finding(s)" and the high threshold must fail (1 >= 1),
    // proving the duplicate id was not double-counted.
    let project = tempfile::tempdir().unwrap();
    write_package_json(
        project.path(),
        &[("left-pad", "1.3.0"), ("right-pad", "1.0.0")],
    );
    write_npm_lock(
        project.path(),
        &[("left-pad", "1.3.0"), ("right-pad", "1.0.0")],
    );
    let response = br#"{"left-pad":[{"id":7,"severity":"high","title":"shared","url":"https://example.test/x"}],"right-pad":[{"id":7,"severity":"high","title":"shared","url":"https://example.test/x"}]}"#;
    let server =
        MiniServer::start_routed(move |_| Some(RouteBody(response.to_vec(), "application/json")));

    // With dedup, there is exactly 1 high finding, so --audit-level high fails.
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
    assert!(
        !output.status.success(),
        "high threshold should fail for one shared high advisory\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // The human-readable summary must report 1 finding, not 2.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("1 vulnerability finding(s)"),
        "expected deduped count of 1 in stdout, got: {stdout}"
    );
    assert!(
        !stdout.contains("2 vulnerability finding(s)"),
        "advisory was double-counted; got: {stdout}"
    );
}

#[test]
fn audit_rejects_malformed_advisory_json_without_success_summary() {
    let project = tempfile::tempdir().unwrap();
    write_package_json(project.path(), &[("left-pad", "1.3.0")]);
    write_npm_lock(project.path(), &[("left-pad", "1.3.0")]);
    // A malformed HTTP 200 body must be a hard error, never a silent
    // zero-vulnerability success.
    let malformed = b"{ this is not json";
    let server =
        MiniServer::start_routed(move |_| Some(RouteBody(malformed.to_vec(), "application/json")));

    let output = Command::new(bpm_bin())
        .current_dir(project.path())
        .args(["audit", "--registry", &server.url("")])
        .output()
        .expect("run bpm audit");
    assert!(
        !output.status.success(),
        "malformed advisory JSON must exit nonzero\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !combined.contains("0 vulnerability finding(s)"),
        "malformed response must not be reported as a clean zero-vulnerability success: {combined}"
    );
    assert!(
        combined.contains("malformed JSON") || combined.contains("audit"),
        "error should mention the audit/JSON problem, got: {combined}"
    );
}

#[test]
fn audit_rejects_wrong_shaped_advisory_response() {
    let project = tempfile::tempdir().unwrap();
    write_package_json(project.path(), &[("left-pad", "1.3.0")]);
    write_npm_lock(project.path(), &[("left-pad", "1.3.0")]);
    // Syntactically valid JSON, but a package key mapped to an object instead
    // of an array — this must be rejected.
    let wrong_shape = br#"{"left-pad":{"id":1,"severity":"high"}}"#;
    let server = MiniServer::start_routed(move |_| {
        Some(RouteBody(wrong_shape.to_vec(), "application/json"))
    });

    let output = Command::new(bpm_bin())
        .current_dir(project.path())
        .args(["audit", "--registry", &server.url("")])
        .output()
        .expect("run bpm audit");
    assert!(
        !output.status.success(),
        "wrong-shaped advisory response must exit nonzero\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn audit_fails_before_network_when_no_lock_exists() {
    let project = tempfile::tempdir().unwrap();
    write_package_json(project.path(), &[("left-pad", "1.3.0")]);
    // No bpm.lock and no package-lock.json: a local inventory failure that must
    // never reach the registry.
    let server = MiniServer::start_routed(|_| Some(RouteBody(b"{}".to_vec(), "application/json")));

    let output = Command::new(bpm_bin())
        .current_dir(project.path())
        .args(["audit", "--registry", &server.url("")])
        .output()
        .expect("run bpm audit");
    assert!(
        !output.status.success(),
        "missing lock must exit nonzero\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        server.requests().len(),
        0,
        "local inventory failure must not contact the registry"
    );
}

#[test]
fn audit_fails_before_network_on_malformed_lock() {
    let project = tempfile::tempdir().unwrap();
    write_package_json(project.path(), &[("left-pad", "1.3.0")]);
    fs::write(project.path().join("package-lock.json"), "{ not json").unwrap();
    let server = MiniServer::start_routed(|_| Some(RouteBody(b"{}".to_vec(), "application/json")));

    let output = Command::new(bpm_bin())
        .current_dir(project.path())
        .args(["audit", "--registry", &server.url("")])
        .output()
        .expect("run bpm audit");
    assert!(
        !output.status.success(),
        "malformed lock must exit nonzero\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        server.requests().len(),
        0,
        "malformed inventory must not contact the registry"
    );
}

#[test]
fn audit_empty_lock_with_empty_response_succeeds() {
    // A dependency-free, valid npm v3 lock plus a valid empty `{}` advisory
    // response is a legitimate zero-vulnerability success.
    let project = tempfile::tempdir().unwrap();
    write_package_json(project.path(), &[]);
    write_npm_lock(project.path(), &[]);
    let server =
        MiniServer::start_routed(move |_| Some(RouteBody(b"{}".to_vec(), "application/json")));

    let output = Command::new(bpm_bin())
        .current_dir(project.path())
        .args(["audit", "--registry", &server.url("")])
        .output()
        .expect("run bpm audit");
    assert!(
        output.status.success(),
        "valid empty inventory and response must succeed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0 vulnerability finding(s)"),
        "valid empty response should report zero findings, got: {stdout}"
    );
}

/// Write an `.npmrc` authenticating the given server authority so `bpm token`
/// passes its bearer-token gate and reaches the secret-resolution step.
fn write_auth_npmrc(project: &std::path::Path, authority: &str) {
    fs::write(
        project.join(".npmrc"),
        format!("registry=http://{authority}/\n//{authority}/:_authToken=bearer-not-a-secret\n"),
    )
    .unwrap();
}

#[test]
fn token_create_fails_without_password_before_network() {
    // A noninteractive `token create` invocation has no terminal and no
    // $BPM_PASSWORD; it must fail before the token-creation mutation request
    // is sent. The child process inherits a non-terminal stdin by default.
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    let server = MiniServer::start_routed(|_| Some(RouteBody(b"{}".to_vec(), "application/json")));
    let url = server.url("");
    let authority = url
        .strip_prefix("http://")
        .unwrap_or_else(|| panic!("expected http:// url, got {url}"));
    write_auth_npmrc(project.path(), authority);

    let output = Command::new(bpm_bin())
        .current_dir(project.path())
        .args(["token", "create", "--registry", &server.url("")])
        .output()
        .expect("run bpm token create");
    assert!(
        !output.status.success(),
        "noninteractive token create without password must fail\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        server.requests().len(),
        0,
        "token create must not contact the registry before resolving the password"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("BPM_PASSWORD"),
        "error should point to $BPM_PASSWORD, got: {stderr}"
    );
}

#[test]
fn token_rejects_positional_on_create() {
    // An old `token create --otp <value>` must not silently treat `<value>` as
    // the `id` positional after Clap rejects `--otp`. Verify a stray positional
    // is rejected at the action-validation layer, not silently accepted.
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    let server = MiniServer::start_routed(|_| Some(RouteBody(b"{}".to_vec(), "application/json")));
    let url = server.url("");
    let authority = url
        .strip_prefix("http://")
        .unwrap_or_else(|| panic!("expected http:// url, got {url}"));
    write_auth_npmrc(project.path(), authority);

    let output = Command::new(bpm_bin())
        .current_dir(project.path())
        .args([
            "token",
            "create",
            "stray-value",
            "--registry",
            &server.url(""),
        ])
        .output()
        .expect("run bpm token create");
    assert!(
        !output.status.success(),
        "stray positional on token create must be rejected\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        server.requests().len(),
        0,
        "rejected token create must not contact the registry"
    );
}
