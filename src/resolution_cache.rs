//! Persistent snapshots of completed dependency resolution.
//!
//! A snapshot is an acceleration cache, never the source of truth. It is
//! keyed by the complete resolver input identity and is only read by the
//! prefer-offline/offline install paths. Normal installs still validate
//! registry metadata so dist-tags and ranges retain npm-compatible freshness
//! behavior.

use std::fs;
use std::path::{Path, PathBuf};

use blake3::Hasher;

use crate::config::NpmConfig;
use crate::lockfile::Lockfile;
use crate::manifest::PackageManifest;
use crate::resolver::model::TargetPlatform;
use crate::resolver::peer::PeerMode;
use crate::resolver::workspaces::WorkspaceIndex;

const SNAPSHOT_DOMAIN: &[u8] = b"bpm-resolution-snapshot-v1\0";
const DIRECTORY_NAME: &str = "resolution-snapshots";

/// Store for completed resolver outputs under a BPM store root.
#[derive(Debug, Clone)]
pub struct ResolutionSnapshotCache {
    root: PathBuf,
}

impl ResolutionSnapshotCache {
    /// Create a cache handle without touching the filesystem.
    pub fn new(store_root: &Path) -> Self {
        Self {
            root: store_root.join(DIRECTORY_NAME),
        }
    }

    /// Load and validate a snapshot. Missing, malformed, or stale snapshots
    /// are treated as cache misses so a damaged acceleration cache cannot
    /// prevent a normal resolver fallback.
    pub fn load(&self, key: &str) -> Option<Lockfile> {
        let path = self.path_for(key);
        let bytes = fs::read(path).ok()?;
        let json = std::str::from_utf8(&bytes).ok()?;
        Lockfile::from_json(json).ok()
    }

    /// Persist a validated lockfile atomically. Snapshot publication is
    /// best-effort: callers should continue successfully if the store is
    /// read-only or the cache directory cannot be created.
    pub fn store(&self, key: &str, lockfile: &Lockfile) -> std::io::Result<()> {
        let json = lockfile
            .to_json()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        fs::create_dir_all(&self.root)?;
        let path = self.path_for(key);
        let temp = path.with_extension("tmp");
        fs::write(&temp, json.as_bytes())?;
        fs::rename(temp, path)
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.json"))
    }
}

