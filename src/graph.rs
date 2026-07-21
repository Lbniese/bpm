//! Graph-plan cache (IMPLEMENTATION §6, §9, §11 — Milestone 3).
//!
//! A dependency graph is identified by a canonical hash of its lockfile graph
//! and target platform; an install plan is the deterministic record of the
//! materialization operations performed for that graph. Both are cached so an
//! unchanged repeated install skips resolution and plan construction.
//!
//! The plan file (`.bpm-state`) is disposable: if it is missing, stale, or fails
//! validation against the live project state, the installer regenerates it from
//! the authoritative `bpm.lock`. The text lockfile remains the source of truth.
//!
//! Determinism (IMPLEMENTATION §6): the hash input has a canonical
//! serialization independent of hash-map iteration or insertion order. Package
//! entries are already sorted by path in `bpm.lock`, and dependency/bin maps
//! are `BTreeMap`s, so encoding is stable across machines and runs.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::integrity::ArtifactId;
use crate::lockfile::{Lockfile, PackageEntry, PackageResolution};
use crate::store::ArtifactStore;

///plan file name written next to `bpm.lock` (IMPLEMENTATION §9: `.bpm-state`).
pub const PLAN_FILE: &str = ".bpm-state";

/// Bumped when the plan encoding or materialization semantics change. A plan
/// with a different version is treated as invalid and regenerated.
pub const PLAN_VERSION: u32 = 2;

/// Bumped when the materializer's output semantics change (e.g. bin linking
/// strategy, symlink vs hardlink volume layout). Incompatible materializer
/// versions invalidate a cached plan even if the graph is identical.
pub const MATERIALIZER_VERSION: u32 = 5;

/// A 256-bit blake3 digest identifying a canonical dependency graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GraphId(pub blake3::Hash);

impl GraphId {
    /// Lowercase hex (64 chars), filesystem- and diff-safe.
    pub fn to_hex(&self) -> String {
        self.0.to_hex().to_string()
    }

    /// First 12 hex chars for compact human-facing display.
    pub fn to_hex_short(&self) -> String {
        self.to_hex()[..12].to_string()
    }
}

/// A single materialization operation recorded in a plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanEntry {
    /// `node_modules/...` path (the `bpm.lock` package key).
    pub path: String,
    pub name: String,
    pub version: String,
    /// Registry tarball URL (empty for link/file entries).
    pub resolved: String,
    /// npm integrity string (`sha512-...`), when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
    /// `true` for symlink/workspace/file entries (not materialized).
    #[serde(default)]
    pub link: bool,
    /// Lowercase hex of the verified artifact digest (the store image key).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artifact_hex: String,
    /// Declared executables (`bin name -> relative path within package`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bin: BTreeMap<String, String>,
}

/// A project-view entry that BPM created and therefore may safely remove.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ManagedEntry {
    /// Exact project-relative path (e.g. `node_modules/foo`,
    /// `node_modules/@scope/bar`).
    pub path: String,
    /// How this entry was attached: "relay" (symlink to volume),
    /// "local" (hardlink/copy), "reflink" (copy-on-write clone), or
    /// "direct" (workspace symlink).
    pub mode: String,
    /// Old volume or symlink target at the time of recording, for identity
    /// preflight before removal.
    pub identity: String,
}

