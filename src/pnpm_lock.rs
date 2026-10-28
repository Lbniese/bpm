//! pnpm `pnpm-lock.yaml` (v9) import.
//!
//! A pnpm lock records exact resolutions and dependency edges (`packages:`
//! metadata + `snapshots:` edges) but no npm-style physical placement —
//! pnpm materializes a symlinked virtual store instead. The importer
//! recomputes an npm-style hoisted tree with the shared [`crate::hoist`]
//! placer: dependency edges already name exact versions (no semver work),
//! importers seed the root and workspace links become live link entries.
//!
//! pnpm 10 writes multi-document locks (install contexts separated by
//! `---`); the document with the largest `packages` map is imported and
//! the selection is recorded as a diagnostic. Patched entries resolve to
//! their base package plus a [`crate::lockfile::LockSource::Patch`]
//! overlay (the patch file comes from the lock's or the project's
//! `patchedDependencies` table and must live inside the project),
//! `file:` directory dependencies become live link entries like workspace
//! links, and metadata-only entries without a fetchable resolution are
//! refused with diagnostics rather than guessed.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use crate::diagnostic::{sort_diagnostics, Diagnostic, Severity};
use crate::hoist::Placer;
use crate::lockfile::{LockSource, Lockfile, PackageEntry, PackageResolution, RootEntry};
use crate::manifest::PackageManifest;
use crate::npm_lock::ImportReport;

#[derive(Debug, thiserror::Error)]
pub enum PnpmLockError {
    #[error("cannot read pnpm-lock.yaml at {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("malformed pnpm-lock.yaml: {0}")]
    Parse(String),
    #[error("cannot read package.json beside pnpm-lock.yaml: {0}")]
    Manifest(String),
}

// ── minimal YAML subset parser ───────────────────────────────────────────
//
// pnpm's lock is machine-generated with a stable shape: block maps and
// sequences, single-line flow collections, quoted or bare scalars, and
// full-line comments. Anything richer (anchors, block scalars) fails loudly
// rather than misparsing.

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Yaml {
    Str(String),
    Map(Vec<(String, Yaml)>),
    Seq(Vec<Yaml>),
}

