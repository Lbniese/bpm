//! Lossless `package.json` dependency editing and crash-safe two-file
//! publishing.
//!
//! The typed [`crate::manifest::PackageManifest`] is a *subset* of npm's
//! package.json schema: serializing it would drop `license`, `exports`,
//! `files`, `publishConfig`, and tool configuration. Dependency mutation must
//! instead edit the raw JSON document and reparse it through the typed
//! manifest for validation. This module owns that lossless edit and the
//! crash-bounded publication of the edited manifest alongside its lock.
//!
//! This module never resolves packages, never invokes the resolver, and never
//! chooses a lock kind. It edits one document and publishes two byte buffers.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use thiserror::Error;

use crate::manifest::{ManifestError, PackageManifest};

/// The npm dependency sections this editor can read or mutate.
pub const DEPENDENCY_SECTIONS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
];

/// The two sections a local add may write. Optional and peer mutation are
/// deferred to a later source-protocol plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencySection {
    Production,
    Dev,
}

impl DependencySection {
    /// The JSON object key this section is stored under.
    pub fn json_key(self) -> &'static str {
        match self {
            DependencySection::Production => "dependencies",
            DependencySection::Dev => "devDependencies",
        }
    }
}

/// Error while loading, editing, or rendering a manifest document.
#[derive(Debug, Error)]
pub enum ManifestEditError {
    #[error("cannot read package.json at {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("invalid JSON in package.json at {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("package.json at {path} is not a JSON object at the top level")]
    NotObject { path: PathBuf },
    #[error("dependency section \"{section}\" in {path} is not a JSON object")]
    SectionNotObject { path: PathBuf, section: String },
    #[error(
        "package \"{name}\" is already declared in optionalDependencies or peerDependencies; remove it there first or use a supported section"
    )]
    AmbiguousDependency { path: PathBuf, name: String },
}

/// A lossless view of a `package.json` for dependency editing.
///
/// Stores the parsed JSON object (so unknown top-level fields survive), the
/// original bytes (for no-op detection and rollback), and the detected
/// trailing-newline policy (preserved on render).
#[derive(Debug)]
pub struct ManifestDocument {
    source_path: PathBuf,
    root: Map<String, Value>,
    original_bytes: Vec<u8>,
    trailing_newline: bool,
}

impl ManifestDocument {
    /// Load a manifest document from a file path.
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, ManifestEditError> {
        let path = path.into();
        let bytes = fs::read(&path).map_err(|source| ManifestEditError::Read {
            path: path.clone(),
            source,
        })?;
        Self::from_bytes(bytes, path)
    }

    /// Parse a manifest document from its bytes and source path.
    pub fn from_bytes(
        bytes: Vec<u8>,
        source_path: impl Into<PathBuf>,
    ) -> Result<Self, ManifestEditError> {
        let source_path = source_path.into();
        let trailing_newline = bytes.last() == Some(&b'\n');
        let root_value: Value =
            serde_json::from_slice(&bytes).map_err(|source| ManifestEditError::Parse {
                path: source_path.clone(),
                source,
            })?;
        let Value::Object(root) = root_value else {
            return Err(ManifestEditError::NotObject { path: source_path });
        };
        Ok(Self {
            source_path,
            root,
            original_bytes: bytes,
            trailing_newline,
        })
    }

    /// The source path this document was loaded from.
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    /// The exact bytes the document was loaded from.
    pub fn original_bytes(&self) -> &[u8] {
        &self.original_bytes
    }

    /// Whether `name` appears in any of the four dependency sections.
    pub fn has_dependency(&self, name: &str) -> bool {
        DEPENDENCY_SECTIONS
            .iter()
            .any(|section| self.contains(section, name))
    }

    fn contains(&self, section: &str, name: &str) -> bool {
        self.root
            .get(section)
            .and_then(Value::as_object)
            .is_some_and(|map| map.contains_key(name))
    }