/// A compiled install plan for one graph (IMPLEMENTATION §9).
///
/// Authoritative only as a cache: `bpm.lock` drives regeneration on mismatch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallPlan {
    pub plan_version: u32,
    pub materializer_version: u32,
    pub graph_id_hex: String,
    /// Package paths whose lifecycle scripts ran against the graph volume,
    /// producing derived (isolated, non-hardlink) entries there. Their volume
    /// entries are validated by existence rather than store-image identity.
    #[serde(default)]
    pub lifecycle_paths: Vec<String>,
    /// Exact project-relative paths BPM created in the previous install,
    /// with their attachment mode and identity, used for safe stale-entry
    /// reconciliation on the next graph change.
    #[serde(default)]
    pub owned_entries: Vec<ManagedEntry>,
    pub entries: Vec<PlanEntry>,
}

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("failed to read plan {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse plan {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write plan {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Best-effort target-platform descriptor (`<arch>/<os>`), lowercased. Included
/// in the graph id so a plan for one platform is never reused on another.
pub fn platform_descriptor() -> String {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    format!("{arch}/{os}")
}

/// Canonical, byte-stable encoding of a lockfile's resolved graph.
///
/// Encodes only graph-relevant fields (never cosmetic ones like `generator`),
/// in declaration-order-independent order: root dependency names+specs sorted,
/// then package entries in their (path-sorted) list order with each field
/// length-prefixed. Identical logical graphs hash identically regardless of how
/// the lockfile was constructed.
pub fn canonical_graph_bytes(lockfile: &Lockfile) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1024);
    // Header bounds the encoding so unrelated trailing bytes can't collide.
    buf.extend_from_slice(b"bpm-graph-v2\n");
    write_field(&mut buf, &platform_descriptor());

    // Root declared dependencies: name -> spec, sorted (BTreeMap iteration order).
    buf.extend_from_slice(b"root\n");
    write_u64(&mut buf, lockfile.root.dependencies.len() as u64);
    for (name, spec) in &lockfile.root.dependencies {
        write_field(&mut buf, name);
        write_field(&mut buf, spec);
    }
    // Resolver inputs that may alter the effective graph without changing
    // the compatibility root dependency map.
    write_u64(&mut buf, lockfile.resolution.root.overrides.len() as u64);
    for (name, spec) in &lockfile.resolution.root.overrides {
        write_field(&mut buf, name);
        write_field(&mut buf, spec);
    }
    if let Some(target) = &lockfile.resolution.root.target {
        write_bool(&mut buf, true);
        write_field(&mut buf, &target.os);
        write_field(&mut buf, &target.cpu);
        write_field(&mut buf, target.libc.as_deref().unwrap_or(""));
    } else {
        write_bool(&mut buf, false);
    }

    // Package entries in list order (bpm.lock keeps them path-sorted).
    buf.extend_from_slice(b"packages\n");
    write_u64(&mut buf, lockfile.packages.len() as u64);
    for p in &lockfile.packages {
        write_field(&mut buf, &p.path);
        write_field(&mut buf, &p.name);
        write_field(&mut buf, &p.version);
        write_field(&mut buf, &p.resolved);
        write_field(&mut buf, p.integrity.as_deref().unwrap_or(""));
        write_bool(&mut buf, p.link);
        write_bool(&mut buf, p.dev);
        write_bool(&mut buf, p.optional);
        write_u64(&mut buf, p.os.len() as u64);
        for value in &p.os {
            write_field(&mut buf, value);
        }
        write_u64(&mut buf, p.cpu.len() as u64);
        for value in &p.cpu {
            write_field(&mut buf, value);
        }
        if let Some(resolution) = lockfile.resolution.packages.get(&p.path) {
            write_u64(&mut buf, resolution.libc.len() as u64);
            for value in &resolution.libc {
                write_field(&mut buf, value);
            }
        } else {
            write_u64(&mut buf, 0);
        }
        // bin map sorted (BTreeMap).
        write_u64(&mut buf, p.bin.len() as u64);
        for (bname, bpath) in &p.bin {
            write_field(&mut buf, bname);
            write_field(&mut buf, bpath);
        }
        // dependency specs sorted (BTreeMap).
        write_u64(&mut buf, p.dependencies.len() as u64);
        for (dname, dspec) in &p.dependencies {
            write_field(&mut buf, dname);
            write_field(&mut buf, dspec);
        }
    }
    buf
}

/// Compute the graph id for a lockfile (+ the running platform).
pub fn graph_id(lockfile: &Lockfile) -> GraphId {
    let bytes = canonical_graph_bytes(lockfile);
    GraphId(blake3::hash(&bytes))
}

