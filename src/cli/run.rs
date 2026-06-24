//! `package.json` lifecycle-script command orchestration.

use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

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
    let mut child = bpm::platform::script_command(command);
    child.current_dir(&cwd);
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
    let node = bpm::platform::find_executable(
        std::ffi::OsStr::new("node"),
        env::var_os("PATH").as_deref(),
    )
    .unwrap_or_else(|| PathBuf::from("node"));
    child.env("NODE", node);
    child.env("PATH", path_with_bin(&bin, env::var_os("PATH"))?);
    let status = child
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run script: {e}"))?;
    if !status.success() {
        anyhow::bail!("script '{script}' exited with status {:?}", status.code());
    }
    Ok(())
}

fn path_with_bin(bin: &Path, inherited: Option<OsString>) -> anyhow::Result<OsString> {
    let mut paths = vec![PathBuf::from(bin)];
    if let Some(inherited) = inherited {
        paths.extend(env::split_paths(&inherited));
    }
    env::join_paths(paths)
        .map_err(|error| anyhow::anyhow!("could not construct PATH for lifecycle script: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{env, path::PathBuf};

    use super::path_with_bin;

    #[test]
    fn prepends_bin_using_platform_path_separator() {
        let inherited = env::join_paths([PathBuf::from("first"), PathBuf::from("second")]).unwrap();
        let joined = path_with_bin(
            PathBuf::from("node_modules/.bin").as_path(),
            Some(inherited),
        )
        .unwrap();

        assert_eq!(
            env::split_paths(&joined).collect::<Vec<_>>(),
            vec![
                PathBuf::from("node_modules/.bin"),
                PathBuf::from("first"),
                PathBuf::from("second")
            ]
        );
    }
}
