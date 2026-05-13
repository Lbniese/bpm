//! Registry packument resolution: turn a package spec (`lodash`,
//! `lodash@4.17.21`, `@scope/pkg@^1.2.0`) into a concrete tarball URL and
//! integrity, the way `npm`/`bun` resolve a name before download.
//!
//! This is the small, self-contained end of dependency resolution. It does
//! *not* build a dependency graph — it resolves a single name to one tarball
//! and hands `(tarball_url, integrity)` to the existing immutable store, which
//! is unchanged.
//!
//! Behavior:
//! - `<name>`           -> `dist-tags.latest`
//! - `<name>@<version>` -> exact version (must exist in the packument)
//! - `<name>@<range>`   -> highest published version satisfying the range
//!   (`^`, `~`, `>=`, `x` ranges, `*`), via the `semver` crate
//!
//! Scoped names (`@scope/pkg`) are URL-encoded the way the npm registry
//! expects (`/` -> `%2F`) so the whole name is one path segment.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use semver::{Version, VersionReq};
use serde::{Deserialize, Deserializer};
use thiserror::Error;

use crate::config::NpmConfig;
use crate::http::HttpClient;
use crate::metadata_cache::{CacheMode, MetadataCache};

/// The abbreviated install-metadata media type npm negotiates for graph
/// resolution. Requesting it avoids downloading each packument's full
/// publish-time history (multi-megabyte for popular packages).
const ABBREV_ACCEPT: &str = "application/vnd.npm.install-v1+json";

/// How a spec asks for a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRange(Vec<VersionReq>);

impl VersionRange {
    pub fn parse(value: &str) -> Result<Self, semver::Error> {
        let mut ranges = Vec::new();
        for part in value.split("||") {
            let part = part.trim();
            if part.is_empty() {
                return Err(VersionReq::parse(part).unwrap_err());
            }
            ranges.push(VersionReq::parse(part)?);
        }
        Ok(Self(ranges))
    }

    pub fn matches(&self, version: &Version) -> bool {
        self.0.iter().any(|range| range.matches(version))
    }

    pub fn requirements(&self) -> &[VersionReq] {
        &self.0
    }
}

impl std::fmt::Display for VersionRange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let joined = self
            .0
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" || ");
        formatter.write_str(&joined)
    }
}

#[derive(Debug, Clone)]
pub enum VersionRequest {
    /// No version given: use `dist-tags.latest`.
    Latest,
    /// An exact version (`lodash@4.17.21`).
    Exact(Version),
    /// A semver range (`lodash@^4.17.0`).
    Range(VersionRange),
}

/// A parsed package spec: a name plus a version request.
#[derive(Debug, Clone)]
pub struct PackageSpec {
    pub name: String,
    pub req: VersionRequest,
}

/// Deterministic package metadata returned by an npm packument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packument {
    pub name: String,
    pub dist_tags: BTreeMap<String, String>,
    pub versions: BTreeMap<String, VersionMetadata>,
}

/// Metadata needed to resolve and install one concrete package version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMetadata {
    pub name: String,
    pub version: Version,
    pub deprecated: Option<String>,
    pub dependencies: BTreeMap<String, String>,
    pub optional_dependencies: BTreeMap<String, String>,
    pub peer_dependencies: BTreeMap<String, String>,
    pub peer_dependencies_meta: BTreeMap<String, PeerMeta>,
    pub bin: BTreeMap<String, String>,
    pub dist: Dist,
    pub engines: BTreeMap<String, String>,
    pub os: Vec<String>,
    pub cpu: Vec<String>,
    pub libc: Vec<String>,
    pub has_install_script: bool,
    pub has_shrinkwrap: bool,
}

/// Additional semantics for one peer dependency.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct PeerMeta {
    #[serde(default)]
    pub optional: bool,
}

/// Registry distribution data required by the immutable artifact store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Dist {
    #[serde(default)]
    pub tarball: String,
    #[serde(default)]
    pub integrity: String,
    #[serde(default)]
    pub shasum: Option<String>,
}

#[derive(Deserialize)]
struct WirePackument {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "dist-tags")]
    dist_tags: BTreeMap<String, String>,
    #[serde(default)]
    versions: BTreeMap<String, WireVersionMetadata>,
}

