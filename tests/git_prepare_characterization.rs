//! Compatibility oracle for npm's Git `prepare` lifecycle (Plan 004).
//!
//! This suite drives the real `git` and `node` binaries against a fully local
//! fixture to pin the Git-`prepare` contract. The BPM contract test is active
//! when those tools are available; the npm characterization oracle remains
//! ignored because it additionally depends on the host npm toolchain. The
//! observed behavior is the input to `docs/git-prepare-design.md`.
//!
//! Run the ignored npm oracle with:
//!
//! ```text
//! cargo test --test git_prepare_characterization -- --ignored --nocapture
//! ```
//!
//! Everything is local: a Git package repository with `file:` regular and dev
//! dependencies committed inside it, and a consumer that depends on a pinned
//! immutable commit. No public network and no credentials are used.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// A built local fixture: one Git package repository (`gitpkg`) with three
/// reachable commits and a mutable branch/tag, plus the tool versions recorded
/// at build time.
struct Fixture {
    /// Bare-ish working clone the consumer points at via `git+file://`.
    repo: PathBuf,
    /// Initial good commit (prepare writes `REV: 1`). `stable` + `v1.0.0`.
    good_rev1: String,
    /// Commit whose `prepare` throws (failure case).
    bad_prepare: String,
    /// Good commit whose `prepare` writes `REV: 2` (changed-identity case).
    good_rev2: String,
}

/// One recorded lifecycle line: phase name, context tag, dev/regular visibility.
#[derive(Debug)]
struct PhaseLine {
    phase: String,
    build_context: bool,
    dev_visible: bool,
    regular_visible: bool,
}

/// Probe whether a tool is runnable on PATH. The plan allows the whole oracle
/// to skip — never fail — when a required tool is missing.
fn tool_available(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn tool_version(tool: &str) -> String {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|_| "(missing)".to_owned())
}