/// Compute a graph id with prepared-image identities but without workspace
/// layout, matching the historical graph-volume identity.
pub fn graph_id_with_prepared(
    lockfile: &Lockfile,
    prepared: &BTreeMap<String, [u8; 32]>,
) -> GraphId {
    if prepared.is_empty() {
        return graph_id(lockfile);
    }
    let mut bytes = canonical_graph_bytes(lockfile);
    append_prepared_images(&mut bytes, prepared);
    GraphId(blake3::hash(&bytes))
}

/// Graph id including the workspace layout (IMPLEMENTATION §15: "include
/// workspace layout in the graph ID"). Falls back to the plain graph id when
/// the project has no workspaces.
pub fn graph_id_with_workspace(
    lockfile: &Lockfile,
    workspace: &crate::workspace::WorkspaceLayout,
) -> GraphId {
    if workspace.packages.is_empty() && workspace.patterns.is_empty() {
        return graph_id(lockfile);
    }
    let mut bytes = canonical_graph_bytes(lockfile);
    bytes.extend(crate::workspace::canonical_workspace_bytes(workspace));
    GraphId(blake3::hash(&bytes))
}

/// Graph id for a project: discovers the workspace layout from `project_root`
/// and folds it into the id, so a change to the workspace tree invalidates the
/// cached plan/volume.
pub fn graph_id_for_project(lockfile: &Lockfile, project_root: &Path) -> GraphId {
    graph_id_for_project_with_prepared(lockfile, project_root, &BTreeMap::new())
}

/// Compute the project graph id while including immutable prepared-image keys.
///
/// Prepared Git images are not lockfile packages, so their identity must be
/// folded into the volume/plan key separately. Keeping this input outside the
/// lockfile preserves the consumer graph while preventing a raw-source volume
/// from being reused for a different prepared image.
pub fn graph_id_for_project_with_prepared(
    lockfile: &Lockfile,
    project_root: &Path,
    prepared: &BTreeMap<String, [u8; 32]>,
) -> GraphId {
    let ws = crate::workspace::discover(project_root);
    if prepared.is_empty() {
        return graph_id_with_workspace(lockfile, &ws);
    }
    let mut bytes = canonical_graph_bytes(lockfile);
    if !ws.packages.is_empty() || !ws.patterns.is_empty() {
        bytes.extend(crate::workspace::canonical_workspace_bytes(&ws));
    }
    append_prepared_images(&mut bytes, prepared);
    GraphId(blake3::hash(&bytes))
}

fn append_prepared_images(bytes: &mut Vec<u8>, prepared: &BTreeMap<String, [u8; 32]>) {
    bytes.extend_from_slice(b"prepared-images\n");
    write_u64(bytes, prepared.len() as u64);
    for (path, key) in prepared {
        write_field(bytes, path);
        bytes.extend_from_slice(key);
    }
}

/// Compute a deterministic BLAKE3 digest of one package's resolved dependency
/// closure, for use as the `dependency_graph` input of a derived-artifact key.
///
/// The walk follows the resolved `target` edges the resolver recorded in
/// `bpm.lock` across `dependencies`, `optional_dependencies`, and
/// `peer_dependencies`, so it mirrors exactly what a lifecycle script's
/// `require()` resolves to at runtime, including npm's hoisting. An unrelated
/// dependency elsewhere in the graph does not flip a package's digest; a
/// reachable dependency's version, placement, or own closure does. Dependency
/// cycles (which npm permits) terminate via a deterministic back-edge marker.
///
/// The digest is a pure function of `(lockfile, package_path)`: each call uses
/// a fresh memoization table so a package's key never depends on the order in
/// which packages are processed during an install.
pub fn package_closure_digest(lockfile: &Lockfile, package_path: &str) -> [u8; 32] {
    let package_index: HashMap<&str, &PackageEntry> = lockfile
        .packages
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut memo: HashMap<String, [u8; 32]> = HashMap::new();
    let mut stack: HashSet<String> = HashSet::new();
    closure_digest(
        package_path,
        &package_index,
        &lockfile.resolution.packages,
        &mut memo,
        &mut stack,
    )
}