#[derive(Default, Deserialize)]
struct WireVersionMetadata {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    deprecated: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependenciesMeta")]
    peer_dependencies_meta: BTreeMap<String, PeerMeta>,
    #[serde(default)]
    bin: WireBin,
    #[serde(default)]
    dist: Dist,
    #[serde(default)]
    engines: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    os: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    cpu: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    libc: Vec<String>,
    #[serde(default, rename = "hasInstallScript")]
    has_install_script: bool,
    #[serde(default, rename = "_hasShrinkwrap")]
    has_shrinkwrap: bool,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum WireBin {
    #[default]
    Missing,
    Single(String),
    Map(BTreeMap<String, String>),
}

impl<'de> Deserialize<'de> for Packument {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WirePackument::deserialize(deserializer)?;
        let package_name = wire.name;
        let mut versions = BTreeMap::new();
        for (version_key, metadata) in wire.versions {
            if let Some(metadata) = version_metadata(&package_name, &version_key, metadata) {
                versions.insert(version_key, metadata);
            }
        }
        Ok(Self {
            name: package_name,
            dist_tags: wire.dist_tags,
            versions,
        })
    }
}

fn deserialize_string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringList {
        One(String),
        Many(Vec<String>),
    }

    Ok(match StringList::deserialize(deserializer)? {
        StringList::One(value) => vec![value],
        StringList::Many(values) => values,
    })
}

fn normalize_list(mut values: Vec<String>) -> Vec<String> {
    values.retain(|value| !value.is_empty());
    values.sort();
    values.dedup();
    values
}

fn version_metadata(
    package_name: &str,
    version_key: &str,
    metadata: WireVersionMetadata,
) -> Option<VersionMetadata> {
    let version_text = if metadata.version.is_empty() {
        version_key
    } else {
        metadata.version.as_str()
    };
    let version = Version::parse(version_text).ok()?;
    let name = if metadata.name.is_empty() {
        package_name.to_owned()
    } else {
        metadata.name
    };
    let mut bin = match metadata.bin {
        WireBin::Missing => BTreeMap::new(),
        WireBin::Single(path) => {
            BTreeMap::from([(name.rsplit('/').next().unwrap_or(&name).to_owned(), path)])
        }
        WireBin::Map(bin) => bin,
    };
    bin.retain(|command, path| !command.is_empty() && !path.is_empty());

    Some(VersionMetadata {
        name,
        version,
        deprecated: metadata.deprecated,
        dependencies: metadata.dependencies,
        optional_dependencies: metadata.optional_dependencies,
        peer_dependencies: metadata.peer_dependencies,
        peer_dependencies_meta: metadata.peer_dependencies_meta,
        bin,
        dist: metadata.dist,
        engines: metadata.engines,
        os: normalize_list(metadata.os),
        cpu: normalize_list(metadata.cpu),
        libc: normalize_list(metadata.libc),
        has_install_script: metadata.has_install_script,
        has_shrinkwrap: metadata.has_shrinkwrap,
    })
}

/// A fully resolved single package and all metadata needed by the resolver.
#[derive(Debug, Clone)]
pub struct ResolvedArtifact {
    pub name: String,
    pub version: Version,
    pub tarball_url: String,
    /// npm-style `sha512-<base64>` integrity from the registry `dist` block.
    pub integrity: String,
    pub metadata: VersionMetadata,
}

/// Configured registry facade sharing one pooled HTTP client across requests.
#[derive(Clone)]
pub struct RegistryClient {
    config: NpmConfig,
    http: HttpClient,
    /// Packuments are immutable for the lifetime of one resolution. Sharing
    /// this small cache avoids fetching the same transitive package once per
    /// physical placement (common with peer and nested dependency graphs).
    packument_cache: Arc<Mutex<BTreeMap<String, Packument>>>,
    /// Optional persistent response cache shared across runs. When present,
    /// packument fetches revalidate over the network with conditional
    /// requests (`If-None-Match` / `If-Modified-Since`) and reuse the stored
    /// body verbatim on a `304`. `None` preserves the legacy uncached path.
    metadata_cache: Option<Arc<MetadataCache>>,
    cache_mode: CacheMode,
}

impl std::fmt::Debug for RegistryClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegistryClient")
            .field("config", &self.config)
            .field("cache_mode", &self.cache_mode)
            .field("persistent_cache", &self.metadata_cache.is_some())
            .finish_non_exhaustive()
    }
}

impl RegistryClient {
    /// Return the effective registry for a package name.
    pub fn registry_for_package(&self, package: &str) -> &str {
        self.config.registry_for_package(package)
    }

    /// Return the pooled HTTP client backing this registry facade.
    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    /// Construct a registry facade with a newly allocated HTTP pool.
    ///
    /// This compatibility constructor is intended for callers that do not
    /// need to share the pool with artifact retrieval. Production pipelines
    /// should use [`Self::with_client`] with their caller-owned client.
    pub fn new(config: NpmConfig) -> Self {
        let http = HttpClient::new(config.clone());
        Self {
            config,
            http,
            packument_cache: Arc::new(Mutex::new(BTreeMap::new())),
            metadata_cache: None,
            cache_mode: CacheMode::Default,
        }
    }

