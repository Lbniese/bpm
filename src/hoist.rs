//! Shared hoisting placement for lockfile imports that must recompute a
//! physical `node_modules` tree (yarn v1, pnpm) from resolution data.

use std::collections::BTreeMap;

/// The parent package path of a placement: `node_modules/a/node_modules/b`
/// -> `node_modules/a`; `node_modules/a` -> None (the root).
pub(crate) fn parent_package(path: &str) -> Option<String> {
    if !path.starts_with("node_modules/") {
        return None;
    }
    path.rfind("/node_modules/")
        .map(|at| path[..at].to_string())
}

/// Where resolved packages are physically placed, yarn-v1-style.
///
/// A requirement scans its dependent's ancestor chain from the dependent's
/// own `node_modules` upward: a slot holding the SAME version is shared; a
/// slot holding a different version blocks everything above it. The claim
/// target is the ROOT when the whole chain is free (the hoisted common
/// case), otherwise the dependent's OWN directory. Intermediate directories
/// are never claimed directly by deep dependents: a free-looking
/// intermediate can be the resolution path of its owner (placing there
/// would retroactively shadow it), which is the same hazard that keeps
/// yarn's linker to root-and-nest-under-dependent placement. Root claims
/// are always safe (nothing resolves above the root), and own-dir claims
/// only affect the dependent's subtree. Every edge therefore resolves to
/// its exact version by construction, and the sorted FIFO seed order gives
/// root-manifest versions the root slots.
#[derive(Default)]
pub(crate) struct Placer {
    /// node_modules-relative dir -> occupied (name -> version).
    occupied: BTreeMap<String, BTreeMap<String, String>>,
}

impl Placer {
    /// Place `(name, version)` required by the package at `dependent_path`
    /// ("" for the root). Returns the package's lockfile path.
    pub(crate) fn place(&mut self, name: &str, version: &str, dependent_path: &str) -> String {
        let mut chain: Vec<String> = Vec::new();
        if dependent_path.is_empty() {
            chain.push("node_modules".to_string());
        } else {
            let mut current = Some(dependent_path.to_string());
            while let Some(package) = current {
                chain.push(format!("{package}/node_modules"));
                current = parent_package(&package);
            }
            chain.push("node_modules".to_string());
        }
        let root = chain.last().expect("chain always ends at the root");
        let own = &chain[0];
        let mut root_free = true;
        for dir in &chain {
            match self
                .occupied
                .get(dir)
                .and_then(|slot| slot.get(name))
                .map(String::as_str)
            {
                Some(occupied) if occupied == version => {
                    return format!("{dir}/{name}");
                }
                Some(_) => {
                    root_free = false;
                    break;
                }
                None => continue,
            }
        }
        let target = if root_free { root } else { own };
        self.occupied
            .entry(target.clone())
            .or_default()
            .insert(name.to_string(), version.to_string());
        format!("{target}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_root_nests_conflicts_and_shares() {
        let mut placer = Placer::default();
        assert_eq!(placer.place("a", "1.0.0", ""), "node_modules/a");
        assert_eq!(
            placer.place("a", "1.0.0", "node_modules/b"),
            "node_modules/a"
        );
        assert_eq!(
            placer.place("a", "2.0.0", "node_modules/b"),
            "node_modules/b/node_modules/a"
        );
        assert_eq!(
            placer.place("a", "3.0.0", "node_modules/b/node_modules/c"),
            "node_modules/b/node_modules/c/node_modules/a"
        );
    }

    #[test]
    fn parent_package_walks_upward() {
        assert_eq!(
            parent_package("node_modules/b/node_modules/c"),
            Some("node_modules/b".to_string())
        );
        assert_eq!(parent_package("node_modules/b"), None);
    }
}
