//! `bpm ls` — list installed packages as a dependency tree (npm `ls` compat).
//!
//! Reads the project lockfile (`bpm.lock` or `package-lock.json`) and renders
//! the resolved dependency tree. Edges are taken from the v2 resolver metadata
//! (`resolution.packages[path].dependencies[].target`) when present, and
//! otherwise reconstructed from the compatibility `PackageEntry.dependencies`
//! map using npm's `node_modules` lookup order (nearest enclosing
//! `node_modules/<name>` that exists).
//!
//! Diamonds and cycles are bounded: by default each physical package is
//! expanded once (re-encounters are shown without their children, matching
//! `npm ls`); `--all` expands every occurrence, breaking only true cycles.

use std::collections::{BTreeMap, BTreeSet};
use std::env;

use bpm::lockfile::{Lockfile, PackageEntry};
use bpm::project_lock::find_project_lock;
use serde::Serialize;

pub(super) struct Options {
    /// Optional positional package-name filter; only paths leading to a
    /// matching package are shown.
    pub filter: Option<String>,
    /// Expand every occurrence of a package instead of deduplicating.
    pub all: bool,
    /// Maximum levels to expand below the root (`None` = unlimited).
    pub depth: Option<usize>,
    /// Emit machine-readable JSON.
    pub json: bool,
}

pub(super) fn run(opts: Options) -> anyhow::Result<()> {
    let cwd = env::current_dir()?;
    let project_lock = find_project_lock(&cwd)?
        .ok_or_else(|| anyhow::anyhow!("no lockfile found (bpm.lock or package-lock.json)"))?;
    let lockfile = &project_lock.lockfile;

    // Physical placement lookup: node_modules path -> package entry.
    let by_path: BTreeMap<&str, &PackageEntry> = lockfile
        .packages
        .iter()
        .map(|p| (p.path.as_str(), p))
        .collect();

    let root_name = lockfile
        .root
        .name
        .clone()
        .or_else(|| cwd.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| ".".to_string());
    let root_version = lockfile.root.version.clone();

    // Root direct dependencies: union of the compatibility map and the v2
    // resolver's split dev/optional groups, so dev and optional deps are
    // included regardless of lockfile generation.
    let root_dep_names: BTreeSet<String> = lockfile
        .root
        .dependencies
        .keys()
        .cloned()
        .chain(lockfile.resolution.root.dev_dependencies.keys().cloned())
        .chain(
            lockfile
                .resolution
                .root
                .optional_dependencies
                .keys()
                .cloned(),
        )
        .collect();

    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = Vec::new();
    let children: Vec<Node> = root_dep_names
        .iter()
        .filter_map(|name| {
            let target = format!("node_modules/{name}");
            build_node(
                &target,
                &by_path,
                lockfile,
                opts.all,
                opts.depth,
                &mut visited,
                &mut stack,
            )
        })
        .collect();

    let mut root = Node {
        name: root_name,
        version: root_version.clone().unwrap_or_default(),
        version_is_root: true,
        children,
    };

    // Apply the optional positional filter by pruning to ancestor chains of
    // any matching package.
    if let Some(filter) = &opts.filter {
        prune_to(&mut root, filter);
        if root.children.is_empty() {
            anyhow::bail!("no package matching '{filter}' in the dependency tree");
        }
    }

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&node_to_json(&root))?);
    } else {
        print_tree(&root);
    }

    Ok(())
}

/// One node in the rendered dependency tree.
#[derive(Debug, Clone)]
struct Node {
    name: String,
    version: String,
    /// Distinguish the synthetic root (which may omit its version) from real
    /// packages (which always carry a version).
    version_is_root: bool,
    children: Vec<Node>,
}