    /// Construct a registry facade using the caller's pooled HTTP client.
    ///
    /// Cloning `http` before passing it here lets metadata and artifact
    /// requests share the same underlying agent and connection pool.
    pub fn with_client(config: NpmConfig, http: HttpClient) -> Self {
        Self {
            config,
            http,
            packument_cache: Arc::new(Mutex::new(BTreeMap::new())),
            metadata_cache: None,
            cache_mode: CacheMode::Default,
        }
    }

    /// Attach a persistent packument cache and select how it may be reused.
    ///
    /// When `cache_mode` forbids network access ([`CacheMode::Offline`]) the
    /// client resolves only against bodies already present in `cache`.
    pub fn with_metadata_cache(mut self, cache: Arc<MetadataCache>, cache_mode: CacheMode) -> Self {
        self.metadata_cache = Some(cache);
        self.cache_mode = cache_mode;
        self
    }

    /// The active cache reuse policy (`Default` when no cache is attached).
    pub fn cache_mode(&self) -> CacheMode {
        self.cache_mode
    }

    /// Fetch the configured scoped/default registry and resolve one package.
    pub fn resolve(&self, spec: &PackageSpec) -> Result<ResolvedArtifact, RegistryError> {
        let registry = self.config.registry_for_package(&spec.name);
        let packument = self.packument_for(spec)?;
        resolve_packument(spec, &packument, registry)
    }

    /// Fetch the smallest metadata document that can resolve `spec`.
    ///
    /// Exact versions have a dedicated registry endpoint and do not need the
    /// package's complete multi-megabyte version history. Ranges and tags use
    /// the abbreviated install metadata packument so dependency fields remain
    /// available without downloading npm's publish-time metadata.
    pub fn packument_for(&self, spec: &PackageSpec) -> Result<Packument, RegistryError> {
        match &spec.req {
            VersionRequest::Exact(version) => {
                let registry = self.config.registry_for_package(&spec.name);
                fetch_version_packument(
                    &self.http,
                    &spec.name,
                    version,
                    registry,
                    self.metadata_cache.as_deref(),
                    self.cache_mode,
                )
            }
            VersionRequest::Latest | VersionRequest::Range(_) => self.packument(&spec.name),
        }
    }

    /// Fetch a typed packument for use by dependency-graph resolution.
    pub fn packument(&self, name: &str) -> Result<Packument, RegistryError> {
        let registry = self.config.registry_for_package(name);
        let key = format!("{}\0{name}", registry.trim_end_matches('/'));
        if let Ok(cache) = self.packument_cache.lock() {
            if let Some(packument) = cache.get(&key) {
                return Ok(packument.clone());
            }
        }
        let packument = fetch_packument(
            &self.http,
            name,
            registry,
            self.metadata_cache.as_deref(),
            self.cache_mode,
        )?;
        if let Ok(mut cache) = self.packument_cache.lock() {
            cache.insert(key, packument.clone());
        }
        Ok(packument)
    }
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("invalid package spec '{0}': {1}")]
    InvalidSpec(String, String),
    #[error("registry request for {package} failed")]
    Network {
        package: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("registry returned status {code} for {package}")]
    BadStatus { package: String, code: u16 },
    #[error("registry response for {package} was not valid JSON")]
    BadJson {
        package: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("packument for {package} has no versions")]
    NoVersions { package: String },
    #[error("no version of {package} satisfies {req}")]
    VersionNotFound { package: String, req: String },
    #[error("packument for {package}@{version} is missing a tarball URL or integrity")]
    MissingDist { package: String, version: String },
    #[error("no cached metadata for {url}; --offline refused to contact the registry")]
    OfflineMiss { url: String },
}