fn closure_digest(
    path: &str,
    packages: &HashMap<&str, &PackageEntry>,
    resolution: &BTreeMap<String, PackageResolution>,
    memo: &mut HashMap<String, [u8; 32]>,
    stack: &mut HashSet<String>,
) -> [u8; 32] {
    if let Some(digest) = memo.get(path) {
        return *digest;
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bpm-package-closure-v1\0");
    // The package's own identity. Redundant with `source_artifact` for the
    // root of the walk, but harmless and required for each transitive member
    // of the closure to contribute its version/resolved/integrity.
    hasher.update(b"path\0");
    hasher.update(path.as_bytes());
    if let Some(entry) = packages.get(path) {
        hasher.update(b"version\0");
        hasher.update(entry.version.as_bytes());
        hasher.update(b"resolved\0");
        hasher.update(entry.resolved.as_bytes());
        hasher.update(b"integrity\0");
        hasher.update(entry.integrity.as_deref().unwrap_or("").as_bytes());
    }

    // Collect the resolved target paths across every dependency kind npm lets a
    // lifecycle script reach via require(). Dedup by placement so a target
    // shared by several dep names is folded once.
    stack.insert(path.to_string());
    let mut children: BTreeSet<&str> = BTreeSet::new();
    if let Some(resolved) = resolution.get(path) {
        for dependency in resolved
            .dependencies
            .values()
            .chain(resolved.optional_dependencies.values())
            .chain(resolved.peer_dependencies.values())
        {
            children.insert(dependency.target.as_str());
        }
    }
    for target in children {
        hasher.update(b"child\0");
        let child_digest = if stack.contains(target) {
            // Back-edge (a dependency cycle). Fold a deterministic marker
            // instead of recursing so the walk terminates without counting the
            // cycle more than once.
            let mut marker = blake3::Hasher::new();
            marker.update(b"bpm-package-closure-cycle\0");
            marker.update(target.as_bytes());
            *marker.finalize().as_bytes()
        } else {
            closure_digest(target, packages, resolution, memo, stack)
        };
        hasher.update(&child_digest);
    }
    stack.remove(path);

    let digest = *hasher.finalize().as_bytes();
    memo.insert(path.to_string(), digest);
    digest
}

/// Build a compiled plan from a lockfile and the resolved artifact id for each
/// fetchable package. `artifact_ids` is indexed by package position in
/// `lockfile.packages` (the installer sorts outcomes back into this order).
pub fn build_plan(
    lockfile: &Lockfile,
    artifact_ids: &[Option<ArtifactId>],
    lifecycle_paths: &[String],
) -> InstallPlan {
    let entries = lockfile
        .packages
        .iter()
        .enumerate()
        .map(|(i, p)| PlanEntry {
            path: p.path.clone(),
            name: p.name.clone(),
            version: p.version.clone(),
            resolved: p.resolved.clone(),
            integrity: p.integrity.clone(),
            link: p.link,
            artifact_hex: artifact_ids
                .get(i)
                .copied()
                .flatten()
                .map(|id| id.to_hex())
                .unwrap_or_default(),
            bin: p.bin.clone(),
        })
        .collect();
    InstallPlan {
        plan_version: PLAN_VERSION,
        materializer_version: MATERIALIZER_VERSION,
        graph_id_hex: graph_id(lockfile).to_hex(),
        lifecycle_paths: lifecycle_paths.to_vec(),
        owned_entries: Vec::new(),
        entries,
    }
}

/// The plan file path beside a `bpm.lock` at `lockfile_path`.
pub fn plan_path_for(lockfile_path: &Path) -> PathBuf {
    lockfile_path
        .parent()
        .map(|p| p.join(PLAN_FILE))
        .unwrap_or_else(|| PathBuf::from(PLAN_FILE))
}

/// Write a plan atomically (temp file + rename) next to the lockfile.
pub fn write_plan(plan: &InstallPlan, path: &Path) -> Result<(), PlanError> {
    let mut json = serde_json::to_vec_pretty(plan).expect("plan serializes");
    json.push(b'\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| PlanError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, &json).map_err(|source| PlanError::Write {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, path).map_err(|source| PlanError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Read a plan from disk. Returns `Ok(None)` if the file does not exist
/// (callers treat a missing plan as a cache miss, not an error).
pub fn read_plan(path: &Path) -> Result<Option<InstallPlan>, PlanError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path).map_err(|source| PlanError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let plan: InstallPlan = serde_json::from_slice(&bytes).map_err(|source| PlanError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(plan))
}

/// Reasons a cached plan may be unusable (drives a cache miss + rebuild).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanInvalid {
    /// File is absent.
    Absent,
    /// `plan_version` or `materializer_version` differ.
    VersionMismatch,
    /// Graph id differs from the current lockfile.
    GraphChanged,
    /// A materialized symlink is missing or points at the wrong store image.
    StateDrift,
}

/// Validate a cached plan against the current lockfile and live state.
///
/// With graph volumes (Milestone 4), the durable object is the **graph volume**
/// keyed by the plan's graph id, not the project's `node_modules`. Validation
/// checks:
///   1. plan/materializer versions match;
///   2. the plan's graph id equals the current lockfile's graph id;
///   3. the graph volume exists and every recorded package symlink in the
///      volume still points at its store image (volume integrity); and
///   4. the project's `node_modules` still relays into that volume
///      (project attachment).
///
/// Returns `Ok(())` if the plan is fully valid, or an `Err(PlanInvalid)`
/// describing why it must be discarded and rebuilt.
pub fn validate_plan(
    plan: &InstallPlan,
    lockfile: &Lockfile,
    project_root: &Path,
    store: &ArtifactStore,
) -> Result<(), PlanInvalid> {
    if plan.plan_version != PLAN_VERSION || plan.materializer_version != MATERIALIZER_VERSION {
        return Err(PlanInvalid::VersionMismatch);
    }
    let current = graph_id_for_project(lockfile, project_root).to_hex();
    if plan.graph_id_hex != current {
        return Err(PlanInvalid::GraphChanged);
    }

    // Graph volume integrity: the durable, graph-keyed node_modules projection.
    let volume_dir = store.graph_volume_path(&plan.graph_id_hex);
    if !volume_dir.join("node_modules").exists() {
        return Err(PlanInvalid::StateDrift);
    }
    // Packages whose scripts ran in the volume hold derived (isolated) copies,
    // not hardlinks of the store image; for them only require the entry exists.
    let derived: std::collections::BTreeSet<&str> =
        plan.lifecycle_paths.iter().map(String::as_str).collect();
    for e in &plan.entries {
        if e.link || e.resolved.is_empty() || e.artifact_hex.is_empty() {
            continue;
        }
        let Ok(digest) = ArtifactId::from_hex(&e.artifact_hex) else {
            return Err(PlanInvalid::StateDrift);
        };
        let image = store.image_path(&digest);
        let entry = volume_dir.join(&e.path);
        if derived.contains(e.path.as_str()) {
            if !entry.join("package.json").exists() {
                return Err(PlanInvalid::StateDrift);
            }
        } else if !volume_entry_intact(&entry, &image) {
            return Err(PlanInvalid::StateDrift);
        }
    }

    // Project attachment: the project must still relay into this volume.
    if !crate::volume::project_attached(project_root, &volume_dir) {
        return Err(PlanInvalid::StateDrift);
    }
    Ok(())
}

/// Whether a graph-volume entry still reflects its pristine store image.
///
/// Accepts both the legacy symlink layout (the entry is a symlink to the store
/// image) and the current hardlink layout (the entry is a real directory whose
/// `package.json` shares an inode with the store image's `package.json`).
fn volume_entry_intact(entry: &Path, image: &Path) -> bool {
    if let Ok(target) = fs::read_link(entry) {
        return target == image;
    }
    same_file(&entry.join("package.json"), &image.join("package.json"))
}

/// `true` when `a` and `b` are the same on-disk file (same device + inode on
/// Unix). Used to confirm a hardlinked volume entry matches its store image.
fn same_file(a: &Path, b: &Path) -> bool {
    let (Ok(a), Ok(b)) = (fs::metadata(a), fs::metadata(b)) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        a.len() == b.len()
    }
}

