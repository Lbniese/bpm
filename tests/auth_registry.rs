mod common;

use std::fs;
use std::process::Command;

use common::{MiniServer, RouteBody};

#[test]
fn whoami_accepts_path_registry_auth_at_the_base_boundary() {
    let server = MiniServer::start_routed(|path| match path {
        "/npm/-/whoami" => Some(RouteBody(
            br#"{"username":"path-user"}"#.to_vec(),
            "application/json",
        )),
        _ => None,
    });
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();

    let registry = server.url("npm/");
    let authority = registry
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    fs::write(
        home.join(".npmrc"),
        format!("registry={registry}\n//{authority}/npm/:_authToken=integration-sentinel\n"),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bpm"))
        .arg("whoami")
        .current_dir(&project)
        .env("HOME", &home)
        .output()
        .expect("run bpm whoami");

    assert!(
        output.status.success(),
        "whoami failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "path-user");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/npm/-/whoami");
    assert!(requests[0].header("authorization").is_some());
}