/// Parse a package spec string into a name + version request.
///
/// The version separator is the last `@` that is not the leading scope marker
/// of a scoped name. So `@scope/pkg` has no version, but `@scope/pkg@1.2.3`
/// and `pkg@1.2.3` do.
pub fn parse_spec(spec: &str) -> Result<PackageSpec, RegistryError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(RegistryError::InvalidSpec(
            spec.to_string(),
            "spec is empty".to_string(),
        ));
    }

    let (name, req_str) = match spec.rfind('@') {
        // `@scope/pkg` (the only `@` is the leading scope marker) or bare `pkg`.
        Some(0) | None => (spec, None),
        // `<name>@<req>` or `@scope/name@<req>`.
        Some(i) => (&spec[..i], Some(&spec[i + 1..])),
    };

    if !is_valid_npm_name(name) {
        return Err(RegistryError::InvalidSpec(
            spec.to_string(),
            format!("'{name}' is not a valid npm package name"),
        ));
    }

    let req = match req_str.map(str::trim) {
        None | Some("") | Some("latest") => VersionRequest::Latest,
        Some(s) if s.starts_with(['^', '~', '>', '<', '=', '*']) => {
            VersionRequest::Range(VersionRange::parse(s).map_err(|e| {
                RegistryError::InvalidSpec(spec.to_string(), format!("bad range '{s}': {e}"))
            })?)
        }
        Some(s) => {
            // A bare version like `1.2.3` is exact; anything else (e.g. `1.x`)
            // is treated as a range.
            match Version::parse(s) {
                Ok(v) => VersionRequest::Exact(v),
                Err(_) => VersionRequest::Range(VersionRange::parse(s).map_err(|e| {
                    RegistryError::InvalidSpec(spec.to_string(), format!("bad version '{s}': {e}"))
                })?),
            }
        }
    };

    Ok(PackageSpec {
        name: name.to_string(),
        req,
    })
}

/// Compatibility API resolving `spec` against `registry` (a base URL like
/// `https://registry.npmjs.org`) by fetching the packument and selecting a
/// version with a default-config HTTP client.
///
/// Configured production callers should use [`RegistryClient::with_client`]
/// so metadata and artifact retrieval share one caller-owned pool.
pub fn resolve(spec: &PackageSpec, registry: &str) -> Result<ResolvedArtifact, RegistryError> {
    let http = HttpClient::new(NpmConfig::default());
    let packument = fetch_packument(&http, &spec.name, registry, None, CacheMode::Default)?;
    resolve_packument(spec, &packument, registry)
}

/// Select one version from an already-fetched packument.
pub fn resolve_packument(
    spec: &PackageSpec,
    packument: &Packument,
    registry: &str,
) -> Result<ResolvedArtifact, RegistryError> {
    let version = select_version(&spec.name, &spec.req, packument)?;
    let mut metadata = packument
        .versions
        .get(version.to_string().as_str())
        .ok_or_else(|| RegistryError::VersionNotFound {
            package: spec.name.clone(),
            req: version.to_string(),
        })?
        .clone();
    if metadata.name.is_empty() {
        metadata.name.clone_from(&spec.name);
    }
    if metadata.dist.tarball.is_empty() || metadata.dist.integrity.is_empty() {
        return Err(RegistryError::MissingDist {
            package: spec.name.clone(),
            version: version.to_string(),
        });
    }

    Ok(ResolvedArtifact {
        name: metadata.name.clone(),
        version,
        tarball_url: resolve_tarball_url(registry, &metadata.dist.tarball),
        integrity: metadata.dist.integrity.clone(),
        metadata,
    })
}

fn resolve_tarball_url(registry: &str, tarball: &str) -> String {
    if tarball.contains("://") || tarball.starts_with("file:") {
        tarball.to_string()
    } else {
        format!(
            "{}/{}",
            registry.trim_end_matches('/'),
            tarball.trim_start_matches('/')
        )
    }
}

/// Fetch and parse the packument JSON for `name` through the shared client.
fn fetch_packument(
    http: &HttpClient,
    name: &str,
    registry: &str,
    cache: Option<&MetadataCache>,
    mode: CacheMode,
) -> Result<Packument, RegistryError> {
    let base = registry.trim_end_matches('/');
    // npm encodes scoped names so the whole name is one path segment.
    let encoded = name.replace('/', "%2F");
    let url = format!("{base}/{encoded}");

    let body = fetch_with_cache(http, &url, name, cache, mode, true)?;
    serde_json::from_str(&body).map_err(|source| RegistryError::BadJson {
        package: name.to_string(),
        source,
    })
}

fn fetch_version_packument(
    http: &HttpClient,
    name: &str,
    version: &Version,
    registry: &str,
    cache: Option<&MetadataCache>,
    mode: CacheMode,
) -> Result<Packument, RegistryError> {
    let base = registry.trim_end_matches('/');
    let encoded = name.replace('/', "%2F");
    let url = format!("{base}/{encoded}/{version}");
    let body = fetch_with_cache(http, &url, name, cache, mode, false)?;
    let wire: WireVersionMetadata =
        serde_json::from_str(&body).map_err(|source| RegistryError::BadJson {
            package: name.to_string(),
            source,
        })?;
    let metadata = version_metadata(name, &version.to_string(), wire).ok_or_else(|| {
        RegistryError::NoVersions {
            package: name.to_string(),
        }
    })?;
    Ok(Packument {
        name: name.to_string(),
        dist_tags: BTreeMap::new(),
        versions: BTreeMap::from([(version.to_string(), metadata)]),
    })
}

