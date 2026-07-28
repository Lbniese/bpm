//! `bpm view` — show package metadata from the registry (npm `view` compat).
//!
//! Fetches a package's packument from the configured registry, resolves the
//! requested version (defaulting to `dist-tags.latest`), and prints the
//! resolved version's metadata. An optional field selector prints just one
//! field (e.g. `dependencies`, `dist.tarball`, `versions`). The command is
//! read-only — it never modifies the store or any project files.
//!
//! Only the resolution-relevant fields bpm already extracts from each version
//! are shown (dependencies, optional/peer dependencies, bin, dist, engines,
//! os/cpu/libc, deprecated); richer manifest fields such as `description`,
//! `license`, or `homepage` are not retained by the resolver today.

use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;

use bpm::config::NpmConfig;
use bpm::http::{HttpClient, HttpError};
use bpm::metadata_cache::{CacheMode, MetadataCache};
use bpm::registry::{self, Packument, RegistryClient, RegistryError, VersionMetadata};
use serde::Serialize;

pub(super) struct Options {
    pub package: String,
    pub field: Option<String>,
    pub registry: Option<String>,
    pub store: Option<PathBuf>,
    pub offline: bool,
    pub json: bool,
}

pub(super) fn run(opts: Options) -> anyhow::Result<()> {
    let spec = registry::parse_spec(&opts.package).map_err(|e| anyhow::anyhow!("{e}"))?;

    // 1. Build the registry client (mirrors `bpm outdated`).
    let cwd = env::current_dir()?;
    let store_root = store_root_or_else(opts.store)?;
    let home = env::var_os("HOME").map(PathBuf::from);
    let config = NpmConfig::load(&cwd, home.as_deref())
        .map_err(|e| anyhow::anyhow!("failed to load npm config: {e}"))?;

    let effective_registry = opts
        .registry
        .or_else(|| env::var_os("BPM_REGISTRY").map(|s| s.to_string_lossy().into_owned()));
    let config = match effective_registry {
        Some(r) => config
            .with_registry_override(&r)
            .map_err(|e| anyhow::anyhow!("invalid registry override: {e}"))?,
        None => config,
    };

    let cache_mode = if opts.offline {
        CacheMode::Offline
    } else {
        CacheMode::Default
    };
    let http = HttpClient::new(config.clone());
    let metadata_cache = MetadataCache::open(&store_root).ok();
    let mut client = RegistryClient::with_client(config, http);
    if let Some(cache) = metadata_cache {
        client = client.with_metadata_cache(std::sync::Arc::new(cache), cache_mode);
    }

    // 2. Always fetch the full packument so dist-tags and the version list are
    //    available even for an exact/range request. A registry 404 surfaces as
    //    `Network` wrapping `HttpError::Status { code: 404 }`; a packument that
    //    parsed but has no versions surfaces as `NoVersions`. Both map to a
    //    clear "not found" message.
    let packument = client.packument(&spec.name).map_err(|e| {
        let not_found = match &e {
            RegistryError::Network { source, .. }
                if source
                    .downcast_ref::<HttpError>()
                    .map(|h| matches!(h, HttpError::Status { code: 404, .. }))
                    .unwrap_or(false) =>
            {
                true
            }
            RegistryError::NoVersions { .. } => true,
            _ => false,
        };
        if not_found {
            anyhow::anyhow!(
                "package '{}' not found on registry (or has no published versions)",
                spec.name
            )
        } else {
            anyhow::anyhow!("{e}")
        }
    })?;

    // 3. Resolve the target version and print.
    match opts.field.as_deref() {
        Some("versions") => print_versions(&packument, opts.json),
        Some("dist-tags") | Some("distTags") => print_dist_tags(&packument, opts.json),
        Some(path) => {
            let manifest = resolve_manifest(&spec, &packument)?;
            let value = serde_json::to_value(&manifest)?;
            match walk(&value, path) {
                Some(v) => {
                    print_field(v, opts.json);
                    Ok(())
                }
                None => anyhow::bail!(
                    "no field '{}' for {}@{}",
                    path,
                    manifest.name,
                    manifest.version
                ),
            }
        }
        None => {
            let manifest = resolve_manifest(&spec, &packument)?;
            if opts.json {
                println!("{}", serde_json::to_string_pretty(&manifest)?);
            } else {
                print_human(&manifest, &packument);
            }
            Ok(())
        }
    }
}

/// Resolve the target version and project its metadata into a serializable view.
fn resolve_manifest(
    spec: &registry::PackageSpec,
    packument: &Packument,
) -> anyhow::Result<ViewManifest> {
    let version = registry::select_version(&spec.name, &spec.req, packument)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let meta = packument
        .versions
        .get(version.to_string().as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "registry returned no metadata for {}@{}",
                spec.name,
                version
            )
        })?;
    Ok(ViewManifest::from(meta))
}