impl Yaml {
    pub(crate) fn get(&self, key: &str) -> Option<&Yaml> {
        match self {
            Yaml::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Map entries; an absent or empty value is an empty map.
    pub(crate) fn entries(&self) -> Vec<(&str, &Yaml)> {
        match self {
            Yaml::Map(entries) => entries.iter().map(|(k, v)| (k.as_str(), v)).collect(),
            _ => Vec::new(),
        }
    }

    pub(crate) fn items(&self) -> Vec<&Yaml> {
        match self {
            Yaml::Seq(items) => items.iter().collect(),
            _ => Vec::new(),
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Yaml::Str(value) => Some(value),
            _ => None,
        }
    }

    /// String list from a block or flow sequence.
    pub(crate) fn string_list(&self) -> Vec<String> {
        self.items()
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect()
    }
}

struct LineCursor {
    lines: Vec<(usize, String)>,
    position: usize,
}

impl LineCursor {
    fn peek(&self) -> Option<(usize, &str)> {
        self.lines
            .get(self.position)
            .map(|(indent, text)| (*indent, text.as_str()))
    }
}

fn unquote(token: &str) -> String {
    let trimmed = token.trim();
    if let Some(rest) = trimmed.strip_prefix('\'') {
        if let Some(body) = rest.strip_suffix('\'') {
            return body.replace("''", "'");
        }
    }
    if let Some(rest) = trimmed.strip_prefix('"') {
        if let Some(body) = rest.strip_suffix('"') {
            return body.to_string();
        }
    }
    trimmed.to_string()
}

/// Split `key: value` at the YAML key separator: a colon followed by a
/// space or end of line (URL values and lock ids contain `:` unseparated).
fn split_key(line: &str) -> Option<(String, Option<&str>)> {
    let bytes = line.as_bytes();
    // A leading quote opens the key; start scanning after it so the opener
    // is not mistaken for a closer.
    let starts_quoted = bytes.first().is_some_and(|b| *b == b'\'' || *b == b'"');
    let mut quoted = starts_quoted;
    for at in usize::from(starts_quoted)..bytes.len() {
        if quoted {
            if bytes[at] == b'\'' || bytes[at] == b'"' {
                quoted = false;
            }
            continue;
        }
        if bytes[at] == b'\'' || bytes[at] == b'"' {
            quoted = true;
            continue;
        }
        if bytes[at] == b':' && (at + 1 == bytes.len() || bytes[at + 1] == b' ') {
            let key = unquote(&line[..at]).to_string();
            let value = line[at + 1..].trim();
            return Some((key, (!value.is_empty()).then_some(value)));
        }
    }
    None
}

fn parse_block(lines: &mut LineCursor, indent: usize) -> Result<Yaml, String> {
    match lines.peek() {
        Some((at, text)) if at == indent && (text.starts_with("- ") || text == "-") => {
            let mut items = Vec::new();
            while let Some((at, text)) = lines.peek().map(|(at, text)| (at, text.to_string())) {
                if at != indent || !(text.starts_with("- ") || text == "-") {
                    break;
                }
                let rest = text.strip_prefix("- ").unwrap_or("").trim();
                if rest.is_empty() {
                    items.push(Yaml::Str(String::new()));
                    lines.position += 1;
                    continue;
                }
                if split_key(rest).is_some() {
                    return Err(format!("sequence-of-maps is unsupported: {rest:?}"));
                }
                items.push(parse_inline(rest)?);
                lines.position += 1;
            }
            Ok(Yaml::Seq(items))
        }
        Some((at, _)) if at >= indent => {
            let mut entries: Vec<(String, Yaml)> = Vec::new();
            while let Some((at, text)) = lines.peek().map(|(at, text)| (at, text.to_string())) {
                if at != indent {
                    if at < indent {
                        break;
                    }
                    return Err(format!("unexpected indent: {text:?}"));
                }
                let Some((key, value)) = split_key(&text) else {
                    return Err(format!("expected a mapping entry, found {text:?}"));
                };
                lines.position += 1;
                let node = match value {
                    Some(inline) => parse_inline(inline)?,
                    None => {
                        let child_indent = lines.peek().map(|(at, _)| at);
                        match child_indent {
                            Some(child) if child > indent => parse_block(lines, child)?,
                            // A key with no value and no nested block: null.
                            _ => Yaml::Str(String::new()),
                        }
                    }
                };
                entries.push((key, node));
            }
            Ok(Yaml::Map(entries))
        }
        _ => Ok(Yaml::Str(String::new())),
    }
}

fn parse_inline(value: &str) -> Result<Yaml, String> {
    if value.starts_with('{') || value.starts_with('[') {
        let (node, rest) = parse_flow(value)?;
        if !rest.trim().is_empty() {
            return Err(format!("trailing content after flow collection: {rest:?}"));
        }
        return Ok(node);
    }
    if value.contains('\n') {
        return Err("multi-line scalars are unsupported".to_string());
    }
    Ok(Yaml::Str(unquote(value).to_string()))
}

fn parse_flow(text: &str) -> Result<(Yaml, &str), String> {
    let bytes = text.as_bytes();
    match bytes.first() {
        Some(b'{') => {
            let mut rest = &text[1..];
            let mut entries = Vec::new();
            loop {
                let rest_trimmed = rest.trim_start();
                if let Some(after) = rest_trimmed.strip_prefix('}') {
                    return Ok((Yaml::Map(entries), after));
                }
                let (key, rest_after_key) = split_flow_key(rest_trimmed)?;
                let (value, rest_after_value) = parse_flow(rest_after_key)?;
                entries.push((key, value));
                let rest_trimmed = rest_after_value.trim_start();
                if let Some(after) = rest_trimmed.strip_prefix(',') {
                    rest = after;
                } else {
                    rest = rest_trimmed;
                }
            }
        }
        Some(b'[') => {
            let mut rest = &text[1..];
            let mut items = Vec::new();
            loop {
                let rest_trimmed = rest.trim_start();
                if let Some(after) = rest_trimmed.strip_prefix(']') {
                    return Ok((Yaml::Seq(items), after));
                }
                let (node, rest_after) = parse_flow(rest_trimmed)?;
                items.push(node);
                let rest_trimmed = rest_after.trim_start();
                if let Some(after) = rest_trimmed.strip_prefix(',') {
                    rest = after;
                } else {
                    rest = rest_trimmed;
                }
            }
        }
        _ => {
            let end = text.find(&[',', '}', ']'][..]).unwrap_or(text.len());
            let (token, rest) = text.split_at(end);
            Ok((Yaml::Str(unquote(token).to_string()), rest))
        }
    }
}

fn split_flow_key(text: &str) -> Result<(String, &str), String> {
    let bytes = text.as_bytes();
    let starts_quoted = bytes.first().is_some_and(|b| *b == b'\'' || *b == b'"');
    let mut quoted = starts_quoted;
    for at in usize::from(starts_quoted)..bytes.len() {
        if quoted {
            if bytes[at] == b'\'' || bytes[at] == b'"' {
                quoted = false;
            }
            continue;
        }
        if bytes[at] == b'\'' || bytes[at] == b'"' {
            quoted = true;
            continue;
        }
        if bytes[at] == b':' {
            let key = unquote(&text[..at]).to_string();
            return Ok((key, &text[at + 1..]));
        }
    }
    Err(format!("flow entry is missing a key: {text:?}"))
}

/// Parse every YAML document in `text` (pnpm 10 writes multiple, separated
/// by `---`).
///
/// Explicit key syntax (`? <key>` then `: <value>`) is pre-normalized into
/// ordinary `key: value` lines: pnpm emits it for package ids longer than
/// YAML's key-line threshold (common in locks with deep peer suffixes).
pub(crate) fn parse_documents(text: &str) -> Result<Vec<Yaml>, String> {
    let mut documents = Vec::new();
    let mut current: Vec<(usize, String)> = Vec::new();
    // A `? <key>` line waits for its `: <value>` line at the same indent.
    let mut pending_key: Option<(usize, String)> = None;
    for raw in text.lines() {
        let trimmed_end = raw.trim_end();
        if trimmed_end == "---" || trimmed_end == "..." {
            if !current.is_empty() {
                documents.push(std::mem::take(&mut current));
            }
            continue;
        }
        let trimmed = trimmed_end.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = trimmed_end.len() - trimmed.len();
        if let Some((key_indent, key)) = pending_key.take() {
            if trimmed.starts_with(':') && indent == key_indent {
                // `: <first entry>` opens the value's map: the key line plus
                // the remainder re-indented as that map's first entry. A
                // non-mapping scalar folds into a one-line entry instead.
                let inline = trimmed[1..].trim();
                match split_key(inline) {
                    Some((name, Some(value))) => {
                        current.push((key_indent, format!("{key}:")));
                        current.push((key_indent + 2, format!("{name}: {value}")));
                    }
                    Some((name, None)) => {
                        current.push((key_indent, format!("{key}:")));
                        current.push((key_indent + 2, format!("{name}:")));
                    }
                    // A scalar value (`: 1.3.0`).
                    _ => current.push((key_indent, format!("{key}: {inline}"))),
                }
                continue;
            }
            // No `: <value>` line: the key's value is null.
            current.push((key_indent, format!("{key}:")));
            // Fall through to process the current line normally.
        }
        if let Some(key) = trimmed.strip_prefix("? ") {
            pending_key = Some((indent, unquote(key)));
            continue;
        }
        if trimmed.contains(" #") {
            return Err(format!("inline comments are unsupported: {trimmed:?}"));
        }
        if trimmed.starts_with('&') || trimmed.starts_with('*') || trimmed.starts_with('|') {
            return Err(format!(
                "anchors and block scalars are unsupported: {trimmed:?}"
            ));
        }
        current.push((indent, trimmed.to_string()));
    }
    if let Some((key_indent, key)) = pending_key.take() {
        current.push((key_indent, format!("{key}:")));
    }
    if !current.is_empty() {
        documents.push(current);
    }
    documents
        .into_iter()
        .map(|lines| {
            let mut cursor = LineCursor { lines, position: 0 };
            parse_block(&mut cursor, 0)
        })
        .collect()
}

// ── pnpm v9 importer ─────────────────────────────────────────────────────

/// One merge of a `packages` entry and its `snapshots` edge set.
#[derive(Debug, Default, Clone)]
struct PnpmEntry {
    name: String,
    version: String,
    resolved: String,
    integrity: Option<String>,
    dependencies: BTreeMap<String, String>,
    optional_dependencies: BTreeSet<String>,
    peer_dependencies: BTreeSet<String>,
    os: Vec<String>,
    cpu: Vec<String>,
}

/// Markers pnpm appends to package ids for patched dependencies
/// (`/foo@1.0.0_patch=<hash>`; pnpm 10 also writes `_patch_hash=` and
/// `(patch_hash=` forms). A package named `foo_patch` never matches: the
/// markers continue with `=` or `(`, not an id character.
fn patch_marker_at(id: &str) -> Option<usize> {
    ["_patch=", "_patch_hash=", "(patch_hash="]
        .iter()
        .filter_map(|marker| id.find(marker))
        .min()
}

/// Strip pnpm id decorations: leading `/`, peer-suffix parens, and patch
/// suffixes (patched ids normalize onto their base package so dependency
/// edges compose; the patch overlay is recorded separately).
fn normalize_id(id: &str) -> String {
    let id = match patch_marker_at(id) {
        Some(at) => &id[..at],
        None => id,
    };
    let bare = id.strip_prefix('/').unwrap_or(id);
    match bare.find('(') {
        Some(at) => bare[..at].to_string(),
        None => bare.to_string(),
    }
}

/// Lexically normalize a slash path (resolve `.` and `..` without touching
/// the filesystem). Leading `..` segments resolve away, so a path escaping
/// the root collapses instead of silently surviving.
fn normalize_path(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

/// Split a normalized registry id `name@version` into its parts; scoped
/// names split at the last `@` (versions never contain one on this path).
fn split_registry_id(id: &str) -> Option<(String, String)> {
    let at = id.rfind('@')?;
    if at == 0 {
        return None;
    }
    Some((id[..at].to_string(), id[at + 1..].to_string()))
}

fn derive_registry_url(name: &str, version: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    format!("https://registry.npmjs.org/{name}/-/{base}-{version}.tgz")
}

/// Patch table entries (`name@version` or bare `name` -> patch path) from a
/// YAML node; pnpm writes either a bare path string or a `{path, hash}` map.
fn patches_from_yaml(node: Option<&Yaml>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (key, value) in node.map(Yaml::entries).unwrap_or_default() {
        let path = value
            .as_str()
            .map(str::to_string)
            .or_else(|| value.get("path").and_then(Yaml::as_str).map(str::to_string));
        if let Some(path) = path {
            out.insert(key.to_string(), path);
        }
    }
    out
}

/// The `patchedDependencies` table from the root manifest's package.json
/// (pnpm <= 9 records it there).
fn read_manifest_patches(manifest_path: &Path) -> Result<BTreeMap<String, String>, PnpmLockError> {
    let text = std::fs::read_to_string(manifest_path).map_err(|source| PnpmLockError::Read {
        path: manifest_path.display().to_string(),
        source,
    })?;
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        // A manifest that fails JSON parsing is the manifest loader's error
        // to report; the import itself only needs this one table.
        return Ok(BTreeMap::new());
    };
    match value.get("patchedDependencies") {
        None | Some(serde_json::Value::Null) => Ok(BTreeMap::new()),
        Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
            PnpmLockError::Manifest(format!("malformed patchedDependencies: {error}"))
        }),
    }
}