/// Compute the stable cache key for all resolver inputs that can affect a
/// completed graph. Debug output supplies the ordered, non-secret npm
/// configuration fields; a separate one-way credential digest partitions
/// authenticated identities without exposing tokens. The domain prefix
/// invalidates the key on schema changes.
pub fn key_for(
    manifest: &PackageManifest,
    workspace: &WorkspaceIndex,
    config: &NpmConfig,
    peer_mode: PeerMode,
    target: &TargetPlatform,
) -> String {
    let mut hasher = Hasher::new();
    hasher.update(SNAPSHOT_DOMAIN);
    for value in [
        format!("manifest:{manifest:?}"),
        format!("workspace:{workspace:?}"),
        format!("config:{config:?}"),
        format!("peer-mode:{peer_mode:?}"),
        format!("target:{target:?}"),
    ] {
        hasher.update(value.as_bytes());
        hasher.update(&[0]);
    }
    hasher.update(b"credential-partition\0");
    hasher.update(&config.credential_partition_digest());
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn manifest() -> PackageManifest {
        PackageManifest::from_json(
            r#"{"name":"snapshot-test","version":"1.0.0","dependencies":{"a":"1.0.0"}}"#,
            Path::new("package.json"),
        )
        .unwrap()
    }

    fn workspace() -> WorkspaceIndex {
        WorkspaceIndex::default()
    }

    fn target() -> TargetPlatform {
        TargetPlatform {
            os: "darwin".into(),
            cpu: "arm64".into(),
            libc: None,
        }
    }

    fn config_from(source: &str) -> NpmConfig {
        let temp = tempfile::tempdir().unwrap();
        let npmrc = temp.path().join(".npmrc");
        fs::write(&npmrc, source).unwrap();
        NpmConfig::load_paths(None, Some(&npmrc)).unwrap()
    }

    fn snapshot_key(config: &NpmConfig) -> String {
        key_for(
            &manifest(),
            &workspace(),
            config,
            PeerMode::Strict,
            &target(),
        )
    }

    #[test]
    fn key_changes_when_resolver_inputs_change() {
        let manifest = manifest();
        let workspace = workspace();
        let config = NpmConfig::default();
        let target = target();
        let first = key_for(&manifest, &workspace, &config, PeerMode::Strict, &target);
        let second = key_for(
            &manifest,
            &workspace,
            &config,
            PeerMode::LegacyIgnore,
            &target,
        );
        assert_ne!(first, second);
    }

    #[test]
    fn roundtrip_uses_validated_lockfile() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ResolutionSnapshotCache::new(temp.path());
        let lockfile = Lockfile::new("bpm");
        cache.store("abc", &lockfile).unwrap();
        assert_eq!(cache.load("abc"), Some(lockfile));
    }

    #[test]
    fn malformed_snapshot_is_a_miss() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ResolutionSnapshotCache::new(temp.path());
        fs::create_dir_all(temp.path().join(DIRECTORY_NAME)).unwrap();
        fs::write(
            temp.path().join(DIRECTORY_NAME).join("bad.json"),
            b"not a lockfile",
        )
        .unwrap();
        assert!(cache.load("bad").is_none());
    }

    #[test]
    fn source_path_participates_in_key() {
        let mut first = manifest();
        first.source_dir = Some(PathBuf::from("one"));
        let mut second = first.clone();
        second.source_dir = Some(PathBuf::from("two"));
        let workspace = workspace();
        let config = NpmConfig::default();
        let target = target();
        assert_ne!(
            key_for(&first, &workspace, &config, PeerMode::Strict, &target),
            key_for(&second, &workspace, &config, PeerMode::Strict, &target)
        );
    }

    #[test]
    fn credential_partition_separates_same_scope_with_different_tokens() {
        const TOKEN_A: &str = "snapshot-placeholder-alpha";
        const TOKEN_B: &str = "snapshot-placeholder-bravo";
        let first = config_from(&format!(
            "registry=https://registry.example/\n//registry.example/:_authToken={TOKEN_A}\n"
        ));
        let second = config_from(&format!(
            "registry=https://registry.example/\n//registry.example/:_authToken={TOKEN_B}\n"
        ));

        assert_ne!(snapshot_key(&first), snapshot_key(&second));
        let first_debug = format!("{first:?}");
        let second_debug = format!("{second:?}");
        assert!(!first_debug.contains(TOKEN_A));
        assert!(!first_debug.contains(TOKEN_B));
        assert!(!second_debug.contains(TOKEN_A));
        assert!(!second_debug.contains(TOKEN_B));
    }

    #[test]
    fn credential_partition_separates_auth_path_scopes() {
        let root_scope = config_from(
            "registry=https://registry.example/\n//registry.example/:_authToken=shared-placeholder\n",
        );
        let private_scope = config_from(
            "registry=https://registry.example/\n//registry.example/private/:_authToken=shared-placeholder\n",
        );

        assert_ne!(snapshot_key(&root_scope), snapshot_key(&private_scope));
    }

    #[test]
    fn credential_partition_is_identical_for_effective_config_from_different_files() {
        let source = "registry=https://registry.example/\n//registry.example/private/:_authToken=stable-placeholder\n";
        let first = config_from(source);
        let second = config_from(source);

        assert_eq!(snapshot_key(&first), snapshot_key(&second));
    }

    #[test]
    fn credential_partition_never_persists_token_text() {
        const TOKEN_A: &str = "snapshot-file-secret-alpha";
        const TOKEN_B: &str = "snapshot-file-secret-bravo";
        let first = config_from(&format!(
            "registry=https://registry.example/\n//registry.example/:_authToken={TOKEN_A}\n"
        ));
        let second = config_from(&format!(
            "registry=https://registry.example/\n//registry.example/:_authToken={TOKEN_B}\n"
        ));
        let first_key = snapshot_key(&first);
        let second_key = snapshot_key(&second);
        let temp = tempfile::tempdir().unwrap();
        let cache = ResolutionSnapshotCache::new(temp.path());
        let lockfile = Lockfile::new("bpm");

        cache.store(&first_key, &lockfile).unwrap();
        cache.store(&second_key, &lockfile).unwrap();
        let first_path = cache.path_for(&first_key);
        let second_path = cache.path_for(&second_key);
        let first_json = fs::read_to_string(&first_path).unwrap();
        let second_json = fs::read_to_string(&second_path).unwrap();

        assert_eq!(first_json, lockfile.to_json().unwrap());
        assert_eq!(second_json, first_json);
        for value in [
            first_path.file_name().unwrap().to_string_lossy().as_ref(),
            second_path.file_name().unwrap().to_string_lossy().as_ref(),
            first_json.as_str(),
            second_json.as_str(),
        ] {
            assert!(!value.contains(TOKEN_A));
            assert!(!value.contains(TOKEN_B));
        }
    }
}