/// Retrieve the response body for `url`, consulting and updating the optional
/// persistent cache according to `mode`.
///
/// Revalidation is npm-compatible: a cached entry's `ETag` / `Last-Modified`
/// are sent as `If-None-Match` / `If-Modified-Since`, a `304 Not Modified`
/// reuses the stored body verbatim, and a `200` refreshes the cache. The
/// stored body is byte-for-byte what the registry last sent, so resolution
/// output stays deterministic regardless of whether a request was served from
/// the cache or the network.
///
/// `send_abbreviated_accept` negotiates npm's abbreviated install-metadata
/// media type for packuments; per-version endpoints omit it.
fn fetch_with_cache(
    http: &HttpClient,
    url: &str,
    package: &str,
    cache: Option<&MetadataCache>,
    mode: CacheMode,
    send_abbreviated_accept: bool,
) -> Result<String, RegistryError> {
    // A persistent-cache read failure degrades to a miss for every mode: the
    // online modes fall back to a network fetch, and offline mode fails on the
    // resulting missing entry below.
    let cached = cache.and_then(|store| store.get(url).ok()).flatten();

    if !mode.allows_network() {
        return cached
            .map(|entry| entry.body)
            .map(|bytes| String::from_utf8(bytes).unwrap_or_default())
            .ok_or_else(|| RegistryError::OfflineMiss {
                url: url.to_string(),
            });
    }

    // PreferOffline may serve a still-cached body without any round-trip.
    if mode.serves_stale() {
        if let Some(entry) = cached.as_ref() {
            return Ok(String::from_utf8_lossy(&entry.body).into_owned());
        }
    }

    let mut headers: Vec<(&str, &str)> = Vec::new();
    if send_abbreviated_accept {
        headers.push(("Accept", ABBREV_ACCEPT));
    }
    if let Some(entry) = cached.as_ref() {
        if let Some(etag) = entry.etag.as_deref() {
            headers.push(("If-None-Match", etag));
        }
        if let Some(last_modified) = entry.last_modified.as_deref() {
            headers.push(("If-Modified-Since", last_modified));
        }
    }

    let response =
        http.get_with_headers(url, &headers)
            .map_err(|source| RegistryError::Network {
                package: package.to_string(),
                source: Box::new(source),
            })?;

    if response.status() == 304 {
        // The registry confirmed the cached body is still current. A validator
        // must have been sent for the registry to answer 304, so a cached entry
        // exists; otherwise treat the unexpected 304 as a protocol error.
        return cached
            .map(|entry| String::from_utf8_lossy(&entry.body).into_owned())
            .ok_or_else(|| RegistryError::BadStatus {
                package: package.to_string(),
                code: 304,
            });
    }

    let etag = response.header("ETag").map(str::to_owned);
    let last_modified = response.header("Last-Modified").map(str::to_owned);
    let body = response
        .into_string()
        .map_err(|error| RegistryError::Network {
            package: package.to_string(),
            source: Box::new(error),
        })?;

    // Best-effort refresh: a write failure must never fail an install.
    if let Some(store) = cache {
        let _ = store.put(
            url,
            body.as_bytes(),
            etag.as_deref(),
            last_modified.as_deref(),
        );
    }

    Ok(body)
}

/// Pick the target version string from a packument for a version request.
fn select_version(
    name: &str,
    req: &VersionRequest,
    packument: &Packument,
) -> Result<Version, RegistryError> {
    if packument.versions.is_empty() {
        return Err(RegistryError::NoVersions {
            package: name.to_string(),
        });
    }

    match req {
        VersionRequest::Latest => {
            let tag = packument
                .dist_tags
                .get("latest")
                .map(String::as_str)
                .ok_or_else(|| RegistryError::NoVersions {
                    package: name.to_string(),
                })?;
            Version::parse(tag).map_err(|_| RegistryError::VersionNotFound {
                package: name.to_string(),
                req: format!("latest ({tag})"),
            })
        }
        VersionRequest::Exact(v) => {
            if packument.versions.contains_key(v.to_string().as_str()) {
                Ok(v.clone())
            } else {
                Err(RegistryError::VersionNotFound {
                    package: name.to_string(),
                    req: format!("={v}"),
                })
            }
        }
        VersionRequest::Range(r) => {
            // Deterministic max: parse all, filter, take the greatest (prereleases
            // excluded by `semver` unless the range explicitly opts in).
            let mut matching: Vec<Version> = packument
                .versions
                .keys()
                .filter_map(|k| Version::parse(k).ok())
                .filter(|v| r.matches(v))
                .collect();
            matching.sort();
            matching
                .pop()
                .ok_or_else(|| RegistryError::VersionNotFound {
                    package: name.to_string(),
                    req: r.to_string(),
                })
        }
    }
}

