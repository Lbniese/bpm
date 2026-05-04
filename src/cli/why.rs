//! `bpm why` — show why a package is in the dependency tree.
//!
//! Walks the lockfile's dependency graph in reverse to find which packages
//! declare a dependency on the target package. The output shows the target
//! package version and each parent that depends on it, including the
//! dependency group (dependencies, optionalDependencies, peerDependencies).

use std::collections::BTreeSet;
use std::env;

use bpm::lockfile::PackageEntry;
use bpm::project_lock::find_project_lock;

pub(super) fn execute(target: &str) -> anyhow::Result<()> {
    let cwd = env::current_dir()?;
    let project_lock = find_project_lock(&cwd)?
        .ok_or_else(|| anyhow::anyhow!("no lockfile found (bpm.lock or package-lock.json)"))?;

    let lockfile = &project_lock.lockfile;

    // Collect all lockfile entries matching the target name.
    let target_packages: Vec<&PackageEntry> = lockfile
        .packages
        .iter()
        .filter(|p| p.name == target)
        .collect();

    if target_packages.is_empty() {
        anyhow::bail!("'{target}' not found in lockfile");
    }

    // Build a set of all package paths for fast parent lookups from
    // resolution metadata.
    let all_paths: BTreeSet<&str> = lockfile.packages.iter().map(|p| p.path.as_str()).collect();

    // Check whether the target is a direct root dependency.
    let is_root_dep = lockfile.root.dependencies.contains_key(target);

    let mut any_output = false;

    for tp in &target_packages {
        if any_output {
            // Separate multiple versions with a blank line.
            println!();
        }
        any_output = true;

        let version = &tp.version;
        println!("{target}@{version}");

        // 1. Root dependency check.
        if is_root_dep {
            let spec = &lockfile.root.dependencies[target];
            println!("  root: {}@{}", target, spec);
            // A root dependency is a direct dep; no need to look for parents.
            continue;
        }

        // 2. Find reverse edges from PackageEntry.dependencies.
        let mut parents: Vec<(&PackageEntry, &str)> = Vec::new();

        for other in &lockfile.packages {
            if other.path == tp.path {
                continue; // skip self
            }
            if let Some(spec) = other.dependencies.get(target) {
                parents.push((other, spec));
            }
        }

        // 3. Find reverse edges from PackageResolution metadata.
        for (path, res) in &lockfile.resolution.packages {
            // Skip if this IS the target package itself.
            if *path == tp.path {
                continue;
            }
            // Find the corresponding PackageEntry for this resolution.
            let Some(entry) = lockfile.packages.iter().find(|e| e.path == *path) else {
                continue;
            };
            if entry.path == tp.path {
                continue;
            }

            // Check each dependency group for references to the target.
            for (dep_name, dep) in &res.dependencies {
                if dep_name == target && all_paths.contains(dep.target.as_str()) {
                    parents.push((entry, &dep.spec));
                }
            }
            for (dep_name, dep) in &res.optional_dependencies {
                if dep_name == target && all_paths.contains(dep.target.as_str()) {
                    parents.push((entry, &dep.spec));
                }
            }
            for (dep_name, dep) in &res.peer_dependencies {
                if dep_name == target && all_paths.contains(dep.target.as_str()) {
                    parents.push((entry, &dep.spec));
                }
            }
        }

        // 4. Print parents (deduplicated).
        if parents.is_empty() {
            println!("  <no parents — direct or orphaned dependency>");
        } else {
            // Deduplicate by parent path.
            let mut seen = BTreeSet::new();
            for (parent, spec) in &parents {
                if seen.insert(parent.path.as_str()) {
                    println!(
                        "  {}@{} requires {target}@{}",
                        parent.name, parent.version, spec
                    );
                }
            }
        }
    }

    Ok(())
}
