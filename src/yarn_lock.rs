//! Yarn v1 `yarn.lock` import.
//!
//! A yarn.lock records resolutions keyed by exact dependency specs
//! (`"name@range":`), with no physical placement — the tree shape must be
//! recomputed. The importer walks the root manifest's ranges through the
//! lock's exact spec keys (every range in the graph appears as a key, so no
//! network or semver evaluation is needed), then places each resolved
//! package npm-style: hoisted to the root unless the name is already
//! occupied there by a different version, in which case nested under the
//! package that first required it.
//!
//! Integrity: yarn records `sha512-…` like npm; a `sha1`-only digest is
//! dropped rather than rejected so the import fetch pass can recompute the
//! sha512 from the tarball. `os`/`cpu` fields are carried through for
//! platform filtering. Workspace entries (`workspace:` specs) are refused
//! with a diagnostic until workspace import is supported.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use crate::diagnostic::{sort_diagnostics, Diagnostic, Severity};
use crate::lockfile::{Lockfile, PackageEntry, RootEntry};
use crate::manifest::PackageManifest;
use crate::npm_lock::ImportReport;

#[derive(Debug, thiserror::Error)]
pub enum YarnLockError {
    #[error("cannot read yarn.lock at {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("malformed yarn.lock: {0}")]
    Parse(String),
    #[error("cannot read package.json beside yarn.lock: {0}")]
    Manifest(String),
}

/// One parsed yarn.lock entry.
#[derive(Debug, Clone, Default)]
struct YarnEntry {
    /// Exact spec keys that resolve here, e.g. `lodash@^4.17.0`.
    specs: Vec<String>,
    name: String,
    version: String,
    resolved: String,
    integrity: Option<String>,
    dependencies: BTreeMap<String, String>,
    optional_dependencies: BTreeMap<String, String>,
    peer_dependencies: BTreeMap<String, String>,
    os: Vec<String>,
    cpu: Vec<String>,
}

impl YarnEntry {
    fn key(&self, name: &str, range: &str) -> bool {
        let wanted = format!("{name}@{range}");
        self.specs.iter().any(|spec| spec == &wanted)
    }
}

/// Unquote a yarn lock token: `"foo bar"` -> `foo bar`.
fn unquote(token: &str) -> &str {
    let trimmed = token.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed)
}

fn field_value(line: &str, field: &str) -> Option<String> {
    let rest = line.strip_prefix(field)?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.rfind('"')?;
    Some(rest[..end].to_string())
}