/// The version metadata projected into npm-conventional (camelCase) field names.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ViewManifest {
    name: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    deprecated: Option<String>,
    dependencies: BTreeMap<String, String>,
    optional_dependencies: BTreeMap<String, String>,
    peer_dependencies: BTreeMap<String, String>,
    bin: BTreeMap<String, String>,
    dist: ViewDist,
    engines: BTreeMap<String, String>,
    os: Vec<String>,
    cpu: Vec<String>,
    libc: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ViewDist {
    tarball: String,
    integrity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    shasum: Option<String>,
}

impl From<&VersionMetadata> for ViewManifest {
    fn from(m: &VersionMetadata) -> Self {
        Self {
            name: m.name.clone(),
            version: m.version.to_string(),
            deprecated: m.deprecated.clone(),
            dependencies: m.dependencies.clone(),
            optional_dependencies: m.optional_dependencies.clone(),
            peer_dependencies: m.peer_dependencies.clone(),
            bin: m.bin.clone(),
            dist: ViewDist {
                tarball: m.dist.tarball.clone(),
                integrity: m.dist.integrity.clone(),
                shasum: m.dist.shasum.clone(),
            },
            engines: m.engines.clone(),
            os: m.os.clone(),
            cpu: m.cpu.clone(),
            libc: m.libc.clone(),
        }
    }
}

/// Walk a dotted field path into a JSON value (e.g. `dist.tarball`).
fn walk<'v>(value: &'v serde_json::Value, path: &str) -> Option<&'v serde_json::Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

/// Print one field value: scalars raw, objects/arrays pretty (or JSON under
/// `--json`).
fn print_field(value: &serde_json::Value, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).expect("valid json value")
        );
        return;
    }
    match value {
        serde_json::Value::Null => println!("null"),
        serde_json::Value::String(s) => println!("{s}"),
        serde_json::Value::Number(n) => println!("{n}"),
        serde_json::Value::Bool(b) => println!("{b}"),
        serde_json::Value::Array(items) => {
            if items.iter().all(|v| {
                !matches!(
                    v,
                    serde_json::Value::Array(_) | serde_json::Value::Object(_)
                )
            }) {
                for v in items {
                    match v {
                        serde_json::Value::String(s) => println!("{s}"),
                        other => println!("{other}"),
                    }
                }
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(value).expect("valid json value")
                );
            }
        }
        serde_json::Value::Object(_) => {
            println!(
                "{}",
                serde_json::to_string_pretty(value).expect("valid json value")
            );
        }
    }
}

/// Print the list of all published versions (packument-level field).
fn print_versions(packument: &Packument, json: bool) -> anyhow::Result<()> {
    let keys: Vec<&str> = packument.versions.keys().map(String::as_str).collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&keys)?);
    } else {
        for key in &keys {
            println!("{key}");
        }
    }
    Ok(())
}

/// Print the dist-tags map (packument-level field).
fn print_dist_tags(packument: &Packument, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&packument.dist_tags)?);
    } else {
        for (tag, version) in &packument.dist_tags {
            println!("{tag}: {version}");
        }
    }
    Ok(())
}

/// Print a human-readable summary of the resolved version.
fn print_human(manifest: &ViewManifest, packument: &Packument) {
    println!("{}@{}", manifest.name, manifest.version);
    if let Some(reason) = &manifest.deprecated {
        println!("DEPRECATED: {reason}");
    }

    print_map("dist-tags", &packument.dist_tags);
    print_map("dependencies", &manifest.dependencies);
    print_map("optionalDependencies", &manifest.optional_dependencies);
    print_map("peerDependencies", &manifest.peer_dependencies);
    print_map("bin", &manifest.bin);
    print_map("engines", &manifest.engines);

    println!("\ndist:");
    println!("  tarball: {}", manifest.dist.tarball);
    println!("  integrity: {}", manifest.dist.integrity);
    if let Some(shasum) = &manifest.dist.shasum {
        println!("  shasum: {shasum}");
    }

    print_list("os", &manifest.os);
    print_list("cpu", &manifest.cpu);
    print_list("libc", &manifest.libc);

    println!("\nversions: {} published", packument.versions.len());
}

fn print_map(label: &str, map: &BTreeMap<String, String>) {
    if map.is_empty() {
        return;
    }
    println!("\n{label}:");
    for (key, value) in map {
        println!("  {key}: {value}");
    }
}

fn print_list(label: &str, list: &[String]) {
    if list.is_empty() {
        return;
    }
    println!("\n{label}:");
    println!("  {}", list.join(", "));
}

/// Resolve the store root from CLI flag, env var, or home directory default.
fn store_root_or_else(store: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    store
        .or_else(|| env::var_os("BPM_STORE").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".bpm")))
        .ok_or_else(|| anyhow::anyhow!("no --store given and $BPM_STORE/$HOME is unset"))
}
