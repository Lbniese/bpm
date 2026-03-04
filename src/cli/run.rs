//! `package.json` lifecycle-script command orchestration.

use std::{env, ffi::OsString, path::MAIN_SEPARATOR, process::Command};

use bpm::manifest::PackageManifest;

pub(super) fn run(script: &str) -> anyhow::Result<()> {
    let cwd = env::current_dir()?;
    let manifest = PackageManifest::from_path(&cwd.join("package.json"))
        .map_err(|e| anyhow::anyhow!("no readable package.json in {}: {e}", cwd.display()))?;
    let command = manifest
        .scripts
        .get(script)
        .ok_or_else(|| anyhow::anyhow!("script '{script}' is not defined in package.json"))?;

    let bin = cwd.join("node_modules").join(".bin");
    let mut child = Command::new("sh");
    child.arg("-c").arg(command).current_dir(&cwd);
    child.env("npm_lifecycle_event", script);
    child.env("npm_lifecycle_script", command);
    child.env(
        "npm_package_name",
        manifest.name.clone().unwrap_or_default(),
    );
    child.env(
        "npm_package_version",
        manifest.version.clone().unwrap_or_default(),
    );
    child.env("npm_config_user_agent", "bpm/0.1.0");
    child.env("npm_execpath", "bpm");
    child.env("INIT_CWD", &cwd);
    child.env("NODE", which("node").unwrap_or_else(|| "node".into()));
    if let Some(path) = env::var_os("PATH") {
        let mut new_path = OsString::from(&bin);
        new_path.push(MAIN_SEPARATOR.to_string());
        new_path.push(path);
        child.env("PATH", new_path);
    }
    let status = child
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run script: {e}"))?;
    if !status.success() {
        anyhow::bail!("script '{script}' exited with status {:?}", status.code());
    }
    Ok(())
}

fn which(tool: &str) -> Option<String> {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
}
