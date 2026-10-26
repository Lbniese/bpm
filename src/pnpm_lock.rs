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
//! the selection is recorded as a diagnostic. Patched entries, `file:`
//! directory dependencies, and metadata-only entries without a fetchable
//! resolution are refused with diagnostics rather than guessed.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use crate::diagnostic::{sort_diagnostics, Diagnostic, Severity};
use crate::hoist::Placer;
use crate::lockfile::{Lockfile, PackageEntry, RootEntry};
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
pub(crate) fn parse_documents(text: &str) -> Result<Vec<Yaml>, String> {
    let mut documents = Vec::new();
    let mut current: Vec<(usize, String)> = Vec::new();
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
        if trimmed.contains(" #") {
            return Err(format!("inline comments are unsupported: {trimmed:?}"));
        }
        if trimmed.starts_with('&') || trimmed.starts_with('*') || trimmed.starts_with('|') {
            return Err(format!(
                "anchors and block scalars are unsupported: {trimmed:?}"
            ));
        }
        let indent = trimmed_end.len() - trimmed.len();
        current.push((indent, trimmed.to_string()));
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

/// Strip pnpm id decorations: leading `/`, peer-suffix parens, and detect
/// patch suffixes (refused by the caller).
fn normalize_id(id: &str) -> Option<String> {
    if id.contains("_patch=") {
        return None;
    }
    let bare = id.strip_prefix('/').unwrap_or(id);
    let base = match bare.find('(') {
        Some(at) => &bare[..at],
        None => bare,
    };
    Some(base.to_string())
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
    for (id, metadata) in packages.entries() {
        let Some(normalized) = normalize_id(id) else {
            diagnostics.push(Diagnostic::new(
                Severity::Warning,
                "PNPM_PATCH_UNSUPPORTED",
                format!("patched package {id:?} cannot be imported yet"),
            ));
            continue;
        };
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
        if integrity.is_none() && tarball.is_none() {
            continue;
        }
        let (name, version) = match split_registry_id(&normalized) {
            Some(parts) => parts,
            None => (String::new(), normalized.clone()),
        };
        let resolved = tarball
            .clone()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| derive_registry_url(&name, &version));
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
        let Some(normalized) = normalize_id(id) else {
            continue;
        };
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
            if let Some(norm) = target.as_str().and_then(normalize_id) {
                entry.dependencies.entry(dep.to_string()).or_insert(norm);
            }
        }
        for (dep, target) in edges
            .get("optionalDependencies")
            .map(Yaml::entries)
            .unwrap_or_default()
        {
            if let Some(norm) = target.as_str().and_then(normalize_id) {
                entry.dependencies.entry(dep.to_string()).or_insert(norm);
                entry.optional_dependencies.insert(dep.to_string());
            }
        }
        for (dep, target) in edges
            .get("peerDependencies")
            .map(Yaml::entries)
            .unwrap_or_default()
        {
            if let Some(norm) = target.as_str().and_then(normalize_id) {
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
        let keep = !entry.name.is_empty();
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
                if let Some(target) = spec
                    .get("version")
                    .and_then(Yaml::as_str)
                    .and_then(|v| v.strip_prefix("link:"))
                {
                    // Link targets are relative to the DECLARING importer.
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
                let Some(normalized) = normalize_id(fields) else {
                    diagnostics.push(Diagnostic::new(
                        Severity::Warning,
                        "PNPM_PATCH_UNSUPPORTED",
                        format!("patched dependency {name:?} cannot be imported yet"),
                    ));
                    continue;
                };
                if importer_path == "." && root_dependencies.contains_key(name) {
                    continue;
                }
                if let Some(index) = lookup(name, &normalized) {
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
        lockfile.packages.push(PackageEntry {
            path,
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
    }
    let project_root = lock_path.parent().unwrap_or_else(|| Path::new("."));
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
    fn normalizes_ids() {
        assert_eq!(
            normalize_id("/@scope/util@1.0.0(util@2.0.0)"),
            Some("@scope/util@1.0.0".to_string())
        );
        assert_eq!(
            normalize_id("left-pad@1.3.0"),
            Some("left-pad@1.3.0".to_string())
        );
        assert_eq!(normalize_id("foo@1.0.0_patch=abc"), None);
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
}