/// Parse a yarn v1 lock document into its entries.
fn parse_yarn_v1(text: &str) -> Result<Vec<YarnEntry>, YarnLockError> {
    let mut entries: Vec<YarnEntry> = Vec::new();
    let mut current: Option<YarnEntry> = None;
    // Nested block being read: ("dependencies" | "optionalDependencies" | ...)
    let mut block: Option<&'static str> = None;

    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let body = line.trim_start();
        if indent == 0 {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            let Some(header) = body.strip_suffix(':') else {
                return Err(YarnLockError::Parse(format!(
                    "expected an entry header, found {body:?}"
                )));
            };
            let mut entry = YarnEntry::default();
            for spec in header.split(", ") {
                let spec = unquote(spec);
                entry.specs.push(spec.to_string());
                // The name is everything before the final `@` of the spec
                // (scoped names include their own leading `@`).
                if let Some(name) = spec_name(spec) {
                    if entry.name.is_empty() {
                        entry.name = name.to_string();
                    }
                }
            }
            current = Some(entry);
            block = None;
            continue;
        }
        let Some(entry) = current.as_mut() else {
            return Err(YarnLockError::Parse(format!(
                "indented line outside an entry: {body:?}"
            )));
        };
        if indent == 2 {
            block = None;
            if let Some(version) = field_value(body, "version") {
                entry.version = version;
            } else if let Some(resolved) = field_value(body, "resolved") {
                entry.resolved = resolved;
            } else if let Some(integrity) = body
                .strip_prefix("integrity")
                .map(str::trim_start)
                .filter(|value| !value.is_empty())
            {
                // yarn does not quote integrity values.
                entry.integrity = Some(integrity.to_string());
            } else if matches!(body, "os" | "os:") {
                block = Some("os");
            } else if matches!(body, "cpu" | "cpu:") {
                block = Some("cpu");
            } else if matches!(body, "dependencies" | "dependencies:") {
                block = Some("dependencies");
            } else if matches!(body, "optionalDependencies" | "optionalDependencies:") {
                block = Some("optionalDependencies");
            } else if matches!(body, "peerDependencies" | "peerDependencies:") {
                block = Some("peerDependencies");
            }
            continue;
        }
        // Indent 4: a row inside the open block.
        if let Some(kind) = block {
            match kind {
                "os" => {
                    let value = unquote(body);
                    if !value.is_empty() {
                        entry.os.push(value.to_string());
                    }
                }
                "cpu" => {
                    let value = unquote(body);
                    if !value.is_empty() {
                        entry.cpu.push(value.to_string());
                    }
                }
                "dependencies" | "optionalDependencies" | "peerDependencies" => {
                    let mut parts = body.splitn(2, ' ');
                    let name = unquote(parts.next().unwrap_or_default()).to_string();
                    let range = unquote(parts.next().unwrap_or("\"\"")).to_string();
                    match kind {
                        "dependencies" => {
                            entry.dependencies.insert(name, range);
                        }
                        "optionalDependencies" => {
                            entry.optional_dependencies.insert(name, range);
                        }
                        _ => {
                            entry.peer_dependencies.insert(name, range);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(entry) = current.take() {
        entries.push(entry);
    }
    Ok(entries)
}

/// The package name portion of a yarn spec: `@scope/pkg@range` ->
/// `@scope/pkg`; `pkg@range` -> `pkg`. The name itself never contains `@`,
/// but the range can (`npm:alias@^2`), so the split is the FIRST `@` after
/// the (possibly scoped) name.
fn spec_name(spec: &str) -> Option<&str> {
    if let Some(rest) = spec.strip_prefix('@') {
        // Scoped: the name runs to the first `@` of the range part.
        let at = rest.find('@')?;
        Some(&spec[..at + 1])
    } else {
        let at = spec.find('@')?;
        Some(&spec[..at])
    }
}

/// The parent package path of a placement: `node_modules/a/node_modules/b`
/// -> `node_modules/a`; `node_modules/a` -> None (the root).
fn parent_package(path: &str) -> Option<String> {
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
struct Placer {
    /// node_modules-relative dir -> occupied (name -> version).
    occupied: BTreeMap<String, BTreeMap<String, String>>,
}

impl Placer {
    /// Place `(name, version)` required by the package at `dependent_path`
    /// ("" for the root). Returns the package's lockfile path.
    fn place(&mut self, name: &str, version: &str, dependent_path: &str) -> String {
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

/// Import a yarn v1 lock plus its sibling `package.json` into a canonical
/// lockfile.
pub fn import_yarn_v1(
    lock_path: &Path,
    manifest_path: &Path,
) -> Result<ImportReport, YarnLockError> {
    let text = std::fs::read_to_string(lock_path).map_err(|source| YarnLockError::Read {
        path: lock_path.display().to_string(),
        source,
    })?;
    let manifest = PackageManifest::from_path(manifest_path)
        .map_err(|error| YarnLockError::Manifest(error.to_string()))?;
    let entries = parse_yarn_v1(&text)?;

    let mut diagnostics = Vec::new();
    // Index: name -> entries; exact spec lookup walks these (a small list
    // per name, typically 1-3 versions).
    let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.specs.iter().any(|spec| spec.contains("workspace:")) {
            diagnostics.push(Diagnostic::new(
                Severity::Warning,
                "YARN_WORKSPACE_UNSUPPORTED",
                format!(
                    "workspace entry {:?} cannot be imported yet; convert workspaces to plain paths first",
                    entry.specs.first().map(String::as_str).unwrap_or("")
                ),
            ));
            continue;
        }
        if entry.resolved.is_empty() {
            continue;
        }
        by_name.entry(entry.name.as_str()).or_default().push(index);
    }

    let resolve = |name: &str, range: &str| -> Option<usize> {
        let candidates = by_name.get(name)?;
        candidates
            .iter()
            .copied()
            .find(|&index| entries[index].key(name, range))
            .or_else(|| {
                // The lock key may not use this exact range spelling
                // (aliases, `npm:` protocols): fall back to a semver match.
                candidates.iter().copied().find(|&index| {
                    semver::Version::parse(&entries[index].version)
                        .ok()
                        .is_some_and(|v| {
                            semver::VersionReq::parse(range)
                                .ok()
                                .is_some_and(|r| r.matches(&v))
                        })
                })
            })
    };

    // Root declarations drive reachability; dev-only reachability marks the
    // entry dev like an npm lock does.
    let mut root_dependencies = manifest.dependencies.clone();
    for (name, spec) in manifest.dev_dependencies.clone() {
        root_dependencies.entry(name).or_insert(spec);
    }
    for (name, spec) in manifest.optional_dependencies.clone() {
        root_dependencies.entry(name).or_insert(spec);
    }

    // BFS with (name, range) edges; an entry reached only through dev edges
    // is dev, and optional propagation follows optional edges.
    #[derive(Clone, Copy, PartialEq)]
    struct Reach {
        prod: bool,
        optional_only: bool,
    }
    let merged_deps = |index: usize| -> BTreeMap<String, String> {
        let entry = &entries[index];
        let mut merged = entry.dependencies.clone();
        for (name, range) in entry
            .optional_dependencies
            .iter()
            .chain(entry.peer_dependencies.iter())
        {
            merged.entry(name.clone()).or_insert_with(|| range.clone());
        }
        merged
    };

    let mut reach: BTreeMap<String, Reach> = BTreeMap::new(); // entry name@version -> kind
    let mut placer = Placer::default();
    // (entry index, dependent lockfile path, reach kind)
    // FIFO: the sorted root seeds claim the hoisted root slots first (the
    // version yarn hoists), and every level settles before its children.
    let mut queue: VecDeque<(usize, String, Reach)> = VecDeque::new();

    let root_names: BTreeSet<String> = root_dependencies.keys().cloned().collect();
    for (name, range) in &root_dependencies {
        if let Some(index) = resolve(name, range) {
            let optional = manifest.optional_dependencies.contains_key(name);
            queue.push_back((
                index,
                String::new(),
                Reach {
                    prod: !manifest.dev_dependencies.contains_key(name)
                        || manifest.dependencies.contains_key(name),
                    optional_only: optional && !manifest.dependencies.contains_key(name),
                },
            ));
        } else {
            diagnostics.push(Diagnostic::new(
                Severity::Warning,
                "YARN_MISSING_RESOLUTION",
                format!("no yarn.lock entry satisfies {name}@{range}"),
            ));
        }
    }

    let mut visited: BTreeSet<(usize, String)> = BTreeSet::new();
    // (entry index, placed path) in discovery order; kinds finalize after
    // the walk so a dev-first discovery later reached from prod is not
    // mislabeled.
    let mut placed_entries: Vec<(usize, String)> = Vec::new();
    let mut emitted_paths: BTreeSet<String> = BTreeSet::new();
    let mut lockfile = Lockfile::new("bpm");
    lockfile.root = RootEntry {
        name: Some(manifest.name.clone().unwrap_or_else(|| "root".to_string())),
        version: manifest.version.clone(),
        dependencies: root_dependencies.clone(),
    };
    // The frozen drift check reconciles the merged root dependency map
    // against the manifest's separate dev/optional groups.
    lockfile.resolution.root.dev_dependencies = manifest.dev_dependencies.clone();
    lockfile.resolution.root.optional_dependencies = manifest.optional_dependencies.clone();
    let _ = &root_names;
    while let Some((index, dependent, kind)) = queue.pop_front() {
        let entry = &entries[index];
        let identity = format!("{}@{}", entry.name, entry.version);
        if !visited.insert((index, dependent.clone())) {
            // Merge reachability kinds from additional paths.
            if let Some(existing) = reach.get_mut(&identity) {
                existing.prod |= kind.prod;
                existing.optional_only &= kind.optional_only;
            }
            continue;
        }
        let entry_kind = reach.entry(identity.clone()).or_insert(kind);
        entry_kind.prod |= kind.prod;
        entry_kind.optional_only &= kind.optional_only;

        // Place per dependent: a nested copy is reachable only from its own
        // subtree, so the same (name, version) required from a different
        // subtree must place again. The placer's occupancy map dedupes
        // within a directory.
        let path = placer.place(&entry.name, &entry.version, &dependent);
        if emitted_paths.insert(path.clone()) {
            placed_entries.push((index, path.clone()));
        }

        let dependencies = merged_deps(index);

        for (dep_name, dep_range) in &dependencies {
            if let Some(dep_index) = resolve(dep_name, dep_range) {
                let optional = entry.optional_dependencies.contains_key(dep_name);
                queue.push_back((
                    dep_index,
                    path.clone(),
                    Reach {
                        prod: kind.prod,
                        optional_only: kind.optional_only && optional,
                    },
                ));
            }
        }
    }

    for (index, path) in placed_entries {
        let entry = &entries[index];
        let identity = format!("{}@{}", entry.name, entry.version);
        let kind = reach.get(&identity).copied().unwrap_or(Reach {
            prod: true,
            optional_only: false,
        });
        let dependencies = merged_deps(index);
        let integrity = entry.integrity.as_deref().and_then(|value| {
            // sha1-only digests cannot verify a frozen install; drop them
            // and let the import fetch pass recompute sha512 from the tarball.
            if value.starts_with("sha512-") {
                Some(value.to_string())
            } else {
                None
            }
        });
        lockfile.packages.push(PackageEntry {
            path,
            name: entry.name.clone(),
            version: entry.version.clone(),
            resolved: entry.resolved.clone(),
            workspace_target: None,
            integrity,
            link: false,
            dev: !kind.prod,
            optional: kind.optional_only,
            os: entry.os.clone(),
            cpu: entry.cpu.clone(),
            bin: BTreeMap::new(),
            dependencies,
        });
    }
    sort_diagnostics(&mut diagnostics);
    lockfile.sort_packages();
    Ok(ImportReport {
        lockfile,
        diagnostics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1


"@babel/code-frame@^7.0.0":
  version "7.22.13"
  resolved "https://registry.yarnpkg.com/@babel/code-frame/-/code-frame-7.22.13.tgz#e3bc1..."
  integrity sha512-XktuhWlJ5g+3TJXc5upd9Ks1HutJrkviFSskGGdUwasBlpAE9DD3kVl5YIxnDxgjNm2LiuGjpegYTG2rh6lA==
  dependencies:
    "@babel/highlight" "^7.22.13"

lodash@^4.17.0, lodash@^4.0.0:
  version "4.17.21"
  resolved "https://registry.yarnpkg.com/lodash/-/lodash-4.17.21.tgz#679591c564c3bffaae8454cf0b3df370c3d6911c"
  integrity sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg==

fsevents@^1.2.7:
  version "1.2.13"
  resolved "https://registry.yarnpkg.com/fsevents/-/fsevents-1.2.13.tgz#f323cb0f43c35f6d3e6c0efd5b2b9d0b9c8c9d1a"
  integrity sha1-aXSjPkdXw3GVn2tS5k1AXkvJvA=
  os:
    darwin
"#;

    #[test]
    fn parses_entries_fields_and_blocks() {
        let entries = parse_yarn_v1(SAMPLE).unwrap();
        assert_eq!(entries.len(), 3);
        let code_frame = entries
            .iter()
            .find(|e| e.name == "@babel/code-frame")
            .unwrap();
        assert_eq!(code_frame.version, "7.22.13");
        assert_eq!(
            code_frame
                .dependencies
                .get("@babel/highlight")
                .map(String::as_str),
            Some("^7.22.13")
        );
        let lodash = entries.iter().find(|e| e.name == "lodash").unwrap();
        assert_eq!(lodash.specs.len(), 2);
        assert!(lodash.key("lodash", "^4.0.0"));
        let fsevents = entries.iter().find(|e| e.name == "fsevents").unwrap();
        assert_eq!(fsevents.os, vec!["darwin".to_string()]);
        // sha1-only integrity is preserved as parsed; the importer drops it.
        assert!(fsevents.integrity.as_deref().unwrap().starts_with("sha1-"));
    }

    #[test]
    fn spec_name_handles_scoped_and_plain() {
        assert_eq!(spec_name("@scope/pkg@^1.0.0"), Some("@scope/pkg"));
        assert_eq!(spec_name("lodash@^4.0.0"), Some("lodash"));
        // npm: alias ranges contain `@`; the name ends at the first one.
        assert_eq!(spec_name("prettier-2@npm:prettier@^2"), Some("prettier-2"));
        assert_eq!(spec_name("lodash"), None);
    }

    #[test]
    fn placer_hoists_root_owner_and_nests_conflicts() {
        // Root requirements process first and claim the hoisted root slots;
        // conflicting versions claim the highest free slot on their
        // dependent's ancestor chain.
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
        // A deep dependent whose own dir is free claims it directly, even
        // though the root holds another version (no shadowing is possible).
        assert_eq!(
            placer.place("a", "3.0.0", "node_modules/b/node_modules/c"),
            "node_modules/b/node_modules/c/node_modules/a"
        );
        // parent_package walks the placement chain upward.
        assert_eq!(
            parent_package("node_modules/b/node_modules/c"),
            Some("node_modules/b".to_string())
        );
        assert_eq!(parent_package("node_modules/b"), None);
    }

    #[test]
    fn imports_a_small_graph_with_placements_and_kinds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"app","version":"1.0.0","dependencies":{"a":"^1.0.0"},
                "devDependencies":{"devtool":"^2.0.0"},"optionalDependencies":{"fsevents":"^1.2.7"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("yarn.lock"),
            r#"
a@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/a/-/a-1.0.0.tgz#aaa"
  integrity sha512-AAAA
  dependencies:
    b "^2.0.0"

b@^2.0.0, b@^2.1.0:
  version "2.1.0"
  resolved "https://registry.yarnpkg.com/b/-/b-2.1.0.tgz#bbb"
  integrity sha512-BBBB

devtool@^2.0.0:
  version "2.5.0"
  resolved "https://registry.yarnpkg.com/devtool/-/devtool-2.5.0.tgz#ddd"
  integrity sha512-DDDD

unreachable@^9.0.0:
  version "9.0.0"
  resolved "https://registry.yarnpkg.com/unreachable/-/unreachable-9.0.0.tgz#uuu"
  integrity sha512-UUUU

fsevents@^1.2.7:
  version "1.2.13"
  resolved "https://registry.yarnpkg.com/fsevents/-/fsevents-1.2.13.tgz#fff"
  integrity sha1-only
  os:
    darwin
"#,
        )
        .unwrap();
        let report = import_yarn_v1(
            &dir.path().join("yarn.lock"),
            &dir.path().join("package.json"),
        )
        .unwrap();
        let lock = &report.lockfile;
        let find = |name: &str| {
            lock.packages
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("missing {name}"))
        };
        assert_eq!(find("a").path, "node_modules/a");
        assert!(!find("a").dev);
        // Reached only from a devDependency.
        assert!(find("devtool").dev);
        // Optional root declaration propagates.
        assert!(find("fsevents").optional);
        assert_eq!(find("fsevents").os, vec!["darwin".to_string()]);
        // sha1-only integrity dropped for the fetch pass to recompute.
        assert_eq!(find("fsevents").integrity, None);
        assert!(find("a")
            .integrity
            .as_deref()
            .unwrap()
            .starts_with("sha512-"));
        // Multi-spec keys resolve both spellings to one entry.
        assert_eq!(lock.packages.iter().filter(|p| p.name == "b").count(), 1);
        // Unreached entries are not placed.
        assert!(lock.packages.iter().all(|p| p.name != "unreachable"));
    }
}