    /// Add `name -> spec` to `section`, applying npm's dual-section rule:
    /// adding to `dependencies` removes a same-name entry from
    /// `devDependencies` and vice-versa. If the name already lives in
    /// `optionalDependencies` or `peerDependencies`, return an ambiguity
    /// error rather than silently moving it.
    pub fn add_dependency(
        &mut self,
        section: DependencySection,
        name: &str,
        spec: &str,
    ) -> Result<(), ManifestEditError> {
        if self.contains("optionalDependencies", name) || self.contains("peerDependencies", name) {
            return Err(ManifestEditError::AmbiguousDependency {
                path: self.source_path.clone(),
                name: name.to_string(),
            });
        }
        self.ensure_section_object(section.json_key())?;
        let map = self
            .root
            .entry(section.json_key().to_string())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("section validated as object");
        map.insert(name.to_string(), Value::String(spec.to_string()));
        // Move out of the dual section so a name never lives in both
        // `dependencies` and `devDependencies`.
        let dual = match section {
            DependencySection::Production => "devDependencies",
            DependencySection::Dev => "dependencies",
        };
        if let Some(Value::Object(map)) = self.root.get_mut(dual) {
            map.remove(name);
        }
        Ok(())
    }

    /// Remove `name` from every dependency section. Returns whether anything
    /// was removed (used by `remove` to detect a no-op).
    pub fn remove_dependency(&mut self, name: &str) -> bool {
        let mut removed = false;
        for section in DEPENDENCY_SECTIONS {
            if let Some(Value::Object(map)) = self.root.get_mut(section) {
                removed |= map.remove(name).is_some();
            }
        }
        removed
    }

    /// Reject a section that exists but is not a JSON object. A missing
    /// section is fine; [`Self::add_dependency`] creates it on demand.
    fn ensure_section_object(&self, section: &str) -> Result<(), ManifestEditError> {
        match self.root.get(section) {
            None | Some(Value::Object(_)) => Ok(()),
            Some(_) => Err(ManifestEditError::SectionNotObject {
                path: self.source_path.clone(),
                section: section.to_string(),
            }),
        }
    }

    /// Render the edited document to canonical two-space JSON, preserving the
    /// original trailing-newline policy. Unknown top-level fields and
    /// unmodified values survive unchanged at the data level; only whitespace
    /// is normalized.
    pub fn render(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec_pretty(&Value::Object(self.root.clone()))
            .expect("JSON document is always serializable");
        if self.trailing_newline {
            out.push(b'\n');
        }
        out
    }

    /// Whether rendering produces bytes different from the original.
    pub fn changed(&self) -> bool {
        self.render() != self.original_bytes
    }

    /// Reparse the rendered document through the typed manifest so the
    /// resolver never consumes a hand-built structure.
    pub fn to_manifest(&self) -> Result<PackageManifest, ManifestError> {
        let rendered = self.render();
        let text = String::from_utf8(rendered).unwrap_or_default();
        PackageManifest::from_json(&text, &self.source_path)
    }
}

/// The two files a mutation can publish.
#[derive(Debug, Clone)]
pub struct PublishPlan {
    pub manifest_path: PathBuf,
    pub manifest_bytes: Vec<u8>,
    pub lock_path: PathBuf,
    pub lock_bytes: Vec<u8>,
}

/// The publish stage at which an injected failure should occur. Production
/// callers use [`publish`]; tests use [`publish_with_failure`] to prove
/// rollback restores both destinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishStage {
    /// Fail just before publishing the lock (after temps are written).
    Lock,
    /// Fail just after the lock is published, before the manifest.
    Manifest,
}

/// Publish the manifest and lock with best-effort cross-file rollback.
///
/// Both temp files are written first; the lock is then published, then the
/// manifest. If either publish (or an injected failure between them) errors,
/// both destinations are restored to their pre-publish bytes or absence.
pub fn publish(plan: &PublishPlan) -> io::Result<()> {
    publish_impl(plan, None)
}

/// Publish with a deterministic injected failure at `fail_at`. Exposed for
/// rollback tests; production code calls [`publish`].
pub fn publish_with_failure(plan: &PublishPlan, fail_at: PublishStage) -> io::Result<()> {
    publish_impl(plan, Some(fail_at))
}

#[derive(Clone)]
struct Original {
    existed: bool,
    bytes: Option<Vec<u8>>,
}