/// Patch file paths for patched packages, merged from every place pnpm
/// records them (later wins): pnpm 10 keeps the table in
/// `pnpm-workspace.yaml` and mirrors it (with the content hash) in the
/// lock's own top-level `patchedDependencies`, which is authoritative;
/// older pnpm records the table in the root manifest.
fn merged_patch_paths(
    document: &Yaml,
    manifest_path: &Path,
) -> Result<BTreeMap<String, String>, PnpmLockError> {
    let mut paths = std::fs::read_to_string(manifest_path.with_file_name("pnpm-workspace.yaml"))
        .ok()
        .and_then(|text| parse_documents(&text).ok())
        .and_then(|documents| documents.first().cloned())
        .map(|config| patches_from_yaml(config.get("patchedDependencies")))
        .unwrap_or_default();
    paths.extend(read_manifest_patches(manifest_path)?);
    paths.extend(patches_from_yaml(document.get("patchedDependencies")));
    Ok(paths)
}

/// The real package name behind an importer `npm:` alias specifier
/// (`npm:obug@1.0.2` -> `obug`, `npm:@scope/foo` -> `@scope/foo`; a range
/// part is optional).
fn alias_real_name(spec: &str) -> String {
    match spec.rfind('@') {
        Some(at) if at > 0 => spec[..at].to_string(),
        _ => spec.to_string(),
    }
}

/// Clone the real entry's artifact fields into a synthetic entry named for
/// the alias: consumers require the alias NAME while the artifact is the
/// real package. The real entry's dependency edges come along so a package
/// referenced ONLY through the alias still places its own dependencies.
fn push_alias_entry(
    entries: &mut Vec<PnpmEntry>,
    by_id: &mut BTreeMap<String, usize>,
    alias_name: &str,
    real_index: usize,
) -> usize {
    let real = entries[real_index].clone();
    let version = real.version.clone();
    entries.push(PnpmEntry {
        name: alias_name.to_string(),
        version: real.version,
        resolved: real.resolved,
        integrity: real.integrity,
        dependencies: real.dependencies,
        optional_dependencies: real.optional_dependencies,
        os: real.os,
        cpu: real.cpu,
        ..Default::default()
    });
    let alias_index = entries.len() - 1;
    by_id.insert(format!("{alias_name}@{version}"), alias_index);
    alias_index
}

/// The directory target of a pnpm `file:` dependency id: bare
/// (`file:libs/local`) or id-prefixed (`local-lib@file:libs/local`).
fn file_target_of(id: &str) -> Option<&str> {
    match id.split_once("@file:") {
        Some((_, target)) => (!target.is_empty()).then_some(target),
        None => id.strip_prefix("file:").filter(|target| !target.is_empty()),
    }
}

#[derive(Clone, Copy)]
struct Reach {
    prod: bool,
    optional_only: bool,
    /// True when ANY incoming edge was optional (platform-skippable).
    any_optional: bool,
}

