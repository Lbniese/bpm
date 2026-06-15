//! Integration tests for repository/project root discovery.

use std::fs;
use std::path::{Path, PathBuf};

use bpm::project::{find_project_root, find_repository_root, ProjectError};
use tempfile::tempdir;

fn mkdirs(root: &Path, rel: &str) -> PathBuf {
    let p = root.join(rel);
    fs::create_dir_all(&p).unwrap();
    p
}

fn write(dir: &Path, name: &str, contents: &str) {
    fs::write(dir.join(name), contents).unwrap();
}

#[test]
fn finds_manifest_from_deep_subdirectory() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write(root, "package.json", r#"{"name":"root"}"#);

    let deep = mkdirs(root, "a/b/c");
    assert_eq!(find_project_root(&deep).unwrap(), root.to_path_buf());
}

#[test]
fn finds_nearest_manifest_above_cwd() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write(root, "package.json", r#"{"name":"root"}"#);
    let pkg = mkdirs(root, "packages/widget/src");
    write(&pkg.join(".."), "package.json", r#"{"name":"widget"}"#);

    assert_eq!(
        find_project_root(&pkg).unwrap(),
        root.join("packages/widget")
    );
}

#[test]
fn errors_without_any_manifest() {
    let tmp = tempdir().unwrap();
    let deep = mkdirs(tmp.path(), "x/y");
    let err = find_project_root(&deep).expect_err("no manifest");
    assert!(matches!(err, ProjectError::NoManifest { .. }));
}

#[test]
fn repository_root_climbs_to_git() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, "package.json", r#"{"name":"monorepo"}"#);
    let pkg = mkdirs(root, "apps/web/src");
    // apps/web/src/.. == apps/web
    write(&pkg.join(".."), "package.json", r#"{"name":"web"}"#);
    assert_eq!(find_project_root(&pkg).unwrap(), root.join("apps/web"));

    assert_eq!(find_repository_root(&pkg).unwrap(), root.to_path_buf());
}

#[test]
fn repository_root_falls_back_to_project_root_without_git() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write(root, "package.json", r#"{"name":"app"}"#);
    let sub = mkdirs(root, "src");

    assert_eq!(find_repository_root(&sub).unwrap(), root.to_path_buf());
}