/// Recursively build a tree node for the package at `target`, or `None` if no
/// package is physically placed there.
///
/// `visited` tracks packages already expanded (used to deduplicate when
/// `all` is false). `stack` tracks the current recursion path and breaks true
/// cycles even under `--all`.
#[allow(clippy::too_many_arguments)]
fn build_node(
    target: &str,
    by_path: &BTreeMap<&str, &PackageEntry>,
    lockfile: &Lockfile,
    all: bool,
    remaining: Option<usize>,
    visited: &mut BTreeSet<String>,
    stack: &mut Vec<String>,
) -> Option<Node> {
    let entry = by_path.get(target)?;
    let mut node = Node {
        name: entry.name.clone(),
        version: entry.version.clone(),
        version_is_root: false,
        children: Vec::new(),
    };

    let depth_limited = remaining.is_some_and(|r| r == 0);
    let on_stack = stack.iter().any(|s| s == target);
    let already_expanded = !all && visited.contains(target);

    if depth_limited || on_stack || already_expanded {
        return Some(node);
    }

    visited.insert(target.to_string());
    stack.push(target.to_string());

    for (_dep_name, dep_target) in resolve_edges(lockfile, entry, by_path) {
        if let Some(child) = build_node(
            &dep_target,
            by_path,
            lockfile,
            all,
            remaining.map(|r| r.saturating_sub(1)),
            visited,
            stack,
        ) {
            node.children.push(child);
        }
    }

    stack.pop();
    Some(node)
}

/// Resolve the outgoing dependency edges of `entry` to physical child paths.
///
/// Prefers the v2 resolver metadata (exact `target`), falling back to
/// `node_modules` lookup-order reconstruction from the compatibility
/// `dependencies` map.
fn resolve_edges(
    lockfile: &Lockfile,
    entry: &PackageEntry,
    by_path: &BTreeMap<&str, &PackageEntry>,
) -> Vec<(String, String)> {
    let mut edges: Vec<(String, String)> = Vec::new();
    if let Some(res) = lockfile.resolution.packages.get(&entry.path) {
        for (name, dep) in &res.dependencies {
            edges.push((name.clone(), dep.target.clone()));
        }
        for (name, dep) in &res.optional_dependencies {
            edges.push((name.clone(), dep.target.clone()));
        }
        for (name, dep) in &res.peer_dependencies {
            edges.push((name.clone(), dep.target.clone()));
        }
    } else {
        for name in entry.dependencies.keys() {
            if let Some(target) = resolve_dep_target(&entry.path, name, by_path) {
                edges.push((name.clone(), target));
            }
        }
    }
    edges.sort_by(|a, b| a.0.cmp(&b.0));
    edges
}

/// Resolve a single declared dependency `name` (required by the package at
/// `pkg_path`) to its physical `node_modules` placement, following npm's
/// lookup order: nearest enclosing `node_modules/<name>` that exists.
fn resolve_dep_target(
    pkg_path: &str,
    name: &str,
    by_path: &BTreeMap<&str, &PackageEntry>,
) -> Option<String> {
    dep_candidates(pkg_path, name)
        .into_iter()
        .find(|candidate| by_path.contains_key(candidate.as_str()))
}

/// Yield candidate physical paths for `name` required by the package at
/// `pkg_path`, deepest-first (npm lookup order).
fn dep_candidates(pkg_path: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = pkg_path.to_string();
    loop {
        out.push(format!("{cur}/node_modules/{name}"));
        match cur.rfind("/node_modules/") {
            Some(idx) => cur.truncate(idx),
            None => {
                // `cur` is now a top-level placement like `node_modules/<pkg>`;
                // the last candidate is the top-level `node_modules/<name>`.
                if cur.strip_prefix("node_modules/").is_some() {
                    out.push(format!("node_modules/{name}"));
                }
                break;
            }
        }
    }
    out
}

/// Prune `node` in place to only branches that contain a package whose name
/// equals `filter`. Returns whether this subtree should be kept.
fn prune_to(node: &mut Node, filter: &str) -> bool {
    let mut kept = Vec::new();
    for mut child in node.children.drain(..) {
        if prune_to(&mut child, filter) {
            kept.push(child);
        }
    }
    node.children = kept;
    !node.children.is_empty() || node.name == filter
}

/// Render the tree with box-drawing connectors (npm `ls` style).
fn print_tree(root: &Node) {
    if root.version_is_root && root.version.is_empty() {
        println!("{}", root.name);
    } else {
        println!("{}@{}", root.name, root.version);
    }
    print_children(&root.children, "");
}

fn print_children(children: &[Node], prefix: &str) {
    let count = children.len();
    for (index, child) in children.iter().enumerate() {
        let is_last = index + 1 == count;
        let branch = if is_last { "└── " } else { "├── " };
        println!("{prefix}{branch}{}@{}", child.name, child.version);
        let continuation = if is_last { "    " } else { "│   " };
        print_children(&child.children, &format!("{prefix}{continuation}"));
    }
}