/// Import a pnpm v9 lock plus the root `package.json` into a canonical
/// lockfile.
pub fn import_pnpm_v9(
    lock_path: &Path,
    manifest_path: &Path,
) -> Result<ImportReport, PnpmLockError> {
    let text = std::fs::read_to_string(lock_path).map_err(|source| PnpmLockError::Read {
        path: lock_path.display().to_string(),
        source,
    })?;
    let manifest = PackageManifest::from_path(manifest_path)
        .map_err(|error| PnpmLockError::Manifest(error.to_string()))?;
    let mut diagnostics = Vec::new();

    let documents = parse_documents(&text).map_err(PnpmLockError::Parse)?;
    if documents.is_empty() {
        return Err(PnpmLockError::Parse("no lockfile content".to_string()));
    }
    // pnpm 10 writes one document per install context; import the largest
    // graph (the default context) and record the selection.
    let document = documents
        .iter()
        .enumerate()
        .max_by_key(|(index, doc)| {
            (
                doc.get("packages").map(|p| p.entries().len()).unwrap_or(0),
                std::cmp::Reverse(*index),
            )
        })
        .map(|(_, doc)| doc)
        .expect("at least one document");
    if documents.len() > 1 {
        diagnostics.push(Diagnostic::new(
            Severity::Info,
            "PNPM_MULTIPLE_CONTEXTS",
            format!(
                "lockfile contains {} install contexts; importing the largest graph",
                documents.len()
            ),
        ));
    }
    let importers = document
        .get("importers")
        .ok_or_else(|| PnpmLockError::Parse("lockfile has no importers section".to_string()))?;
    let packages = document
        .get("packages")
        .ok_or_else(|| PnpmLockError::Parse("lockfile has no packages section".to_string()))?;
    let snapshots = document.get("snapshots");
    let importer_paths: BTreeSet<String> = importers
        .entries()
        .into_iter()
        .map(|(path, _)| path.to_string())
        .collect();

    // Merge packages + snapshots by normalized id.
    let mut entries: Vec<PnpmEntry> = Vec::new();
    // Workspace links, from importers AND snapshot edges (`vue: link:packages/vue`).
    let mut links: BTreeMap<String, String> = BTreeMap::new();
    let mut by_id: BTreeMap<String, usize> = BTreeMap::new();
    // Identities whose importer version field carried a patch hash — pnpm 10
    // keeps the packages entry under the BASE id, so the importer field is
    // where a patch is visible. A flagged identity without a usable
    // `patchedDependencies` entry is refused rather than installed unpatched.
    let mut patched_importer_flags: BTreeSet<String> = BTreeSet::new();
    // Patch file paths for patched packages, from the lock's own
    // `patchedDependencies` table plus the manifest (and pnpm 10's
    // pnpm-workspace.yaml); see `merged_patch_paths`.
    let patched_dependencies = merged_patch_paths(document, manifest_path)?;
    for (id, metadata) in packages.entries() {
        let normalized = normalize_id(id);
        if by_id.contains_key(&normalized) {
            continue;
        }
        let resolution = metadata.get("resolution");
        let integrity = resolution
            .and_then(|r| r.get("integrity"))
            .and_then(Yaml::as_str)
            .filter(|value| value.starts_with("sha512-"))
            .map(str::to_string);
        let tarball = resolution
            .and_then(|r| r.get("tarball"))
            .and_then(Yaml::as_str)
            .map(str::to_string);
        let (name, version) = match split_registry_id(&normalized) {
            Some(parts) => parts,
            None => (String::new(), normalized.clone()),
        };
        // File-directory entries (`local-lib@file:libs/local`) carry no
        // fetchable resolution: they materialize through their link
        // placement and only contribute dependency edges.
        let is_file_entry = version.starts_with("file:");
        if integrity.is_none() && tarball.is_none() && !is_file_entry {
            continue;
        }
        // File-directory entries materialize through their link placement;
        // they carry no tarball to derive a URL for.
        let resolved = if is_file_entry {
            String::new()
        } else {
            tarball
                .clone()
                .filter(|url| !url.is_empty())
                .unwrap_or_else(|| derive_registry_url(&name, &version))
        };
        let index = entries.len();
        entries.push(PnpmEntry {
            name,
            version,
            resolved,
            integrity,
            os: metadata
                .get("os")
                .map(Yaml::string_list)
                .unwrap_or_default(),
            cpu: metadata
                .get("cpu")
                .map(Yaml::string_list)
                .unwrap_or_default(),
            ..Default::default()
        });
        by_id.insert(normalized, index);
    }
    for (id, edges) in snapshots.map(Yaml::entries).unwrap_or_default() {
        let normalized = normalize_id(id);
        let Some(&index) = by_id.get(&normalized) else {
            continue;
        };
        let entry = &mut entries[index];
        for (dep, target) in edges
            .get("dependencies")
            .map(Yaml::entries)
            .unwrap_or_default()
        {
            if let Some(link) = target.as_str().and_then(|t| t.strip_prefix("link:")) {
                // Snapshot link targets are relative to the DECLARING
                // importer, not the root (`packages/vue` + `../compiler-core`
                // -> `packages/compiler-core`). Resolve against every
                // importer directory; a match that lands on an importer is
                // in-project by construction.
                let resolved = importer_paths
                    .iter()
                    .map(|base| normalize_path(&format!("{base}/{link}")))
                    .find(|candidate| importer_paths.contains(candidate))
                    .unwrap_or_else(|| link.to_string());
                links.entry(dep.to_string()).or_insert(resolved);
                continue;
            }
            // Peer-context variants of the same version can resolve a
            // dependency differently; the FIRST variant (pnpm's primary,
            // file order) wins so one entry carries one consistent edge set.
            if let Some(norm) = target.as_str().map(normalize_id) {
                entry.dependencies.entry(dep.to_string()).or_insert(norm);
            }
        }
        for (dep, target) in edges
            .get("optionalDependencies")
            .map(Yaml::entries)
            .unwrap_or_default()
        {
            if let Some(norm) = target.as_str().map(normalize_id) {
                entry.dependencies.entry(dep.to_string()).or_insert(norm);
                entry.optional_dependencies.insert(dep.to_string());
            }
        }
        for (dep, target) in edges
            .get("peerDependencies")
            .map(Yaml::entries)
            .unwrap_or_default()
        {
            if let Some(norm) = target.as_str().map(normalize_id) {
                // Peers recorded with a resolved target resolve like ordinary
                // deps; a `true`/absent target is a met-peer marker only.
                if by_id.contains_key(&norm) {
                    entry.dependencies.entry(dep.to_string()).or_insert(norm);
                    entry.peer_dependencies.insert(dep.to_string());
                }
            }
        }
    }
    // Attach names to non-registry ids (tarball/git urls) from their
    // referencing edges.
    let mut attached_names: BTreeMap<usize, String> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if !entry.name.is_empty() {
            continue;
        }
        let normalized = entry.version.clone();
        for other in &entries {
            let found = other
                .dependencies
                .iter()
                .find(|(dep, target)| **target == normalized && !dep.is_empty())
                .map(|(dep, _)| dep.clone());
            if let Some(dep) = found {
                attached_names.insert(index, dep);
                break;
            }
        }
    }
    for (index, name) in attached_names {
        entries[index].name = name;
    }
    entries.retain(|entry| {
        // File-directory entries are named by their importing declaration
        // later; only registry-shaped entries need a name already.
        let keep = !entry.name.is_empty() || entry.version.starts_with("file:");
        if !keep {
            diagnostics.push(Diagnostic::new(
                Severity::Warning,
                "PNPM_UNNAMED_ENTRY",
                format!(
                    "package {:?} has no resolvable name; it cannot be imported",
                    entry.version
                ),
            ));
        }
        keep
    });
    // Rebuild the id index after retain compacted indices. Named (registry)
    // entries key as `name@version` — the dependency-edge lookup shape;
    // url-id entries (tarball/git) have no name and key by the id itself.
    let mut by_id: BTreeMap<String, usize> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.name.is_empty() {
            by_id.insert(entry.version.clone(), index);
        } else {
            by_id.insert(format!("{}@{}", entry.name, entry.version), index);
        }
    }

    // Aliases (`"debug": "npm:obug@1.0.2"` — recorded by pnpm as
    // `debug: obug@1.0.2`): consumers require the alias NAME while the
    // artifact is the real package. Expand each alias edge into a synthetic
    // entry placed under the alias name, and rewrite the edge to the real
    // version so resolution checks compose.
    let mut alias_edges: Vec<(usize, String, usize)> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        for (dep, key) in &entry.dependencies {
            if by_id.contains_key(&format!("{dep}@{key}")) {
                continue;
            }
            if let Some(&real) = by_id.get(key) {
                alias_edges.push((index, dep.clone(), real));
            }
        }
    }
    for (index, alias_name, real_index) in alias_edges {
        let real = entries[real_index].clone();
        let version = real.version.clone();
        entries[index]
            .dependencies
            .insert(alias_name.clone(), version);
        let alias_entry = PnpmEntry {
            name: alias_name.clone(),
            version: real.version,
            resolved: real.resolved,
            integrity: real.integrity,
            os: real.os,
            cpu: real.cpu,
            ..Default::default()
        };
        let alias_index = entries.len();
        entries.push(alias_entry);
        by_id.insert(
            format!("{alias_name}@{}", entries[alias_index].version),
            alias_index,
        );
    }

    // Manifest-level `npm:` aliases (`"debug": "npm:obug@1.0.2"`): the
    // importer `version` field carries the REAL package's id and only the
    // specifier names it. Expand each into a synthetic entry named for the
    // alias, so the importer loop's ordinary lookup finds the artifact under
    // the declared name (npm places alias installs at `node_modules/<name>`).
    for (_, importer) in importers.entries() {
        for group in ["dependencies", "devDependencies", "optionalDependencies"] {
            for (name, spec) in importer.get(group).map(Yaml::entries).unwrap_or_default() {
                let Some(fields) = spec.get("version").and_then(Yaml::as_str) else {
                    continue;
                };
                if fields.starts_with("link:")
                    || file_target_of(fields).is_some()
                    || fields.starts_with("https://")
                {
                    continue;
                }
                let normalized = normalize_id(fields);
                if by_id.contains_key(&format!("{name}@{normalized}")) {
                    continue;
                }
                let Some(real_name) = spec
                    .get("specifier")
                    .and_then(Yaml::as_str)
                    .and_then(|s| s.strip_prefix("npm:"))
                    .map(alias_real_name)
                else {
                    continue;
                };
                let real = by_id
                    .get(&format!("{real_name}@{normalized}"))
                    .or_else(|| by_id.get(&normalized))
                    .copied();
                if let Some(real_index) = real {
                    let alias_index = push_alias_entry(&mut entries, &mut by_id, name, real_index);
                    // Importer fields carry the real package's FULL id
                    // (`is-odd@3.0.1`), so the importer loop's first lookup
                    // probe is `alias@<full id>`; index the synthetic under
                    // that shape too (the canonical `alias@version` key from
                    // `push_alias_entry` stays for BFS identity lookups).
                    by_id.insert(format!("{name}@{normalized}"), alias_index);
                }
            }
        }
    }

    // Dependency keys are bare versions (`10.0.0`) or full ids (tarball
    // URLs); registry keys compose with the dependency name.
    let lookup = |name: &str, key: &str| -> Option<usize> {
        by_id
            .get(&format!("{name}@{key}"))
            .or_else(|| by_id.get(key))
            .copied()
    };
    // Root declarations + workspace links across importers. Links are
    // collected first: each workspace importer's dependency context is its
    // own link placement (npm semantics — workspace deps hoist to root, and
    // conflicts nest under the workspace package), not the root itself.
    let mut root_dependencies: BTreeMap<String, String> = BTreeMap::new();
    let mut root_dev: BTreeSet<String> = BTreeSet::new();
    let mut root_optional: BTreeSet<String> = BTreeSet::new();

    let mut seeds: Vec<(usize, String, Reach)> = Vec::new();
    for (path, importer) in importers.entries() {
        for group in ["dependencies", "devDependencies", "optionalDependencies"] {
            for (name, spec) in importer.get(group).map(Yaml::entries).unwrap_or_default() {
                // Workspace and file-directory links both resolve relative to
                // the declaring importer and materialize as live links. File
                // ids are bare (`file:libs/local`) or id-prefixed
                // (`local-lib@file:libs/local`).
                let target = spec
                    .get("version")
                    .and_then(Yaml::as_str)
                    .and_then(|v| v.strip_prefix("link:").or_else(|| file_target_of(v)));
                if let Some(target) = target {
                    let resolved = normalize_path(&format!("{path}/{target}"));
                    let resolved = if importer_paths.contains(&resolved) {
                        resolved
                    } else {
                        target.to_string()
                    };
                    links.entry(name.to_string()).or_insert(resolved);
                }
            }
        }
    }
    let importer_context = |path: &str| -> String {
        if path == "." {
            return String::new();
        }
        links
            .iter()
            .find(|(_, target)| target.as_str() == path)
            .map(|(name, _)| format!("node_modules/{name}"))
            .unwrap_or_default()
    };
    for (importer_path, importer) in importers.entries() {
        // npm semantics install only linked workspaces: an importer with no
        // incoming link (pnpm playgrounds, private sandboxes) is not part of
        // the package graph an npm-shaped tree materializes, and seeding it
        // from the root context would fabricate phantom conflicts.
        let context = importer_context(importer_path);
        if importer_path != "." && context.is_empty() {
            diagnostics.push(Diagnostic::new(
                Severity::Info,
                "PNPM_UNLINKED_IMPORTER",
                format!("importer {importer_path:?} is not linked by any workspace; its dependencies are not part of the installed graph"),
            ));
            continue;
        }
        for (group, kind) in [
            (
                "dependencies",
                Reach {
                    prod: true,
                    optional_only: false,
                    any_optional: false,
                },
            ),
            (
                "devDependencies",
                Reach {
                    prod: false,
                    optional_only: false,
                    any_optional: false,
                },
            ),
            (
                "optionalDependencies",
                Reach {
                    prod: true,
                    optional_only: true,
                    any_optional: true,
                },
            ),
        ] {
            for (name, spec) in importer.get(group).map(Yaml::entries).unwrap_or_default() {
                let Some(fields) = spec.get("version").and_then(Yaml::as_str) else {
                    continue;
                };
                if fields.starts_with("link:") {
                    // Links were collected in the pre-pass; a linked root
                    // dependency stays a declared root edge (the drift check
                    // compares the manifest against it).
                    if importer_path == "." && !root_dependencies.contains_key(name) {
                        root_dependencies.insert(
                            name.to_string(),
                            spec.get("specifier")
                                .and_then(Yaml::as_str)
                                .unwrap_or(fields)
                                .to_string(),
                        );
                    }
                    continue;
                }
                if file_target_of(fields).is_some() {
                    // File-directory links were collected in the pre-pass; a
                    // declared root edge keeps the manifest's specifier for
                    // the drift check, and the linked directory's own
                    // dependencies resolve from the link's context. The
                    // placement name is the DECLARED key (npm semantics), not
                    // the linked package's own name.
                    if importer_path == "." && !root_dependencies.contains_key(name) {
                        root_dependencies.insert(
                            name.to_string(),
                            spec.get("specifier")
                                .and_then(Yaml::as_str)
                                .unwrap_or(fields)
                                .to_string(),
                        );
                    }
                    if let Some(&index) = by_id.get(fields) {
                        entries[index].name = name.to_string();
                        if !seeds.iter().any(|(seed, _, _)| *seed == index) {
                            seeds.push((
                                index,
                                context.clone(),
                                Reach {
                                    prod: kind.prod || importer_path != ".",
                                    optional_only: kind.optional_only && importer_path == ".",
                                    any_optional: kind.any_optional || kind.optional_only,
                                },
                            ));
                        }
                    }
                    continue;
                }
                let normalized = normalize_id(fields);
                if importer_path == "." && root_dependencies.contains_key(name) {
                    continue;
                }
                if let Some(index) = lookup(name, &normalized) {
                    if patch_marker_at(fields).is_some() {
                        patched_importer_flags.insert(format!(
                            "{}@{}",
                            entries[index].name, entries[index].version
                        ));
                    }
                    if importer_path == "." {
                        root_dependencies.insert(
                            name.to_string(),
                            spec.get("specifier")
                                .and_then(Yaml::as_str)
                                .unwrap_or(&normalized)
                                .to_string(),
                        );
                        match kind {
                            Reach { prod: false, .. } => {
                                root_dev.insert(name.to_string());
                            }
                            Reach {
                                optional_only: true,
                                ..
                            } => {
                                root_optional.insert(name.to_string());
                            }
                            _ => {}
                        }
                    }
                    // Workspace importer deps hoist toward the root but nest
                    // under the workspace package on conflict; the seed's
                    // dependent context encodes that placement chain.
                    if !seeds.iter().any(|(seed, _, _)| *seed == index) {
                        seeds.push((
                            index,
                            context.clone(),
                            Reach {
                                prod: kind.prod || importer_path != ".",
                                optional_only: kind.optional_only && importer_path == ".",
                                any_optional: kind.any_optional || kind.optional_only,
                            },
                        ));
                    }
                } else if importer_path == "." && !fields.starts_with("https://") {
                    diagnostics.push(Diagnostic::new(
                        Severity::Warning,
                        "PNPM_MISSING_RESOLUTION",
                        format!("no packages entry satisfies {name} {normalized:?}"),
                    ));
                }
            }
        }
    }

    // BFS placement (sorted FIFO seeds; per-dependent dedupe).
    let mut queue: VecDeque<(usize, String, Reach)> = VecDeque::new();
    for (index, dependent, kind) in &seeds {
        queue.push_back((*index, dependent.clone(), *kind));
    }
    let mut reach: BTreeMap<String, Reach> = BTreeMap::new();
    let mut placer = Placer::default();
    let mut placed_entries: Vec<(usize, String)> = Vec::new();
    let mut emitted_paths: BTreeSet<String> = BTreeSet::new();
    let mut visited: BTreeSet<(usize, String)> = BTreeSet::new();
    while let Some((index, dependent, kind)) = queue.pop_front() {
        let entry = &entries[index];
        if entry.version.starts_with("file:") {
            // File-directory entries materialize through their link
            // placement, never as their own package placement; only their
            // dependency edges join the graph, resolving from the link's
            // context like npm resolves a symlinked package's deps.
            if visited.insert((index, dependent.clone())) {
                let link_context = if entry.name.is_empty() {
                    String::new()
                } else {
                    format!("node_modules/{}", entry.name)
                };
                for (dep_name, dep_key) in &entry.dependencies {
                    if let Some(dep_index) = lookup(dep_name, dep_key) {
                        let optional = entry.optional_dependencies.contains(dep_name);
                        queue.push_back((
                            dep_index,
                            link_context.clone(),
                            Reach {
                                prod: kind.prod,
                                optional_only: kind.optional_only && optional,
                                any_optional: optional,
                            },
                        ));
                    }
                }
            }
            continue;
        }
        let identity = format!("{}@{}", entry.name, entry.version);
        if !visited.insert((index, dependent.clone())) {
            if let Some(existing) = reach.get_mut(&identity) {
                existing.prod |= kind.prod;
                existing.any_optional |= kind.any_optional;
            }
            continue;
        }
        let entry_kind = reach.entry(identity.clone()).or_insert(kind);
        entry_kind.prod |= kind.prod;
        entry_kind.any_optional |= kind.any_optional;

        let path = placer.place(&entry.name, &entry.version, &dependent);
        if emitted_paths.insert(path.clone()) {
            placed_entries.push((index, path.clone()));
        }
        for (dep_name, dep_key) in &entries[index].dependencies {
            if let Some(dep_index) = lookup(dep_name, dep_key) {
                let optional = entries[index].optional_dependencies.contains(dep_name);
                queue.push_back((
                    dep_index,
                    path.clone(),
                    Reach {
                        prod: kind.prod,
                        optional_only: kind.optional_only && optional,
                        any_optional: optional,
                    },
                ));
            }
        }
    }

    let mut lockfile = Lockfile::new("bpm");
    lockfile.root = RootEntry {
        name: Some(manifest.name.clone().unwrap_or_else(|| "root".to_string())),
        version: manifest.version.clone(),
        dependencies: root_dependencies.clone(),
    };
    lockfile.resolution.root.dev_dependencies = manifest.dev_dependencies.clone();
    lockfile.resolution.root.optional_dependencies = manifest.optional_dependencies.clone();

    let project_root = lock_path.parent().unwrap_or_else(|| Path::new("."));
    for (index, path) in placed_entries {
        let entry = &entries[index];
        let identity = format!("{}@{}", entry.name, entry.version);
        let kind = reach.get(&identity).copied().unwrap_or(Reach {
            prod: true,
            optional_only: false,
            any_optional: false,
        });
        let mut dependencies = BTreeMap::new();
        for (dep, key) in &entry.dependencies {
            dependencies.insert(dep.clone(), key.clone());
        }
        // A patch-flagged identity resolves to its base package plus a patch
        // overlay; when the flag has no usable `patchedDependencies` entry,
        // the placement is refused rather than silently installing unpatched
        // content.
        let overlay = if patched_importer_flags.contains(&identity) {
            match patched_dependencies
                .get(&identity)
                .or_else(|| patched_dependencies.get(&entry.name))
            {
                Some(patch) => {
                    let escapes = patch.starts_with("../")
                        || std::path::Path::new(patch).is_absolute()
                        || patch.split('/').any(|segment| segment == "..");
                    if escapes || !project_root.join(patch).is_file() {
                        diagnostics.push(Diagnostic::new(
                            Severity::Warning,
                            "PNPM_PATCH_UNSUPPORTED",
                            format!(
                                "patched package {identity:?} declares patch file {patch:?}, which is missing or outside the project; it cannot be imported"
                            ),
                        ));
                        continue;
                    }
                    Some(patch.clone())
                }
                None => {
                    diagnostics.push(Diagnostic::new(
                        Severity::Warning,
                        "PNPM_PATCH_UNSUPPORTED",
                        format!(
                            "patched package {identity:?} has no patchedDependencies entry; declare it with a patch file inside the project"
                        ),
                    ));
                    continue;
                }
            }
        } else {
            None
        };
        lockfile.packages.push(PackageEntry {
            path: path.clone(),
            name: entry.name.clone(),
            version: entry.version.clone(),
            resolved: entry.resolved.clone(),
            workspace_target: None,
            integrity: entry.integrity.clone(),
            link: false,
            dev: !kind.prod,
            optional: kind.any_optional,
            os: entry.os.clone(),
            cpu: entry.cpu.clone(),
            bin: BTreeMap::new(),
            dependencies,
        });
        if let Some(patch) = overlay {
            lockfile.resolution.packages.insert(
                path,
                PackageResolution {
                    source: LockSource::Patch {
                        source: Box::new(LockSource::Registry {
                            registry: "https://registry.npmjs.org".to_string(),
                        }),
                        patch,
                    },
                    dev_optional: !kind.prod,
                    peer: false,
                    libc: Vec::new(),
                    dependencies: BTreeMap::new(),
                    optional_dependencies: BTreeMap::new(),
                    peer_dependencies: BTreeMap::new(),
                    optional_peers: BTreeSet::new(),
                    peer_context: BTreeMap::new(),
                    workspace_target: None,
                    has_install_script: false,
                },
            );
        }
    }
    for (name, target) in &links {
        // An npm-shaped tree can only materialize in-repo workspace links;
        // pnpm monorepos sometimes link across sibling checkouts
        // (`../plugin-foo`), which the hoisted model cannot represent.
        // Refuse loudly instead of installing a tree missing the link.
        let source = project_root.join(target);
        let escapes = target.starts_with("../")
            || std::path::Path::new(target).is_absolute()
            || target.split('/').any(|segment| segment == "..");
        if escapes || !source.is_dir() {
            diagnostics.push(Diagnostic::new(
                Severity::Warning,
                "PNPM_LINK_OUTSIDE_PROJECT",
                format!(
                    "workspace link {name:?} -> {target:?} is outside the project or missing; convert the layout so workspaces live inside one project root"
                ),
            ));
            continue;
        }
        lockfile.packages.push(PackageEntry {
            path: format!("node_modules/{name}"),
            name: name.clone(),
            version: String::new(),
            resolved: target.clone(),
            workspace_target: Some(target.clone()),
            integrity: None,
            link: true,
            dev: false,
            optional: false,
            os: Vec::new(),
            cpu: Vec::new(),
            bin: BTreeMap::new(),
            dependencies: BTreeMap::new(),
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

    const SAMPLE: &str = "---\nlockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    dependencies:\n      left-pad:\n        specifier: ^1.3.0\n        version: 1.3.0\n    devDependencies:\n      devtool:\n        specifier: ^2.0.0\n        version: 2.5.0\n  packages/cli:\n    dependencies:\n      shared:\n        specifier: workspace:^1.0.0\n        version: link:../shared\n\n  packages/shared: {}\n\npackages:\n\n  devtool@2.5.0:\n    resolution: {integrity: sha512-DDDD}\n    os: [darwin]\n\n  left-pad@1.3.0:\n    resolution: {integrity: sha512-AAAA}\n\n  shared@1.0.0:\n    resolution: {integrity: sha512-SSSS}\n\n  '@scope/util@1.0.0(util@2.0.0)':\n    resolution: {integrity: sha512-UUUU}\n\nsnapshots:\n\n  devtool@2.5.0:\n    dependencies:\n      left-pad: 1.3.0\n\n  left-pad@1.3.0:\n    dependencies:\n      '@scope/util': 1.0.0(util@2.0.0)\n\n  shared@1.0.0:\n    dependencies:\n      left-pad: 1.3.0\n\n  '@scope/util@1.0.0(util@2.0.0)':\n    dependencies:\n      left-pad: 1.3.0\n";

    #[test]
    fn parses_block_flow_and_quotes() {
        let documents = parse_documents(SAMPLE).unwrap();
        assert_eq!(documents.len(), 1);
        let root = &documents[0];
        assert_eq!(
            root.get("lockfileVersion").and_then(Yaml::as_str),
            Some("9.0")
        );
        let version = root
            .get("importers")
            .and_then(|i| i.get("."))
            .and_then(|dot| dot.get("dependencies"))
            .and_then(|d| d.get("left-pad"))
            .and_then(|lp| lp.get("version"))
            .and_then(Yaml::as_str);
        assert_eq!(version, Some("1.3.0"));
        let os = root
            .get("packages")
            .and_then(|p| p.get("devtool@2.5.0"))
            .and_then(|d| d.get("os"))
            .map(Yaml::string_list)
            .unwrap_or_default();
        assert_eq!(os, vec!["darwin".to_string()]);
    }

    #[test]
    fn parses_explicit_key_syntax() {
        // pnpm emits `? <long id>` / `: <value>` when a package id exceeds
        // YAML's plain key-line threshold.
        let documents = parse_documents(
            "packages:\n\n  short@1.0.0:\n    resolution: {integrity: sha512-S}\n\n  ? 'long@1.0.0(react@19.0.0)(typescript@5.9.3)'\n  : resolution: {integrity: sha512-L}\n    engines: {node: '>=18'}\n\nsnapshots:\n\n  long@1.0.0(react@19.0.0)(typescript@5.9.3):\n    dependencies:\n      short: 1.0.0\n",
        )
        .unwrap();
        let root = &documents[0];
        let long = root
            .get("packages")
            .and_then(|packages| packages.get("long@1.0.0(react@19.0.0)(typescript@5.9.3)"))
            .expect("explicit key parsed as a packages entry");
        assert_eq!(
            long.get("resolution")
                .and_then(|resolution| resolution.get("integrity"))
                .and_then(Yaml::as_str),
            Some("sha512-L")
        );
        let engines = long
            .get("engines")
            .map(Yaml::string_list)
            .unwrap_or_default();
        assert!(engines.is_empty(), "engines is a map, not a list");
        assert_eq!(
            long.get("engines")
                .and_then(|engines| engines.get("node"))
                .and_then(Yaml::as_str),
            Some(">=18")
        );
        let edge = root
            .get("snapshots")
            .and_then(|snapshots| snapshots.get("long@1.0.0(react@19.0.0)(typescript@5.9.3)"))
            .and_then(|snapshot| snapshot.get("dependencies"))
            .and_then(|dependencies| dependencies.get("short"))
            .and_then(Yaml::as_str);
        assert_eq!(edge, Some("1.0.0"));
    }

    #[test]
    fn normalizes_ids() {
        assert_eq!(
            normalize_id("/@scope/util@1.0.0(util@2.0.0)"),
            "@scope/util@1.0.0".to_string()
        );
        assert_eq!(normalize_id("left-pad@1.3.0"), "left-pad@1.3.0".to_string());
        // Patch suffixes normalize onto the base package; a package genuinely
        // named with a `_patch` prefix never matches (the marker continues
        // with `=` or `(`).
        assert_eq!(normalize_id("foo@1.0.0_patch=abc"), "foo@1.0.0".to_string());
        assert_eq!(
            normalize_id("foo@1.0.0_patch_hash=abc"),
            "foo@1.0.0".to_string()
        );
        assert_eq!(normalize_id("foo_patch@1.0.0"), "foo_patch@1.0.0");
        assert_eq!(
            patch_marker_at("foo_patch@1.0.0"),
            None,
            "a `_patch` name prefix is not a patch marker"
        );
    }

    #[test]
    fn splits_registry_ids() {
        assert_eq!(
            split_registry_id("@scope/util@1.0.0"),
            Some(("@scope/util".to_string(), "1.0.0".to_string()))
        );
        assert_eq!(split_registry_id("left-pad@1.3.0").unwrap().0, "left-pad");
        assert_eq!(split_registry_id("https://x/y.tgz"), None);
    }

    #[test]
    fn imports_a_small_graph_with_links() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("packages/shared")).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"app","version":"1.0.0","dependencies":{"left-pad":"^1.3.0"},
                "devDependencies":{"devtool":"^2.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("packages/shared/package.json"),
            r#"{"name":"shared","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(root.join("pnpm-lock.yaml"), SAMPLE).unwrap();
        let report =
            import_pnpm_v9(&root.join("pnpm-lock.yaml"), &root.join("package.json")).unwrap();
        let lock = &report.lockfile;
        let find = |name: &str| {
            lock.packages
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("missing {name}"))
        };
        assert_eq!(find("left-pad").path, "node_modules/left-pad");
        // Reached only through the root's devDependencies.
        assert!(find("devtool").dev);
        assert_eq!(find("devtool").os, vec!["darwin".to_string()]);
        // Peer-suffixed ids normalize onto the plain entry.
        assert_eq!(
            lock.packages
                .iter()
                .filter(|p| p.name == "@scope/util")
                .count(),
            1
        );
        // Workspace link becomes a link entry with its importer-relative
        // target normalized to the project-relative importer path.
        let link = find("shared");
        assert!(link.link);
        assert_eq!(link.workspace_target.as_deref(), Some("packages/shared"));
        assert_eq!(find("shared").path, "node_modules/shared");
        // Root declarations carry the importers' specifiers.
        assert_eq!(
            lock.root.dependencies.get("left-pad").map(String::as_str),
            Some("^1.3.0")
        );
    }

    fn write_manifest(dir: &Path, json: &str) {
        std::fs::write(dir.join("package.json"), json).unwrap();
    }

    #[test]
    fn imports_root_npm_alias_under_the_declared_name() {
        // Real pnpm shape: the importer version field carries the real
        // package's FULL id (`is-odd@3.0.1`), only the specifier names the
        // alias.
        let lock = "---\nlockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    dependencies:\n      debug:\n        specifier: npm:obug@1.0.2\n        version: obug@1.0.2\n\npackages:\n\n  obug@1.0.2:\n    resolution: {integrity: sha512-OOOO}\n    dependencies:\n      tiny: 1.0.0\n\n  tiny@1.0.0:\n    resolution: {integrity: sha512-TTTT}\n\nsnapshots:\n\n  obug@1.0.2:\n    dependencies:\n      tiny: 1.0.0\n";
        let dir = tempfile::tempdir().unwrap();
        write_manifest(
            dir.path(),
            r#"{"name":"app","dependencies":{"debug":"npm:obug@1.0.2"}}"#,
        );
        std::fs::write(dir.path().join("pnpm-lock.yaml"), lock).unwrap();
        let report = import_pnpm_v9(
            &dir.path().join("pnpm-lock.yaml"),
            &dir.path().join("package.json"),
        )
        .unwrap();
        assert!(
            !report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "PNPM_MISSING_RESOLUTION"),
            "alias must resolve through the real package: {:?}",
            report.diagnostics
        );
        let alias = report
            .lockfile
            .packages
            .iter()
            .find(|package| package.name == "debug")
            .expect("alias placed under the declared name");
        assert_eq!(alias.path, "node_modules/debug");
        assert_eq!(alias.version, "1.0.2");
        assert_eq!(
            alias.resolved,
            "https://registry.npmjs.org/obug/-/obug-1.0.2.tgz"
        );
        // The alias placement carries the real package's dependency edges.
        let tiny = report
            .lockfile
            .packages
            .iter()
            .find(|package| package.name == "tiny")
            .expect("alias target's dependencies install");
        assert_eq!(tiny.path, "node_modules/tiny");
        assert_eq!(
            report
                .lockfile
                .root
                .dependencies
                .get("debug")
                .map(String::as_str),
            Some("npm:obug@1.0.2"),
            "the root edge keeps the manifest's specifier for the drift check"
        );
    }

    #[test]
    fn imports_file_directory_dependency_as_a_link() {
        // Real pnpm 10 shape: the importer version field and the packages
        // key carry `<real-name>@file:<path>`; the placement name is the
        // importer's declared key.
        let lock = "---\nlockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    dependencies:\n      local:\n        specifier: file:libs/local\n        version: local-lib@file:libs/local\n\npackages:\n\n  local-lib@file:libs/local:\n    resolution: {directory: libs/local, type: directory}\n    dependencies:\n      tiny: 1.0.0\n\n  tiny@1.0.0:\n    resolution: {integrity: sha512-TTTT}\n\nsnapshots:\n\n  local-lib@file:libs/local:\n    dependencies:\n      tiny: 1.0.0\n";
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("libs/local")).unwrap();
        write_manifest(
            dir.path(),
            r#"{"name":"app","dependencies":{"local":"file:libs/local"}}"#,
        );
        std::fs::write(dir.path().join("pnpm-lock.yaml"), lock).unwrap();
        let report = import_pnpm_v9(
            &dir.path().join("pnpm-lock.yaml"),
            &dir.path().join("package.json"),
        )
        .unwrap();
        assert!(
            !report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "PNPM_LINK_OUTSIDE_PROJECT"),
            "in-project file dep must link: {:?}",
            report.diagnostics
        );
        let link = report
            .lockfile
            .packages
            .iter()
            .find(|package| package.name == "local")
            .expect("file dep placed under its declared name");
        assert!(link.link);
        assert_eq!(link.workspace_target.as_deref(), Some("libs/local"));
        assert_eq!(link.path, "node_modules/local");
        // The linked directory's own dependencies join the hoisted graph.
        let tiny = report
            .lockfile
            .packages
            .iter()
            .find(|package| package.name == "tiny")
            .expect("file dep's dependencies install");
        assert_eq!(tiny.path, "node_modules/tiny");
        assert_eq!(
            report
                .lockfile
                .root
                .dependencies
                .get("local")
                .map(String::as_str),
            Some("file:libs/local")
        );
    }

    #[test]
    fn imports_patched_dependency_with_a_patch_overlay() {
        let lock = "---\nlockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    dependencies:\n      left-pad:\n        specifier: 1.3.0\n        version: 1.3.0(patch_hash=abc)\n\npackages:\n\n  left-pad@1.3.0_patch=abc:\n    resolution: {integrity: sha512-LLLL}\n\nsnapshots:\n\n  left-pad@1.3.0_patch=abc: {}\n";
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("patches")).unwrap();
        std::fs::write(
            dir.path().join("patches/left-pad-1.3.0.patch"),
            "--- a/index.js\n+++ b/index.js\n",
        )
        .unwrap();
        write_manifest(
            dir.path(),
            r#"{"name":"app","dependencies":{"left-pad":"1.3.0"},"patchedDependencies":{"left-pad@1.3.0":"patches/left-pad-1.3.0.patch"}}"#,
        );
        std::fs::write(dir.path().join("pnpm-lock.yaml"), lock).unwrap();
        let report = import_pnpm_v9(
            &dir.path().join("pnpm-lock.yaml"),
            &dir.path().join("package.json"),
        )
        .unwrap();
        assert!(
            !report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "PNPM_PATCH_UNSUPPORTED"),
            "declared patch must import: {:?}",
            report.diagnostics
        );
        let package = report
            .lockfile
            .packages
            .iter()
            .find(|package| package.name == "left-pad")
            .expect("base package resolves under the patched id");
        assert_eq!(package.version, "1.3.0");
        assert_eq!(package.integrity.as_deref(), Some("sha512-LLLL"));
        let resolution = report
            .lockfile
            .resolution
            .packages
            .get("node_modules/left-pad")
            .expect("patch overlay recorded");
        match &resolution.source {
            LockSource::Patch { source, patch } => {
                assert_eq!(patch, "patches/left-pad-1.3.0.patch");
                assert!(matches!(**source, LockSource::Registry { .. }));
            }
            other => panic!("expected a patch overlay, got {other:?}"),
        }
    }

    #[test]
    fn imports_pnpm10_patch_table_from_the_lock_itself() {
        // pnpm 10 records patchedDependencies (with the patch path) in the
        // lock and in pnpm-workspace.yaml; the packages entry stays under the
        // BASE id and only importer/snapshot ids carry the hash suffix.
        let lock = "---\nlockfileVersion: '9.0'\n\npatchedDependencies:\n  is-odd@3.0.1:\n    hash: 5d8f\n    path: patches/is-odd@3.0.1.patch\n\nimporters:\n\n  .:\n    dependencies:\n      is-odd:\n        specifier: 3.0.1\n        version: 3.0.1(patch_hash=5d8f)\n\npackages:\n\n  is-odd@3.0.1:\n    resolution: {integrity: sha512-LLLL}\n\nsnapshots:\n\n  is-odd@3.0.1(patch_hash=5d8f):\n    dependencies:\n      is-number: 6.0.0\n\n  is-number@6.0.0: {}\n";
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("patches")).unwrap();
        std::fs::write(
            dir.path().join("patches/is-odd@3.0.1.patch"),
            "--- a/index.js\n+++ b/index.js\n",
        )
        .unwrap();
        write_manifest(
            dir.path(),
            r#"{"name":"app","dependencies":{"is-odd":"3.0.1"}}"#,
        );
        std::fs::write(dir.path().join("pnpm-lock.yaml"), lock).unwrap();
        let report = import_pnpm_v9(
            &dir.path().join("pnpm-lock.yaml"),
            &dir.path().join("package.json"),
        )
        .unwrap();
        assert!(
            !report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "PNPM_PATCH_UNSUPPORTED"),
            "lock-declared patch must import: {:?}",
            report.diagnostics
        );
        let package = report
            .lockfile
            .packages
            .iter()
            .find(|package| package.name == "is-odd")
            .expect("patched base resolves");
        assert_eq!(package.version, "3.0.1");
        let resolution = report
            .lockfile
            .resolution
            .packages
            .get("node_modules/is-odd")
            .expect("patch overlay recorded");
        match &resolution.source {
            LockSource::Patch { patch, .. } => {
                assert_eq!(patch, "patches/is-odd@3.0.1.patch")
            }
            other => panic!("expected a patch overlay, got {other:?}"),
        }
    }

    #[test]
    fn refuses_patched_dependency_without_a_usable_declaration() {
        let lock = "---\nlockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    dependencies:\n      left-pad:\n        specifier: 1.3.0\n        version: 1.3.0(patch_hash=abc)\n\npackages:\n\n  left-pad@1.3.0_patch=abc:\n    resolution: {integrity: sha512-LLLL}\n\nsnapshots:\n\n  left-pad@1.3.0_patch=abc: {}\n";
        let dir = tempfile::tempdir().unwrap();
        // No patchedDependencies in the manifest, no patch file on disk.
        write_manifest(
            dir.path(),
            r#"{"name":"app","dependencies":{"left-pad":"1.3.0"}}"#,
        );
        std::fs::write(dir.path().join("pnpm-lock.yaml"), lock).unwrap();
        let report = import_pnpm_v9(
            &dir.path().join("pnpm-lock.yaml"),
            &dir.path().join("package.json"),
        )
        .unwrap();
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "PNPM_PATCH_UNSUPPORTED"),
            "an undeclared patch must refuse rather than install unpatched: {:?}",
            report.diagnostics
        );
        assert!(
            !report
                .lockfile
                .packages
                .iter()
                .any(|package| package.name == "left-pad"),
            "the patched placement must not be emitted unpatched"
        );
    }
}