/// `node -e "try { require('NAME'); process.exit(0) } catch { process.exit(1) }"`
/// from `dir`, returning whether the module is resolvable there.
fn node_resolves(dir: &Path, module: &str) -> bool {
    let script =
        format!("try {{ require({module:?}); process.exit(0) }} catch {{ process.exit(1) }}");
    Command::new("node")
        .arg("-e")
        .arg(&script)
        .current_dir(dir)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Run `npm install` in `dir` with an isolated cache and optional extra args.
fn npm_install(dir: &Path, cache: &Path, extra: &[&str]) -> Output {
    let mut cmd = Command::new("npm");
    cmd.arg("install")
        .arg("--cache")
        .arg(cache)
        .arg("--no-fund")
        .arg("--no-audit")
        .current_dir(dir);
    for arg in extra {
        cmd.arg(arg);
    }
    cmd.output().expect("failed to spawn npm")
}

/// Run the experimental BPM Git-prepare path against a local fixture.
fn bpm_install(dir: &Path, store: &Path, extra: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bpm"));
    cmd.arg("install")
        .arg("--store")
        .arg(store)
        .arg("--concurrency")
        .arg("1")
        .current_dir(dir);
    for arg in extra {
        cmd.arg(arg);
    }
    cmd.output().expect("failed to spawn bpm")
}

/// Configure deterministic git identity on a `Command` (no per-user config
/// required, no host-specific author).
fn git_env(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_AUTHOR_NAME", "bpm-oracle")
        .env("GIT_AUTHOR_EMAIL", "oracle@bpm.local")
        .env("GIT_COMMITTER_NAME", "bpm-oracle")
        .env("GIT_COMMITTER_EMAIL", "oracle@bpm.local")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// `package.json` for a tiny tool package inside the repo (`file:` dependency).
fn tool_pkg(name: &str) -> String {
    format!("{{ \"name\": \"{name}\", \"version\": \"1.0.0\", \"main\": \"index.js\" }}\n")
}

/// Build the local fixture. Returns the three commit SHAs and the repo path.
///
/// `gitpkg` carries a runtime dependency `regulartool` and a dev dependency
/// `devtool`, both committed as `file:` packages *inside* the repository so
/// they survive npm's clone-into-temp preparation step. `record.js` appends one
/// line per lifecycle phase with a BUILD/FINAL context tag and whether each
/// tool is `require()`-resolvable at that instant.
fn build_fixture() -> Fixture {
    let root = tempfile::tempdir().expect("tempdir for fixture");
    let repo = root.path().join("gitpkg");
    fs::create_dir_all(repo.join("vendor/regulartool")).unwrap();
    fs::create_dir_all(repo.join("vendor/devtool")).unwrap();

    write(
        &repo.join("vendor/regulartool/package.json"),
        &tool_pkg("regulartool"),
    );
    write(
        &repo.join("vendor/regulartool/index.js"),
        "module.exports = { MARKER: \"RT\" };\n",
    );
    write(
        &repo.join("vendor/devtool/package.json"),
        &tool_pkg("devtool"),
    );
    write(
        &repo.join("vendor/devtool/index.js"),
        "module.exports = { MARKER: \"DT\" };\n",
    );

    // package.json: six lifecycle hooks + a prepare that builds distributable
    // output requiring BOTH the regular and the dev tool.
    write(
        &repo.join("package.json"),
        r#"{
  "name": "gitpkg",
  "version": "1.0.0",
  "main": "index.js",
  "scripts": {
    "preprepare":  "node record.js preprepare",
    "prepare":     "node record.js prepare && node build-dist.js",
    "postprepare": "node record.js postprepare",
    "preinstall":  "node record.js preinstall",
    "install":     "node record.js install",
    "postinstall": "node record.js postinstall"
  },
  "dependencies":    { "regulartool": "file:./vendor/regulartool" },
  "devDependencies": { "devtool": "file:./vendor/devtool" }
}
"#,
    );
    // index.js re-exports the prepared dist; without prepare output it throws.
    write(
        &repo.join("index.js"),
        "module.exports = require(\"./dist/built.js\");\n",
    );
    // record.js: phase + context (BUILD inside the clone, FINAL in consumer
    // node_modules) + live require() probes for each tool.
    write(
        &repo.join("record.js"),
        r#"const fs = require("fs");
const path = require("path");
let dev = false, regular = false;
try { require("devtool"); dev = true; } catch (e) {}
try { require("regulartool"); regular = true; } catch (e) {}
const ctx = __dirname.split(path.sep).includes("node_modules") ? "FINAL" : "BUILD";
fs.appendFileSync(__dirname + "/phases.log",
  [process.argv[2], "ctx=" + ctx, "dev=" + (dev ? "yes" : "no"), "regular=" + (regular ? "yes" : "no")].join("\t") + "\n");
"#,
    );

    // build-dist.js: written per-commit (REV 1, throws, REV 2) by callers below.
    let git = |args: &[&str]| -> Output {
        let mut cmd = Command::new("git");
        git_env(&mut cmd).current_dir(&repo).args(args);
        cmd.output().expect("git command failed")
    };
    assert!(git(&["init", "-q"]).status.success());

    let build_dist_rev = |rev: u32| -> String {
        // Requires BOTH tools at prepare time and writes their resolved MARKER
        // values into a distributable module. REV is spliced per-commit via a
        // plain replace so the JS braces stay literal (no format!).
        let body = r#"const fs = require("fs");
let dev = "ABSENT", regular = "ABSENT";
try { regular = require("regulartool").MARKER; } catch (e) {}
try { dev = require("devtool").MARKER; } catch (e) {}
fs.mkdirSync(__dirname + "/dist", { recursive: true });
fs.writeFileSync(__dirname + "/dist/built.js",
  "module.exports = " + JSON.stringify({ built: true, regular: regular, dev: dev, REV: REV_VALUE }) + ";\n");
"#;
        body.replace("REV_VALUE", &rev.to_string())
    };

    // Commit 0: good prepare (REV 1).
    write(&repo.join("build-dist.js"), &build_dist_rev(1));
    assert!(git(&["add", "-A"]).status.success());
    assert!(git(&["commit", "-q", "-m", "gitpkg initial"])
        .status
        .success());
    let good_rev1 = sha_of(&repo);

    // Mutable refs at good_rev1: branch + tag.
    let _ = git(&["branch", "-f", "stable"]).status.success();
    let _ = git(&["tag", "-f", "v1.0.0"]).status.success();

    // Commit 1: prepare throws.
    write(
        &repo.join("build-dist.js"),
        r#"const fs = require("fs");
fs.mkdirSync(__dirname + "/dist", { recursive: true });
fs.writeFileSync(__dirname + "/dist/partial.js", "partial");
throw new Error("INTENTIONAL_PREPARE_FAILURE");
"#,
    );
    assert!(git(&["add", "-A"]).status.success());
    assert!(git(&["commit", "-q", "-m", "gitpkg prepare fails"])
        .status
        .success());
    let bad_prepare = sha_of(&repo);

    // Commit 2: good prepare (REV 2).
    write(&repo.join("build-dist.js"), &build_dist_rev(2));
    assert!(git(&["add", "-A"]).status.success());
    assert!(git(&["commit", "-q", "-m", "gitpkg prepare rev 2"])
        .status
        .success());
    let good_rev2 = sha_of(&repo);

    // Keep the tempdir alive for the whole process; leak it deliberately so the
    // consumer's `git+file://` URL stays valid across all oracle cases.
    std::mem::forget(root);

    Fixture {
        repo,
        good_rev1,
        bad_prepare,
        good_rev2,
    }
}

fn sha_of(repo: &Path) -> String {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(repo)
        .output()
        .expect("git rev-parse failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Read and parse `node_modules/gitpkg/phases.log`, returning every recorded
/// lifecycle line. Absent file ⇒ empty vector.
fn read_phases(consumer: &Path) -> Vec<PhaseLine> {
    let path = consumer.join("node_modules/gitpkg/phases.log");
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let phase = parts.next()?.to_owned();
            let ctx = parts.next()?;
            let dev = parts.next()?;
            let regular = parts.next()?;
            Some(PhaseLine {
                phase,
                build_context: ctx.contains("BUILD"),
                dev_visible: dev.contains("yes"),
                regular_visible: regular.contains("yes"),
            })
        })
        .collect()
}

/// Assert the BUILD-context preparation ran the canonical six-phase lifecycle
/// in order with both tools visible at every phase.
fn assert_build_full_visibility(phases: &[PhaseLine]) {
    let canonical = [
        "preinstall",
        "install",
        "postinstall",
        "preprepare",
        "prepare",
        "postprepare",
    ];
    let mut last_index = 0usize;
    for wanted in canonical {
        let pos = phases
            .iter()
            .enumerate()
            .skip(last_index)
            .find_map(|(i, p)| {
                (p.build_context && p.phase == wanted && p.dev_visible && p.regular_visible)
                    .then_some(i)
            });
        let pos = pos.unwrap_or_else(|| {
            panic!("BUILD context missing canonical phase '{wanted}' with dev+regular visible");
        });
        last_index = pos + 1;
    }
}

/// Assert the FINAL-context consumer install ran only the three install phases
/// (no prepare family) with only the runtime tool visible.
fn assert_final_install_only_runtime(phases: &[PhaseLine]) {
    let final_phases: Vec<&str> = phases
        .iter()
        .filter(|p| !p.build_context)
        .map(|p| p.phase.as_str())
        .collect();
    assert_eq!(
        final_phases,
        ["preinstall", "install", "postinstall"],
        "FINAL context must run only preinstall/install/postinstall, in order"
    );
    for p in phases.iter().filter(|p| !p.build_context) {
        assert!(
            !p.dev_visible,
            "devtool must NOT be visible in FINAL: {p:?}"
        );
        assert!(
            p.regular_visible,
            "regulartool must be visible in FINAL: {p:?}"
        );
    }
    // No prepare-family anywhere outside BUILD.
    assert!(
        phases
            .iter()
            .filter(|p| !p.build_context)
            .all(|p| !p.phase.contains("prepare")),
        "prepare-family scripts must not re-run in the FINAL consumer tree"
    );
}

/// Read `node_modules/gitpkg/dist/built.js` and parse its exported object.
fn built_module(consumer: &Path) -> Option<Value> {
    let path = consumer.join("node_modules/gitpkg/dist/built.js");
    let text = fs::read_to_string(&path).ok()?;
    // The fixture writes a literal module.exports = { ... };\n. Extract the
    // object literal and parse it; this is a controlled, local fixture.
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str::<Value>(&text[start..=end]).ok()
}

/// Read the consumer's package-lock v3 and return the `resolved` URL for gitpkg.
fn gitpkg_resolved(consumer: &Path) -> String {
    let lock = fs::read_to_string(consumer.join("package-lock.json")).unwrap();
    let value: Value = serde_json::from_str(&lock).unwrap();
    value["packages"]["node_modules/gitpkg"]["resolved"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn bpm_gitpkg_resolved_commit(consumer: &Path) -> String {
    let lock = fs::read_to_string(consumer.join("bpm.lock")).unwrap();
    let value: Value = serde_json::from_str(&lock).unwrap();
    value["resolution"]["packages"]["node_modules/gitpkg"]["source"]["resolvedCommit"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Fresh consumer project depending on `gitpkg` at `spec` (a `git+file://…#ref`).
fn consumer(spec: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("package.json"),
        &format!(
            r#"{{ "name": "consumer", "version": "1.0.0", "dependencies": {{ "gitpkg": "{spec}" }} }}
"#
        ),
    );
    dir
}

#[test]
fn bpm_git_prepare_contract() {
    if !tool_available("git") || !tool_available("node") {
        eprintln!("skipping BPM Git-prepare parity: missing git or node");
        return;
    }
    let fixture = build_fixture();
    let url = format!(
        "git+file://{}#{}",
        fixture.repo.display(),
        fixture.good_rev1
    );
    let dir = consumer(&url);
    let store = tempfile::tempdir().unwrap();
    let output = bpm_install(dir.path(), store.path(), &["--git-prepare"]);
    assert!(
        output.status.success(),
        "bpm install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let built = built_module(dir.path()).expect("BPM should publish prepared dist/built.js");
    assert_eq!(built["built"], true);
    assert_eq!(built["regular"], "RT");
    assert_eq!(built["dev"], "DT");
    let phases = read_phases(dir.path());
    assert_build_full_visibility(&phases);
    assert_final_install_only_runtime(&phases);
    assert!(!node_resolves(
        dir.path().join("node_modules/gitpkg").as_path(),
        "devtool"
    ));
    assert_eq!(bpm_gitpkg_resolved_commit(dir.path()), fixture.good_rev1);

    // --ignore-scripts bypasses preparation and ships the raw source only.
    let ignored_dir = consumer(&url);
    let ignored = bpm_install(
        ignored_dir.path(),
        store.path(),
        &["--git-prepare", "--ignore-scripts"],
    );
    assert!(ignored.status.success());
    assert!(ignored_dir.path().join("node_modules/gitpkg").exists());
    assert!(!ignored_dir
        .path()
        .join("node_modules/gitpkg/phases.log")
        .exists());
    assert!(!ignored_dir
        .path()
        .join("node_modules/gitpkg/dist/built.js")
        .exists());
    assert!(!node_resolves(ignored_dir.path(), "gitpkg"));

    // Mutable refs are resolved and persisted as immutable commits.
    for mutable_ref in ["stable", "v1.0.0"] {
        let mutable_dir = consumer(&format!(
            "git+file://{}#{mutable_ref}",
            fixture.repo.display()
        ));
        let mutable = bpm_install(mutable_dir.path(), store.path(), &["--git-prepare"]);
        assert!(mutable.status.success());
        assert_eq!(
            bpm_gitpkg_resolved_commit(mutable_dir.path()),
            fixture.good_rev1
        );
    }

    // A new source revision gets a distinct prepared output.
    let rev2_dir = consumer(&format!(
        "git+file://{}#{}",
        fixture.repo.display(),
        fixture.good_rev2
    ));
    let rev2 = bpm_install(rev2_dir.path(), store.path(), &["--git-prepare"]);
    assert!(rev2.status.success());
    assert_eq!(
        bpm_gitpkg_resolved_commit(rev2_dir.path()),
        fixture.good_rev2
    );
    assert_eq!(
        built_module(rev2_dir.path()).unwrap()["REV"],
        2,
        "a changed commit must not reuse the prior prepared image"
    );

    let rerun = bpm_install(dir.path(), store.path(), &["--git-prepare"]);
    assert!(rerun.status.success());
    assert!(String::from_utf8_lossy(&rerun.stdout).contains("graph volume reused"));
    assert_eq!(
        read_phases(dir.path()).len(),
        9,
        "cached prepare must not rerun scripts"
    );

    let bad_url = format!(
        "git+file://{}#{}",
        fixture.repo.display(),
        fixture.bad_prepare
    );
    let bad_dir = consumer(&bad_url);
    let bad = bpm_install(bad_dir.path(), store.path(), &["--git-prepare"]);
    assert!(
        !bad.status.success(),
        "failed prepare must fail installation"
    );
    assert!(!bad_dir.path().join("node_modules/gitpkg").exists());
}

#[test]
#[ignore = "requires local git, node, and npm on PATH"]
fn npm_git_prepare_contract() {
    if !tool_available("git") || !tool_available("node") || !tool_available("npm") {
        eprintln!(
            "skipping git-prepare oracle: missing tool (git={}, node={}, npm={})",
            tool_available("git"),
            tool_available("node"),
            tool_available("npm")
        );
        return;
    }
    // Print the exact recorded toolchain once.
    eprintln!(
        "[git-prepare oracle] git={} | node={} | npm={}",
        tool_version("git"),
        tool_version("node"),
        tool_version("npm")
    );

    let fixture = build_fixture();
    let url = format!("git+file://{}#{{ref}}", fixture.repo.display());

    // ── Case 1: immutable commit, default install ──────────────────────────
    {
        let dir = consumer(&url.replace("{ref}", &fixture.good_rev1));
        let cache = tempfile::tempdir().unwrap();
        let out = npm_install(dir.path(), cache.path(), &[]);
        assert!(
            out.status.success(),
            "default install must succeed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let phases = read_phases(dir.path());
        assert_build_full_visibility(&phases);
        assert_final_install_only_runtime(&phases);

        // Generated prepare output ships to the consumer and proves BOTH tools
        // were visible at prepare time.
        let built = built_module(dir.path()).expect("dist/built.js must ship to consumer");
        assert_eq!(built["built"], true);
        assert_eq!(built["regular"], "RT");
        assert_eq!(built["dev"], "DT");

        // Runtime dependency is resolvable from the consumer; dev is not.
        assert!(
            node_resolves(dir.path(), "gitpkg"),
            "consumer must require the prepared gitpkg"
        );
        assert!(
            node_resolves(dir.path(), "regulartool"),
            "runtime dependency regulartool must be installed for the consumer"
        );
        assert!(
            !node_resolves(dir.path(), "devtool"),
            "dev dependency devtool must NOT be installed for the consumer"
        );

        // Lock pins the immutable commit, byte-for-byte.
        let resolved = gitpkg_resolved(dir.path());
        let want = format!("#{}", fixture.good_rev1);
        assert!(
            resolved.ends_with(&want),
            "lock must pin immutable commit: resolved={resolved}, want suffix {want}"
        );

        // ── Case 3: second install (rerun) is a no-op for lifecycle ─────────
        let before = fs::read_to_string(dir.path().join("node_modules/gitpkg/phases.log"))
            .unwrap()
            .lines()
            .count();
        let cache2 = tempfile::tempdir().unwrap();
        let rerun = npm_install(dir.path(), cache2.path(), &[]);
        assert!(rerun.status.success(), "rerun must succeed");
        let after = fs::read_to_string(dir.path().join("node_modules/gitpkg/phases.log"))
            .unwrap()
            .lines()
            .count();
        assert_eq!(
            before, after,
            "npm must NOT re-run lifecycle/prepare on an unchanged no-op rerun"
        );
    }

    // ── Case 2: --ignore-scripts skips prepare and ships no generated output
    {
        let dir = consumer(&url.replace("{ref}", &fixture.good_rev1));
        let cache = tempfile::tempdir().unwrap();
        let out = npm_install(dir.path(), cache.path(), &["--ignore-scripts"]);
        assert!(
            out.status.success(),
            "--ignore-scripts install must still succeed"
        );
        assert!(
            !dir.path().join("node_modules/gitpkg/phases.log").exists(),
            "--ignore-scripts must skip ALL scripts (no phases.log)"
        );
        assert!(
            !dir.path()
                .join("node_modules/gitpkg/dist/built.js")
                .exists(),
            "--ignore-scripts must ship no prepare-generated dist"
        );
        assert!(
            !node_resolves(dir.path(), "gitpkg"),
            "unprepared gitpkg (no dist) must fail to require from consumer"
        );
        assert!(
            dir.path().join("node_modules/gitpkg").exists(),
            "the raw source package is still installed under --ignore-scripts"
        );
    }

    // ── Case 4: prepare failure rolls back atomically ──────────────────────
    {
        let dir = consumer(&url.replace("{ref}", &fixture.bad_prepare));
        let cache = tempfile::tempdir().unwrap();
        let out = npm_install(dir.path(), cache.path(), &[]);
        assert!(
            !out.status.success(),
            "a failing prepare must fail the install"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("INTENTIONAL_PREPARE_FAILURE"),
            "prepare error must surface: {stderr}"
        );
        assert!(
            !dir.path().join("node_modules/gitpkg").exists(),
            "a failed prepare must leave no package in node_modules (atomic rollback)"
        );
    }

    // ── Case 5: mutable branch/tag refs are pinned to immutable commits ────
    // (`stable` and `v1.0.0` are both pinned at good_rev1 in build_fixture; the
    // default branch is deliberately excluded because it advances to good_rev2.)
    for mutable_ref in ["stable", "v1.0.0"] {
        let dir = consumer(&url.replace("{ref}", mutable_ref));
        let cache = tempfile::tempdir().unwrap();
        let out = npm_install(dir.path(), cache.path(), &[]);
        assert!(
            out.status.success(),
            "install via mutable ref '{mutable_ref}' must succeed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let resolved = gitpkg_resolved(dir.path());
        assert!(
            resolved.ends_with(&format!("#{}", fixture.good_rev1)),
            "mutable ref '{mutable_ref}' must be pinned to immutable commit in lock: resolved={resolved}"
        );
        assert!(
            !resolved.ends_with(&format!("#{mutable_ref}")),
            "lock must never retain the mutable ref '{mutable_ref}': resolved={resolved}"
        );
    }

    // ── Case 6: a changed prepare at a new commit changes identity + output
    {
        let dir = consumer(&url.replace("{ref}", &fixture.good_rev2));
        let cache = tempfile::tempdir().unwrap();
        let out = npm_install(dir.path(), cache.path(), &[]);
        assert!(out.status.success(), "changed-commit install must succeed");
        let resolved = gitpkg_resolved(dir.path());
        assert!(
            resolved.ends_with(&format!("#{}", fixture.good_rev2)),
            "new commit must produce a new lock pin: resolved={resolved}"
        );
        let built = built_module(dir.path()).expect("REV 2 dist must ship");
        assert_eq!(
            built["REV"], 2,
            "new commit must ship its own prepared output"
        );
    }
}