// --- length-prefixed encoding helpers (deterministic, no map iteration) ---

fn write_field(buf: &mut Vec<u8>, s: &str) {
    write_u64(buf, s.len() as u64);
    buf.extend_from_slice(s.as_bytes());
}

fn write_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn write_bool(buf: &mut Vec<u8>, b: bool) {
    buf.push(if b { 1 } else { 0 });
}

/// Silence the unused `Write` import warning while keeping the capability
/// available for future streaming encoders without touching imports.
#[allow(dead_code)]
fn _write_marker<W: Write>(_w: W, _b: &[u8]) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::{
        LockDependency, LockSource, Lockfile, PackageEntry, PackageResolution, RootEntry,
    };

    fn lf() -> Lockfile {
        let mut l = Lockfile::new("bpm");
        l.root = RootEntry {
            name: Some("app".into()),
            version: Some("1.0.0".into()),
            dependencies: BTreeMap::from([("left-pad".into(), "^1.3.0".into())]),
        };
        l.packages.push(PackageEntry {
            path: "node_modules/left-pad".into(),
            name: "left-pad".into(),
            version: "1.3.0".into(),
            resolved: "https://reg/left-pad-1.3.0.tgz".into(),
            integrity: Some("sha512-AA".into()),
            bin: BTreeMap::from([("lpad".into(), "./cli.js".into())]),
            ..Default::default()
        });
        l.sort_packages();
        l
    }

    #[test]
    fn graph_id_is_stable_across_construction_order() {
        let a = lf();
        // Rebuild with packages pushed in reverse, then re-sort: same graph.
        let mut b = Lockfile::new("different-generator-string");
        b.root = a.root.clone();
        b.packages.push(a.packages[0].clone());
        b.sort_packages();
        assert_eq!(graph_id(&a).to_hex(), graph_id(&b).to_hex());
    }

    #[test]
    fn graph_id_changes_when_a_dependency_changes() {
        let mut a = lf();
        let id0 = graph_id(&a).to_hex();
        a.packages[0].version = "1.3.1".into();
        let id1 = graph_id(&a).to_hex();
        assert_ne!(id0, id1, "version change must alter the graph id");
    }

    #[test]
    fn plan_roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PLAN_FILE);
        let l = lf();
        let id = ArtifactId::from_bytes([0x9; 64]);
        let plan = build_plan(&l, &[Some(id)], &[]);
        write_plan(&plan, &path).unwrap();
        let back = read_plan(&path).unwrap().unwrap();
        assert_eq!(plan, back);
    }

    #[test]
    fn read_plan_returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_plan(&dir.path().join(PLAN_FILE)).unwrap().is_none());
    }

    #[test]
    fn validate_rejects_version_mismatch() {
        let l = lf();
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(dir.path()).unwrap();
        let mut plan = build_plan(&l, &[Some(ArtifactId::from_bytes([0x1; 64]))], &[]);
        plan.plan_version = 999;
        assert_eq!(
            validate_plan(&plan, &l, dir.path(), &store).unwrap_err(),
            PlanInvalid::VersionMismatch
        );
    }

    #[test]
    fn validate_rejects_graph_change() {
        let l = lf();
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(dir.path()).unwrap();
        let mut plan = build_plan(&l, &[Some(ArtifactId::from_bytes([0x1; 64]))], &[]);
        plan.graph_id_hex = "deadbeef".into();
        assert_eq!(
            validate_plan(&plan, &l, dir.path(), &store).unwrap_err(),
            PlanInvalid::GraphChanged
        );
    }

    // --- per-package closure digest (derived-artifact `dependency_graph`) ---

    fn pkg(path: &str, version: &str, integrity: &str) -> PackageEntry {
        PackageEntry {
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or(path).into(),
            version: version.into(),
            resolved: format!("https://reg/{path}-{version}.tgz"),
            integrity: Some(integrity.into()),
            ..Default::default()
        }
    }

    fn res(deps: &[(&str, &str)]) -> PackageResolution {
        PackageResolution {
            source: LockSource::Registry {
                registry: "https://registry.npmjs.org".into(),
            },
            dev_optional: false,
            peer: false,
            dependencies: deps
                .iter()
                .map(|(name, target)| {
                    (
                        (*name).into(),
                        LockDependency {
                            spec: "*".into(),
                            target: (*target).into(),
                        },
                    )
                })
                .collect(),
            optional_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            optional_peers: BTreeSet::new(),
            peer_context: BTreeMap::new(),
            libc: Vec::new(),
            workspace_target: None,
            has_install_script: false,
        }
    }

    fn closure_lockfile(
        packages: &[PackageEntry],
        resolution: &[(&str, PackageResolution)],
    ) -> Lockfile {
        let mut lockfile = Lockfile::new("bpm");
        for entry in packages {
            lockfile.packages.push(entry.clone());
        }
        for (path, resolved) in resolution {
            lockfile
                .resolution
                .packages
                .insert((*path).into(), resolved.clone());
        }
        lockfile.sort_packages();
        lockfile
    }

    #[test]
    fn closure_digest_is_stable_for_the_same_lockfile() {
        let packages = [
            pkg("node_modules/native", "1.0.0", "sha512-native"),
            pkg("node_modules/headers", "2.0.0", "sha512-headers"),
        ];
        let resolution = [
            (
                "node_modules/native",
                res(&[("headers", "node_modules/headers")]),
            ),
            ("node_modules/headers", res(&[])),
        ];
        let lockfile = closure_lockfile(&packages, &resolution);
        let a = package_closure_digest(&lockfile, "node_modules/native");
        let b = package_closure_digest(&lockfile, "node_modules/native");
        assert_eq!(a, b);
        assert_ne!(a, [0u8; 32]);
    }

    #[test]
    fn unrelated_dependency_does_not_flip_a_package_closure() {
        // native -> headers (reachable). unrelated -> other (NOT reachable from
        // native). Bumping `other` must not invalidate native's closure digest.
        let packages_a = [
            pkg("node_modules/native", "1.0.0", "sha512-native"),
            pkg("node_modules/headers", "2.0.0", "sha512-headers"),
            pkg("node_modules/unrelated", "1.0.0", "sha512-unrelated"),
            pkg("node_modules/other", "1.0.0", "sha512-other"),
        ];
        let resolution = [
            (
                "node_modules/native",
                res(&[("headers", "node_modules/headers")]),
            ),
            ("node_modules/headers", res(&[])),
            (
                "node_modules/unrelated",
                res(&[("other", "node_modules/other")]),
            ),
            ("node_modules/other", res(&[])),
        ];
        let lockfile_a = closure_lockfile(&packages_a, &resolution);

        let mut packages_b = packages_a.clone();
        packages_b[3] = pkg("node_modules/other", "2.0.0", "sha512-other-v2");
        let lockfile_b = closure_lockfile(&packages_b, &resolution);

        assert_eq!(
            package_closure_digest(&lockfile_a, "node_modules/native"),
            package_closure_digest(&lockfile_b, "node_modules/native"),
            "an unreachable dependency must not invalidate the closure"
        );
        // Sanity: the unrelated package's own closure DID change.
        assert_ne!(
            package_closure_digest(&lockfile_a, "node_modules/unrelated"),
            package_closure_digest(&lockfile_b, "node_modules/unrelated")
        );
    }

    #[test]
    fn reachable_dependency_version_flips_a_package_closure() {
        let resolution = [
            (
                "node_modules/native",
                res(&[("headers", "node_modules/headers")]),
            ),
            ("node_modules/headers", res(&[])),
        ];
        let lockfile_a = closure_lockfile(
            &[
                pkg("node_modules/native", "1.0.0", "sha512-native"),
                pkg("node_modules/headers", "2.0.0", "sha512-headers"),
            ],
            &resolution,
        );
        let lockfile_b = closure_lockfile(
            &[
                pkg("node_modules/native", "1.0.0", "sha512-native"),
                pkg("node_modules/headers", "2.1.0", "sha512-headers-v2"),
            ],
            &resolution,
        );

        assert_ne!(
            package_closure_digest(&lockfile_a, "node_modules/native"),
            package_closure_digest(&lockfile_b, "node_modules/native"),
            "a reachable dependency version change must invalidate the closure"
        );
    }

    #[test]
    fn transitive_dependency_flips_a_package_closure() {
        // native -> mid -> leaf; bumping leaf must flip native's closure.
        let resolution = [
            ("node_modules/native", res(&[("mid", "node_modules/mid")])),
            ("node_modules/mid", res(&[("leaf", "node_modules/leaf")])),
            ("node_modules/leaf", res(&[])),
        ];
        let lockfile_a = closure_lockfile(
            &[
                pkg("node_modules/native", "1.0.0", "sha512-native"),
                pkg("node_modules/mid", "1.0.0", "sha512-mid"),
                pkg("node_modules/leaf", "1.0.0", "sha512-leaf"),
            ],
            &resolution,
        );
        let lockfile_b = closure_lockfile(
            &[
                pkg("node_modules/native", "1.0.0", "sha512-native"),
                pkg("node_modules/mid", "1.0.0", "sha512-mid"),
                pkg("node_modules/leaf", "2.0.0", "sha512-leaf-v2"),
            ],
            &resolution,
        );

        assert_ne!(
            package_closure_digest(&lockfile_a, "node_modules/native"),
            package_closure_digest(&lockfile_b, "node_modules/native"),
            "a transitive dependency change must invalidate the closure"
        );
    }

    #[test]
    fn dependency_placement_hoisting_flips_a_package_closure() {
        // Same versions, but `headers` moves between a hoisted and a nested
        // node_modules path. npm resolution observes hoisting, so the closure
        // digest must change even though the version did not.
        let packages = [
            pkg("node_modules/native", "1.0.0", "sha512-native"),
            pkg("node_modules/headers", "2.0.0", "sha512-headers"),
            pkg(
                "node_modules/native/node_modules/headers",
                "2.0.0",
                "sha512-headers",
            ),
        ];
        let lockfile_hoisted = closure_lockfile(
            &packages,
            &[
                (
                    "node_modules/native",
                    res(&[("headers", "node_modules/headers")]),
                ),
                ("node_modules/headers", res(&[])),
            ],
        );
        let lockfile_nested = closure_lockfile(
            &packages,
            &[
                (
                    "node_modules/native",
                    res(&[("headers", "node_modules/native/node_modules/headers")]),
                ),
                ("node_modules/native/node_modules/headers", res(&[])),
            ],
        );

        assert_ne!(
            package_closure_digest(&lockfile_hoisted, "node_modules/native"),
            package_closure_digest(&lockfile_nested, "node_modules/native"),
            "a hoisting change affects npm resolution and must invalidate the closure"
        );
    }

    #[test]
    fn dependency_cycle_terminates_and_is_deterministic() {
        // native -> a -> b -> a (cycle). Must not recurse forever, and the
        // digest must be stable across calls.
        let lockfile = closure_lockfile(
            &[
                pkg("node_modules/native", "1.0.0", "sha512-native"),
                pkg("node_modules/a", "1.0.0", "sha512-a"),
                pkg("node_modules/b", "1.0.0", "sha512-b"),
            ],
            &[
                ("node_modules/native", res(&[("a", "node_modules/a")])),
                ("node_modules/a", res(&[("b", "node_modules/b")])),
                ("node_modules/b", res(&[("a", "node_modules/a")])),
            ],
        );
        let first = package_closure_digest(&lockfile, "node_modules/native");
        let second = package_closure_digest(&lockfile, "node_modules/native");
        assert_eq!(first, second, "cyclic closure digest must be deterministic");
        assert_ne!(first, [0u8; 32]);
    }
}