// ── JSON output (npm `ls --json` shape) ─────────────────────────────────────

#[derive(Serialize)]
struct JsonRoot {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    dependencies: BTreeMap<String, JsonDep>,
}

#[derive(Serialize)]
struct JsonDep {
    version: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    dependencies: BTreeMap<String, JsonDep>,
}

fn node_to_json(root: &Node) -> JsonRoot {
    JsonRoot {
        name: root.name.clone(),
        version: (!root.version.is_empty()).then(|| root.version.clone()),
        dependencies: deps_of(root),
    }
}

fn deps_of(node: &Node) -> BTreeMap<String, JsonDep> {
    let mut deps = BTreeMap::new();
    for child in &node.children {
        deps.insert(
            child.name.clone(),
            JsonDep {
                version: child.version.clone(),
                dependencies: deps_of(child),
            },
        );
    }
    deps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(paths: &[(&str, &str, &str)]) -> Vec<PackageEntry> {
        paths
            .iter()
            .map(|(path, name, version)| PackageEntry {
                path: path.to_string(),
                name: name.to_string(),
                version: version.to_string(),
                dependencies: BTreeMap::new(),
                ..Default::default()
            })
            .collect()
    }

    fn by_path(entries: &[PackageEntry]) -> BTreeMap<&str, &PackageEntry> {
        entries.iter().map(|e| (e.path.as_str(), e)).collect()
    }

    #[test]
    fn dep_candidates_top_level_package() {
        // A top-level package can only nest or share the top-level slot.
        let c = dep_candidates("node_modules/express", "accepts");
        assert_eq!(
            c,
            [
                "node_modules/express/node_modules/accepts",
                "node_modules/accepts",
            ]
        );
    }

    #[test]
    fn dep_candidates_nested_package_walks_up() {
        let c = dep_candidates("node_modules/a/node_modules/b", "dep");
        assert_eq!(
            c,
            [
                "node_modules/a/node_modules/b/node_modules/dep",
                "node_modules/a/node_modules/dep",
                "node_modules/dep",
            ]
        );
    }

    #[test]
    fn resolve_dep_target_picks_nearest_enclosing() {
        // `dep` exists both nested under `a` and at the top level; the nested
        // copy is closer to `a/node_modules/b` and must win.
        let pkgs = entries(&[
            ("node_modules/a/node_modules/b", "b", "1.0.0"),
            ("node_modules/a/node_modules/dep", "dep", "1.0.0"),
            ("node_modules/dep", "dep", "2.0.0"),
        ]);
        let by_path = by_path(&pkgs);
        let got = resolve_dep_target("node_modules/a/node_modules/b", "dep", &by_path);
        assert_eq!(got.as_deref(), Some("node_modules/a/node_modules/dep"));
    }

    #[test]
    fn resolve_dep_target_falls_back_to_top_level() {
        let mut pkgs = entries(&[("node_modules/express", "express", "1.0.0")]);
        pkgs.push(PackageEntry {
            path: "node_modules/accepts".into(),
            name: "accepts".into(),
            version: "1.3.8".into(),
            ..Default::default()
        });
        let by_path = by_path(&pkgs);
        let got = resolve_dep_target("node_modules/express", "accepts", &by_path);
        assert_eq!(got.as_deref(), Some("node_modules/accepts"));
    }

    #[test]
    fn prune_keeps_only_branches_reaching_the_filter() {
        // root -> [a (keep, reaches x), b (drop, leaf)]
        let mut root = Node {
            name: "root".into(),
            version: "1.0.0".into(),
            version_is_root: true,
            children: vec![
                Node {
                    name: "a".into(),
                    version: "1.0.0".into(),
                    version_is_root: false,
                    children: vec![Node {
                        name: "x".into(),
                        version: "1.0.0".into(),
                        version_is_root: false,
                        children: vec![],
                    }],
                },
                Node {
                    name: "b".into(),
                    version: "1.0.0".into(),
                    version_is_root: false,
                    children: vec![],
                },
            ],
        };
        assert!(prune_to(&mut root, "x"));
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].name, "a");
        assert_eq!(root.children[0].children.len(), 1);
    }
}