fn capture_original(path: &Path) -> io::Result<Original> {
    match fs::read(path) {
        Ok(bytes) => Ok(Original {
            existed: true,
            bytes: Some(bytes),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Original {
            existed: false,
            bytes: None,
        }),
        Err(error) => Err(error),
    }
}

fn restore(path: &Path, original: &Original) -> io::Result<()> {
    match original.bytes.as_deref() {
        Some(bytes) if original.existed => {
            // Restore in-place via a sibling temp + rename so a crash between
            // truncate and write cannot leave the file empty.
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let tmp = unique_sibling(
                parent,
                path.file_name().and_then(|n| n.to_str()).unwrap_or("doc"),
            );
            write_temp(&tmp, bytes)?;
            rename_or_replace(&tmp, path)
        }
        _ => {
            if path.exists() {
                fs::remove_file(path)?;
            }
            Ok(())
        }
    }
}

fn publish_impl(plan: &PublishPlan, fail_at: Option<PublishStage>) -> io::Result<()> {
    let manifest_original = capture_original(&plan.manifest_path)?;
    let lock_original = capture_original(&plan.lock_path)?;

    let manifest_parent = plan
        .manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let lock_parent = plan.lock_path.parent().unwrap_or_else(|| Path::new("."));
    let manifest_tmp = unique_sibling(
        manifest_parent,
        plan.manifest_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("package.json"),
    );
    let lock_tmp = unique_sibling(
        lock_parent,
        plan.lock_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("bpm.lock"),
    );

    let outcome = (|| -> io::Result<()> {
        write_temp(&manifest_tmp, &plan.manifest_bytes)?;
        write_temp(&lock_tmp, &plan.lock_bytes)?;

        if matches!(fail_at, Some(PublishStage::Lock)) {
            return Err(io::Error::other("injected failure before lock publish"));
        }

        rename_or_replace(&lock_tmp, &plan.lock_path)?;
        // The lock temp has been consumed by the rename. Only the manifest
        // temp remains to clean up on failure.
        if matches!(fail_at, Some(PublishStage::Manifest)) {
            return Err(io::Error::other("injected failure before manifest publish"));
        }

        rename_or_replace(&manifest_tmp, &plan.manifest_path)?;
        Ok(())
    })();

    // Best-effort cleanup of any temp that was not consumed by a rename.
    let _ = fs::remove_file(&manifest_tmp);
    let _ = fs::remove_file(&lock_tmp);

    if outcome.is_err() {
        // Restore both destinations to their pre-publish state. A published
        // lock must be rolled back so the manifest (restored to its prior
        // bytes) and lock never disagree.
        let _ = restore(&plan.lock_path, &lock_original);
        let _ = restore(&plan.manifest_path, &manifest_original);
    }
    outcome
}

/// Write a unique sibling temp file with `O_CREAT|O_EXCL` so two concurrent
/// mutations cannot clobber each other's temp. Syncs before returning so the
/// bytes survive a crash immediately after publication.
fn write_temp(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Rename `tmp` onto `dest`, replacing an existing destination. On Unix this
/// is a single atomic rename; on Windows, where `rename` refuses to overwrite,
/// the destination is removed first. The cross-file boundary (manifest +
/// lock) is still not globally atomic — see [`publish`] and the plan's crash
/// documentation.
fn rename_or_replace(tmp: &Path, dest: &Path) -> io::Result<()> {
    match fs::rename(tmp, dest) {
        Ok(()) => Ok(()),
        Err(error) => {
            if dest.exists() {
                let _ = fs::remove_file(dest);
                fs::rename(tmp, dest)
            } else {
                Err(error)
            }
        }
    }
}

fn unique_sibling(parent: &Path, hint: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let safe_hint = hint.replace('/', "_");
    parent.join(format!(".{safe_hint}.bpm-{pid}-{nanos}.tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn doc(json: &str) -> ManifestDocument {
        ManifestDocument::from_bytes(json.as_bytes().to_vec(), Path::new("package.json")).unwrap()
    }

    #[test]
    fn preserves_unknown_top_level_fields() {
        let mut document = doc(
            r#"{"name":"app","license":"MIT","exports":{".":"./index.js"},
            "publishConfig":{"access":"public"},"dependencies":{"a":"^1.0.0"}}"#,
        );
        document
            .add_dependency(DependencySection::Production, "b", "^2.0.0")
            .unwrap();
        let manifest = document.to_manifest().unwrap();
        assert_eq!(
            manifest.dependencies.get("b").map(String::as_str),
            Some("^2.0.0")
        );
        let rendered = String::from_utf8(document.render()).unwrap();
        assert!(rendered.contains("\"license\""), "{rendered}");
        assert!(rendered.contains("MIT"), "{rendered}");
        assert!(rendered.contains("\"exports\""), "{rendered}");
        assert!(rendered.contains("\"publishConfig\""), "{rendered}");
    }

    #[test]
    fn keeps_dependency_names_sorted_within_a_modified_section() {
        let mut document = doc(r#"{"dependencies":{"zebra":"^1.0.0"}}"#);
        document
            .add_dependency(DependencySection::Production, "apple", "^2.0.0")
            .unwrap();
        document
            .add_dependency(DependencySection::Production, "mango", "^3.0.0")
            .unwrap();
        let manifest = document.to_manifest().unwrap();
        assert_eq!(
            manifest.dependencies.keys().collect::<Vec<_>>(),
            vec!["apple", "mango", "zebra"]
        );
    }

    #[test]
    fn adding_to_dependencies_moves_out_of_devdependencies() {
        let mut document =
            doc(r#"{"dependencies":{"a":"^1.0.0"},"devDependencies":{"b":"^2.0.0"}}"#);
        document
            .add_dependency(DependencySection::Production, "b", "^2.0.0")
            .unwrap();
        let manifest = document.to_manifest().unwrap();
        assert!(manifest.dependencies.contains_key("b"));
        assert!(!manifest.dev_dependencies.contains_key("b"));
    }

    #[test]
    fn adding_to_devdependencies_moves_out_of_dependencies() {
        let mut document = doc(r#"{"dependencies":{"a":"^1.0.0"}}"#);
        document
            .add_dependency(DependencySection::Dev, "a", "^1.0.0")
            .unwrap();
        let manifest = document.to_manifest().unwrap();
        assert!(!manifest.dependencies.contains_key("a"));
        assert!(manifest.dev_dependencies.contains_key("a"));
    }

    #[test]
    fn rejects_ambiguous_optional_or_peer_declaration() {
        let mut document =
            doc(r#"{"dependencies":{"a":"^1.0.0"},"optionalDependencies":{"b":"^2.0.0"}}"#);
        let error = document
            .add_dependency(DependencySection::Production, "b", "^2.0.0")
            .unwrap_err();
        assert!(matches!(
            error,
            ManifestEditError::AmbiguousDependency { .. }
        ));
    }

    #[test]
    fn rejects_non_object_dependency_section() {
        let mut document = doc(r#"{"dependencies":["nope"]}"#);
        let error = document
            .add_dependency(DependencySection::Production, "b", "^2.0.0")
            .unwrap_err();
        assert!(matches!(error, ManifestEditError::SectionNotObject { .. }));
    }

    #[test]
    fn rejects_non_object_root() {
        let error = ManifestDocument::from_bytes(b"[1,2,3]".to_vec(), Path::new("package.json"))
            .unwrap_err();
        assert!(matches!(error, ManifestEditError::NotObject { .. }));
    }

    #[test]
    fn preserves_trailing_newline_policy() {
        let with_newline = doc("{\"name\":\"app\"}\n");
        let without_newline = doc("{\"name\":\"app\"}");
        assert!(with_newline.render().ends_with(b"\n"));
        assert!(!without_newline.render().ends_with(b"\n"));
    }

    #[test]
    fn no_op_remove_reports_no_change_and_real_remove_reports_change() {
        let mut document = doc(r#"{"name":"app","dependencies":{"a":"^1.0.0"}}"#);
        // A remove that finds nothing is a no-op signal so the orchestrator
        // can skip rewriting the file entirely.
        assert!(!document.remove_dependency("missing"));
        // A remove that hits a real entry reports the change.
        assert!(document.remove_dependency("a"));
        let manifest = document.to_manifest().unwrap();
        assert_eq!(manifest.dependency_count(), 0);
    }

    #[test]
    fn remove_strips_from_every_section() {
        let mut document = doc(
            r#"{"dependencies":{"a":"^1.0.0"},"devDependencies":{"a":"^1.0.0"},
            "optionalDependencies":{"a":"^1.0.0"},"peerDependencies":{"a":"^1.0.0"}}"#,
        );
        assert!(document.remove_dependency("a"));
        let manifest = document.to_manifest().unwrap();
        assert_eq!(manifest.dependency_count(), 0);
    }

    #[test]
    fn render_is_byte_stable_across_repeated_calls() {
        let mut document = doc(r#"{"dependencies":{"a":"^1.0.0"}}"#);
        document
            .add_dependency(DependencySection::Production, "b", "^2.0.0")
            .unwrap();
        let first = document.render();
        let second = document.render();
        assert_eq!(first, second);
    }

    #[test]
    fn scoped_names_are_preserved() {
        let mut document = doc(r#"{"dependencies":{}}"#);
        document
            .add_dependency(DependencySection::Production, "@scope/pkg", "^1.0.0")
            .unwrap();
        let manifest = document.to_manifest().unwrap();
        assert_eq!(
            manifest.dependencies.get("@scope/pkg").map(String::as_str),
            Some("^1.0.0")
        );
    }

    fn plan_for(dir: &Path, manifest: &str, lock: &str) -> PublishPlan {
        PublishPlan {
            manifest_path: dir.join("package.json"),
            manifest_bytes: manifest.as_bytes().to_vec(),
            lock_path: dir.join("bpm.lock"),
            lock_bytes: lock.as_bytes().to_vec(),
        }
    }

    #[test]
    fn publish_writes_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_for(dir.path(), r#"{"name":"app"}"#, r#"{"lock":"v1"}"#);
        publish(&plan).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("package.json")).unwrap(),
            r#"{"name":"app"}"#
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("bpm.lock")).unwrap(),
            r#"{"lock":"v1"}"#
        );
    }

    #[test]
    fn injected_failure_before_lock_restores_both_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), r#"{"name":"old"}"#).unwrap();
        fs::write(dir.path().join("bpm.lock"), r#"{"lock":"old"}"#).unwrap();
        let plan = plan_for(dir.path(), r#"{"name":"new"}"#, r#"{"lock":"new"}"#);
        let error = publish_with_failure(&plan, PublishStage::Lock).unwrap_err();
        assert!(error.to_string().contains("injected failure"));
        assert_eq!(
            fs::read_to_string(dir.path().join("package.json")).unwrap(),
            r#"{"name":"old"}"#
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("bpm.lock")).unwrap(),
            r#"{"lock":"old"}"#
        );
    }

    #[test]
    fn injected_failure_before_manifest_restores_both_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), r#"{"name":"old"}"#).unwrap();
        fs::write(dir.path().join("bpm.lock"), r#"{"lock":"old"}"#).unwrap();
        let plan = plan_for(dir.path(), r#"{"name":"new"}"#, r#"{"lock":"new"}"#);
        publish_with_failure(&plan, PublishStage::Manifest).unwrap_err();
        // The lock was published and then rolled back to its original bytes.
        assert_eq!(
            fs::read_to_string(dir.path().join("bpm.lock")).unwrap(),
            r#"{"lock":"old"}"#
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("package.json")).unwrap(),
            r#"{"name":"old"}"#
        );
        // No temp files left behind.
        let leftover = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".bpm-"))
            .count();
        assert_eq!(leftover, 0);
    }

    #[test]
    fn injected_failure_restores_absence_when_files_did_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_for(dir.path(), r#"{"name":"new"}"#, r#"{"lock":"new"}"#);
        publish_with_failure(&plan, PublishStage::Manifest).unwrap_err();
        assert!(!dir.path().join("package.json").exists());
        assert!(!dir.path().join("bpm.lock").exists());
    }
}