/// Validate a package name per npm rules: `(@scope/)?name`, ASCII, <=214 chars,
/// each segment starts with a lowercase letter or digit and otherwise contains
/// only `[a-z0-9._-]`.
pub fn is_valid_npm_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 214 || !name.is_ascii() {
        return false;
    }
    match name.strip_prefix('@') {
        Some(rest) => match rest.split_once('/') {
            Some((scope, pkg)) => valid_segment(scope.as_bytes()) && valid_segment(pkg.as_bytes()),
            None => false,
        },
        None => valid_segment(name.as_bytes()),
    }
}

fn valid_segment(seg: &[u8]) -> bool {
    if seg.is_empty() {
        return false;
    }
    let first = seg[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    seg.iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::sync::Arc;

    use crate::metadata_cache::{CacheMode, MetadataCache};

    fn packument(value: serde_json::Value) -> Packument {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn parses_bare_name_as_latest() {
        let s = parse_spec("lodash").unwrap();
        assert_eq!(s.name, "lodash");
        assert!(matches!(s.req, VersionRequest::Latest));
    }

    #[test]
    fn parses_scoped_name_without_version_as_latest() {
        let s = parse_spec("@scope/pkg").unwrap();
        assert_eq!(s.name, "@scope/pkg");
        assert!(matches!(s.req, VersionRequest::Latest));
    }

    #[test]
    fn parses_exact_version() {
        let s = parse_spec("lodash@4.17.21").unwrap();
        assert_eq!(s.name, "lodash");
        match s.req {
            VersionRequest::Exact(v) => assert_eq!(v, Version::parse("4.17.21").unwrap()),
            other => panic!("expected exact, got {other:?}"),
        }
    }

    #[test]
    fn parses_scoped_exact_version() {
        let s = parse_spec("@scope/pkg@1.2.3").unwrap();
        assert_eq!(s.name, "@scope/pkg");
        assert!(matches!(s.req, VersionRequest::Exact(_)));
    }

    #[test]
    fn parses_caret_and_tilde_as_range() {
        for spec in [
            "lodash@^4.17.0",
            "lodash@~4.17.0",
            "lodash@>=4.0.0",
            "lodash@*",
            "lodash@4.x",
        ] {
            let s = parse_spec(spec).unwrap_or_else(|e| panic!("parse {spec}: {e}"));
            assert_eq!(s.name, "lodash");
            assert!(matches!(s.req, VersionRequest::Range(_)), "{spec}");
        }
    }

    #[test]
    fn scoped_single_bin_uses_unscoped_command_name() {
        let packument = packument(serde_json::json!({
            "name": "@scope/pkg",
            "versions": {
                "1.0.0": { "version": "1.0.0", "bin": "cli.js" }
            }
        }));
        let metadata = packument.versions.get("1.0.0").unwrap();
        assert_eq!(metadata.bin.keys().collect::<Vec<_>>(), vec!["pkg"]);
    }

    #[test]
    fn disjunctive_ranges_match_any_requirement() {
        let spec = parse_spec("js-tokens@^3.0.0 || ^4.0.0").unwrap();
        let VersionRequest::Range(range) = spec.req else {
            panic!("expected range");
        };
        assert!(range.matches(&Version::new(3, 0, 2)));
        assert!(range.matches(&Version::new(4, 0, 1)));
        assert!(!range.matches(&Version::new(2, 0, 0)));
        assert_eq!(range.to_string(), "^3.0.0 || ^4.0.0");
    }

    #[test]
    fn rejects_empty_spec() {
        assert!(parse_spec("").is_err());
        assert!(parse_spec("   ").is_err());
    }

    #[test]
    fn rejects_uppercase_and_invalid_names() {
        assert!(parse_spec("Lodash").is_err());
        assert!(parse_spec("has space").is_err());
        assert!(parse_spec("@noslash").is_err());
        assert!(parse_spec("@scope/").is_err());
    }

    #[test]
    fn rejects_bad_version() {
        assert!(parse_spec("lodash@not-a-version!").is_err());
    }

    #[test]
    fn name_validation_examples() {
        assert!(is_valid_npm_name("lodash"));
        assert!(is_valid_npm_name("@scope/pkg"));
        assert!(!is_valid_npm_name("Lodash"));
        assert!(!is_valid_npm_name(""));
        assert!(!is_valid_npm_name("@scope"));
        assert!(!is_valid_npm_name("has space"));
    }

    #[test]
    fn select_version_picks_latest_from_dist_tags() {
        let packument = packument(serde_json::json!({
            "name": "lodash",
            "dist-tags": { "latest": "4.17.21" },
            "versions": { "1.0.0": {}, "4.17.21": {} }
        }));
        let v = select_version("lodash", &VersionRequest::Latest, &packument).unwrap();
        assert_eq!(v, Version::parse("4.17.21").unwrap());
    }

    #[test]
    fn select_version_range_picks_highest_match() {
        let packument = packument(serde_json::json!({
            "name": "lodash",
            "versions": { "1.0.0": {}, "4.0.0": {}, "4.17.20": {}, "4.17.21": {}, "5.0.0": {} }
        }));
        let req = VersionRequest::Range(VersionRange::parse("^4.0.0").unwrap());
        let v = select_version("lodash", &req, &packument).unwrap();
        assert_eq!(v, Version::parse("4.17.21").unwrap());
    }

    #[test]
    fn select_version_exact_missing_errors() {
        let packument = packument(serde_json::json!({
            "name": "p",
            "versions": { "1.0.0": {} }
        }));
        let req = VersionRequest::Exact(Version::parse("2.0.0").unwrap());
        let err = select_version("p", &req, &packument).unwrap_err();
        assert!(matches!(err, RegistryError::VersionNotFound { .. }));
    }

    #[test]
    fn resolve_reads_tarball_and_integrity() {
        let packument = packument(serde_json::json!({
            "name": "p",
            "dist-tags": { "latest": "1.2.3" },
            "versions": {
                "1.2.3": {
                    "dist": {
                        "tarball": "https://example.test/p/-/p-1.2.3.tgz",
                        "integrity": "sha512-abc"
                    }
                }
            }
        }));
        let spec = parse_spec("p").unwrap();
        let resolved = resolve_packument(&spec, &packument, "https://example.test/").unwrap();
        assert_eq!(resolved.tarball_url, "https://example.test/p/-/p-1.2.3.tgz");
        assert_eq!(resolved.integrity, "sha512-abc");
    }

    #[test]
    fn exact_resolution_uses_version_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let registry = format!("http://{address}");
        let (request_tx, request_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            request_tx
                .send(String::from_utf8_lossy(&request[..read]).into_owned())
                .unwrap();
            let body = r#"{"name":"p","version":"1.2.3","dist":{"tarball":"https://example.test/p.tgz","integrity":"sha512-abc"}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });

        let config = NpmConfig::default()
            .with_registry_override(&registry)
            .unwrap();
        let client = RegistryClient::new(config);
        let resolved = client.resolve(&parse_spec("p@1.2.3").unwrap()).unwrap();
        server.join().unwrap();

        assert_eq!(resolved.version, Version::new(1, 2, 3));
        assert!(request_rx
            .recv()
            .unwrap()
            .starts_with("GET /p/1.2.3 HTTP/1.1"));
    }

    #[test]
    fn configured_client_uses_the_caller_owned_http_client() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let registry = format!("http://{address}");
        let (request_tx, request_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            request_tx
                .send(String::from_utf8_lossy(&request[..read]).into_owned())
                .unwrap();
            let body = r#"{
                "name":"p",
                "dist-tags":{"latest":"1.0.0"},
                "versions":{"1.0.0":{"dist":{"tarball":"https://example.test/p.tgz","integrity":"sha512-abc"}}}
            }"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });

        let directory = tempfile::tempdir().unwrap();
        let npmrc = directory.path().join("configured.npmrc");
        fs::write(
            &npmrc,
            format!("registry={registry}\n//{address}/:_authToken=configured-token\n"),
        )
        .unwrap();
        let client_config = NpmConfig::load_paths(None, Some(&npmrc)).unwrap();
        let routing_config = NpmConfig::default()
            .with_registry_override(&registry)
            .unwrap();
        let client = RegistryClient::with_client(routing_config, HttpClient::new(client_config));

        let resolved = client.resolve(&parse_spec("p").unwrap()).unwrap();
        server.join().unwrap();
        let request = request_rx.recv().unwrap();

        assert_eq!(resolved.version, Version::new(1, 0, 0));
        // reqwest/hyper lowercases header field names on the wire; HTTP header
        // names are case-insensitive, so assert against a lowercased capture.
        let request_lc = request.to_ascii_lowercase();
        assert!(request_lc.contains("authorization: bearer configured-token\r\n"));
        assert!(request_lc.contains("accept: application/vnd.npm.install-v1+json\r\n"));
    }

    /// A minimal HTTP/1.1 test server returning 200 + `ETag` for an
    /// unconditional request and `304` for a request carrying `If-None-Match`.
    /// It records every request line over `requests` for assertion.
    fn conditional_server(
        body: &'static str,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let registry = format!("http://{address}");
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorded = requests.clone();
        let server = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0_u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                recorded.lock().unwrap().push(request.clone());
                if request.to_ascii_lowercase().contains("if-none-match:") {
                    write!(
                        stream,
                        "HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                }
            }
        });
        (registry, requests, server)
    }

    #[test]
    fn persistent_cache_revalidates_and_serves_identical_packument() {
        let body = r#"{"name":"p","dist-tags":{"latest":"1.4.0"},"versions":{"1.4.0":{"dist":{"tarball":"https://example.test/p.tgz","integrity":"sha512-abc"}}}}"#;
        let (registry, requests, _server) = conditional_server(body);
        let config = NpmConfig::default()
            .with_registry_override(&registry)
            .unwrap();
        let cache = Arc::new(MetadataCache::open_in_memory().unwrap());

        // First resolve: uncached, so an unconditional GET answered with 200.
        let client1 = RegistryClient::with_client(config.clone(), HttpClient::new(config.clone()))
            .with_metadata_cache(cache.clone(), CacheMode::Default);
        let first = client1.resolve(&parse_spec("p").unwrap()).unwrap();
        assert_eq!(first.version, Version::new(1, 4, 0));

        // Second resolve with a brand-new client sharing only the persistent
        // cache: a conditional GET (`If-None-Match`) answered with 304, which
        // must reuse the stored body byte-for-byte (identical resolution).
        let client2 = RegistryClient::with_client(config.clone(), HttpClient::new(config.clone()))
            .with_metadata_cache(cache.clone(), CacheMode::Default);
        let second = client2.resolve(&parse_spec("p").unwrap()).unwrap();
        assert_eq!(second.version, Version::new(1, 4, 0));
        assert_eq!(second.tarball_url, first.tarball_url);
        assert_eq!(second.integrity, first.integrity);

        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured.len(), 2);
        assert!(!captured[0].to_ascii_lowercase().contains("if-none-match:"));
        assert!(captured[1]
            .to_ascii_lowercase()
            .contains("if-none-match: \"v1\""));
    }

    #[test]
    fn prefer_offline_serves_stale_without_revalidation() {
        let body = r#"{"name":"p","dist-tags":{"latest":"2.0.0"},"versions":{"2.0.0":{"dist":{"tarball":"https://example.test/p.tgz","integrity":"sha512-xyz"}}}}"#;
        let (registry, requests, _server) = conditional_server(body);
        let config = NpmConfig::default()
            .with_registry_override(&registry)
            .unwrap();
        let cache = Arc::new(MetadataCache::open_in_memory().unwrap());

        // Warm the cache with one full fetch.
        RegistryClient::with_client(config.clone(), HttpClient::new(config.clone()))
            .with_metadata_cache(cache.clone(), CacheMode::Default)
            .resolve(&parse_spec("p").unwrap())
            .unwrap();

        // PreferOffline must serve the cached body without any network contact.
        let resolved = RegistryClient::with_client(config.clone(), HttpClient::new(config.clone()))
            .with_metadata_cache(cache, CacheMode::PreferOffline)
            .resolve(&parse_spec("p").unwrap())
            .unwrap();
        assert_eq!(resolved.version, Version::new(2, 0, 0));

        // Exactly one request reached the server (the warm-up fetch).
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn offline_mode_errors_on_a_cache_miss_without_network_contact() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let registry = format!("http://{address}");
        // Drop the listener so nothing is listening; offline mode must never
        // attempt a connection anyway.
        drop(listener);

        let config = NpmConfig::default()
            .with_registry_override(&registry)
            .unwrap();
        let cache = Arc::new(MetadataCache::open_in_memory().unwrap());
        let client = RegistryClient::with_client(config.clone(), HttpClient::new(config.clone()))
            .with_metadata_cache(cache, CacheMode::Offline);

        let error = client.resolve(&parse_spec("absent").unwrap()).unwrap_err();
        assert!(
            matches!(error, RegistryError::OfflineMiss { .. }),
            "expected OfflineMiss, got {error:?}"
        );
    }
}
