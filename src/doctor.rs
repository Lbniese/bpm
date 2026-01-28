//! `bpm doctor`: structured project diagnostics.
//!
//! `doctor` locates the repository/project root, parses `package.json`, and
//! emits structured diagnostics. It performs no network access, dependency
//! resolution, or installation. Output is deterministic: diagnostics are
//! sorted by stable code/severity/message keys, and dependency maps are
//! `BTreeMap`-backed so serialization is stable across runs and locales.
//!
//! This milestone can only inspect manifests; it cannot install, resolve,
//! fetch, or run scripts. Those capabilities and their diagnostics arrive in
//! later milestones. `doctor` reports what it observes and exits nonzero when an
//! [`Severity::Error`] is present.

use std::path::Path;

use serde::Serialize;

use crate::diagnostic::{sort_diagnostics, Diagnostic, Severity};
use crate::manifest::{is_valid_package_name, ManifestError, PackageManifest};
use crate::project::{find_project_root, find_repository_root};

/// Stable diagnostic codes.
mod codes {
    pub const MANIFEST_NOT_FOUND: &str = "MANIFEST_NOT_FOUND";
    pub const MANIFEST_UNREADABLE: &str = "MANIFEST_UNREADABLE";
    pub const MANIFEST_PARSE: &str = "MANIFEST_PARSE";
    pub const MANIFEST_NAME_MISSING: &str = "MANIFEST_NAME_MISSING";
    pub const MANIFEST_NAME_INVALID: &str = "MANIFEST_NAME_INVALID";
    pub const MANIFEST_VERSION_MISSING: &str = "MANIFEST_VERSION_MISSING";
    pub const DECLARED_DEPENDENCIES: &str = "DECLARED_DEPENDENCIES";
    pub const LIFECYCLE_SCRIPTS: &str = "LIFECYCLE_SCRIPTS";
    pub const NATIVE_ADDON: &str = "NATIVE_ADDON";
    pub const WORKSPACES_UNSUPPORTED: &str = "WORKSPACES_UNSUPPORTED";
    pub const OVERRIDES_UNSUPPORTED: &str = "OVERRIDES_UNSUPPORTED";
    pub const ENGINES_NODE: &str = "ENGINES_NODE";
}

/// Summary of the parsed manifest materialized for reporting. Kept minimal:
/// only fields the milestone can describe without resolving or installing.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ManifestSummary {
    pub name: Option<String>,
    pub version: Option<String>,
    pub private: bool,
    pub module_type: Option<String>,
    pub declared_dependencies: usize,
    pub scripts: usize,
    pub bins: usize,
    pub workspaces: usize,
    pub engines_node: Option<String>,
    pub overrides: usize,
}

impl From<&PackageManifest> for ManifestSummary {
    fn from(m: &PackageManifest) -> Self {
        ManifestSummary {
            name: m.name.clone(),
            version: m.version.clone(),
            private: m.private.unwrap_or(false),
            module_type: m.module_type.clone(),
            declared_dependencies: m.dependency_count(),
            scripts: m.scripts.len(),
            bins: m.bin_count(),
            workspaces: m
                .workspaces
                .as_ref()
                .map(|w| w.patterns().len())
                .unwrap_or(0),
            engines_node: m.engines.get("node").cloned(),
            overrides: m.overrides.len(),
        }
    }
}

/// The full `doctor` report.
///
/// Paths are stored as strings rather than `PathBuf` to keep the public
/// serialized shape platform-neutral and stable.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub bpm_version: String,
    pub repository_root: Option<String>,
    pub project_root: Option<String>,
    pub manifest_found: bool,
    pub manifest: ManifestSummary,
    pub diagnostics: Vec<Diagnostic>,
}

impl DoctorReport {
    /// Highest severity present, or `None` when there are no diagnostics.
    pub fn max_severity(&self) -> Option<Severity> {
        self.diagnostics.iter().map(|d| d.severity).max()
    }

    /// `true` when an [`Severity::Error`] diagnostic is present. `bpm doctor`
    /// exits nonzero in that case.
    pub fn has_error(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    /// Render a human-readable, deterministic summary.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("bpm {}\n", self.bpm_version));
        if let Some(root) = &self.repository_root {
            out.push_str(&format!("repository root: {root}\n"));
        }
        if let Some(root) = &self.project_root {
            out.push_str(&format!("project root:    {root}\n"));
        }
        out.push_str("manifest:\n");
        out.push_str(&format!(
            "  name:        {}\n",
            self.manifest.name.as_deref().unwrap_or("(missing)")
        ));
        out.push_str(&format!(
            "  version:     {}\n",
            self.manifest.version.as_deref().unwrap_or("(missing)")
        ));
        out.push_str(&format!("  private:     {}\n", self.manifest.private));
        out.push_str(&format!(
            "  type:         {}\n",
            self.manifest.module_type.as_deref().unwrap_or("(commonjs)")
        ));
        out.push_str(&format!(
            "  dependencies:{} scripts:{} bins:{} workspaces:{} overrides:{}\n",
            self.manifest.declared_dependencies,
            self.manifest.scripts,
            self.manifest.bins,
            self.manifest.workspaces,
            self.manifest.overrides,
        ));
        if let Some(node) = &self.manifest.engines_node {
            out.push_str(&format!("  engines.node: {node}\n"));
        }
        out.push_str("diagnostics:\n");
        if self.diagnostics.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for d in &self.diagnostics {
                out.push_str(&format!(
                    "  [{severity}] {code}: {message}\n",
                    severity = d.severity.as_str(),
                    code = d.code,
                    message = d.message,
                ));
            }
        }
        out
    }

    /// Canonical machine-readable JSON. Deterministic across runs: struct
    /// field order is fixed by [`Serialize`], maps are `BTreeMap`-sorted, and
    /// diagnostics are pre-sorted (see [`run`]).
    pub fn render_json(&self) -> String {
        serde_json::to_string(self).expect("doctor report serializes")
    }
}

/// Run doctor against `start`, producing a deterministic report.
///
/// This function is infallible at the call boundary: missing manifests and
/// parse failures become structured [`Severity::Error`] diagnostics rather than
/// returned errors, so `--json` output stays consistent.
pub fn run(start: &Path) -> DoctorReport {
    let mut report = DoctorReport {
        bpm_version: env!("CARGO_PKG_VERSION").to_string(),
        repository_root: None,
        project_root: None,
        manifest_found: false,
        manifest: ManifestSummary::default(),
        diagnostics: Vec::new(),
    };

    let project_root = match find_project_root(start) {
        Ok(root) => root,
        Err(_) => {
            report.diagnostics.push(err(
                codes::MANIFEST_NOT_FOUND,
                "no package.json found from the current directory upward",
            ));
            sort_diagnostics(&mut report.diagnostics);
            return report;
        }
    };

    report.project_root = Some(project_root.display().to_string());
    report.manifest_found = true;
    report.repository_root = find_repository_root(start)
        .ok()
        .map(|p| p.display().to_string());

    let manifest_path = project_root.join("package.json");
    let manifest = match PackageManifest::from_path(&manifest_path) {
        Ok(m) => m,
        Err(ManifestError::Read { .. }) => {
            // Ruled out: find_project_root guarantees the file exists.
            report.diagnostics.push(err(
                codes::MANIFEST_UNREADABLE,
                "package.json exists but cannot be read",
            ));
            sort_diagnostics(&mut report.diagnostics);
            return report;
        }
        Err(ManifestError::Parse { source, .. }) => {
            report.diagnostics.push(err(
                codes::MANIFEST_PARSE,
                format!("package.json is not valid JSON: {source}"),
            ));
            sort_diagnostics(&mut report.diagnostics);
            return report;
        }
    };

    report.manifest = ManifestSummary::from(&manifest);
    inspect(&manifest, &project_root, &mut report.diagnostics);
    sort_diagnostics(&mut report.diagnostics);
    report
}

/// Populate diagnostics for a successfully parsed manifest.
fn inspect(manifest: &PackageManifest, project_root: &Path, diagnostics: &mut Vec<Diagnostic>) {
    match &manifest.name {
        None => diagnostics.push(warn(
            codes::MANIFEST_NAME_MISSING,
            "package.json has no \"name\" field; common for workspace roots but required for publishable packages",
        ).with_field("name")),
        Some(name) if !is_valid_package_name(name) => diagnostics.push(err(
            codes::MANIFEST_NAME_INVALID,
            format!("package.json \"name\" is not a valid npm package name: {name}"),
        ).with_field("name").with_package(name.clone())),
        Some(_) => {}
    }

    if manifest.version.is_none() {
        diagnostics.push(
            warn(
                codes::MANIFEST_VERSION_MISSING,
                "package.json has no \"version\" field; required for publishable packages",
            )
            .with_field("version"),
        );
    }

    let dep_count = manifest.dependency_count();
    if dep_count > 0 {
        diagnostics.push(info(
            codes::DECLARED_DEPENDENCIES,
            format!(
                "{dep_count} declared dependencies; this BPM build cannot install dependencies yet"
            ),
        ));
    }

    if !manifest.scripts.is_empty() {
        diagnostics.push(
            info(
                codes::LIFECYCLE_SCRIPTS,
                format!(
                    "{} lifecycle scripts declared; not executed by this BPM build",
                    manifest.scripts.len()
                ),
            )
            .with_field("scripts"),
        );
    }

    if looks_like_native_addon(manifest, project_root) {
        diagnostics.push(warn(
            codes::NATIVE_ADDON,
            "native addon dependencies or a binding.gyp detected; compilation is not yet supported",
        ));
    }

    if manifest.workspaces.is_some() {
        diagnostics.push(
            warn(
                codes::WORKSPACES_UNSUPPORTED,
                "\"workspaces\" declared; workspace installation is not yet supported",
            )
            .with_field("workspaces"),
        );
    }

    if !manifest.overrides.is_empty() {
        diagnostics.push(
            warn(
                codes::OVERRIDES_UNSUPPORTED,
                "\"overrides\" declared; overrides are not yet honored",
            )
            .with_field("overrides"),
        );
    }

    if let Some(node) = manifest.engines.get("node") {
        diagnostics.push(
            info(
                codes::ENGINES_NODE,
                format!("engines.node constraint recorded: {node}"),
            )
            .with_field("engines.node"),
        );
    }
}

/// Heuristic native-addon detection without resolving or fetching:
/// - a `binding.gyp` file at the project root, or
/// - a dependency whose name is a known native-build helper.
fn looks_like_native_addon(manifest: &PackageManifest, project_root: &Path) -> bool {
    if project_root.join("binding.gyp").is_file() {
        return true;
    }
    const NATIVE_BUILDERS: [&str; 3] = ["node-gyp", "node-pre-gyp", "prebuild-install"];
    let sections = [&manifest.dependencies, &manifest.dev_dependencies];
    sections
        .into_iter()
        .flatten()
        .any(|(name, _)| NATIVE_BUILDERS.contains(&name.as_str()))
}

fn err(code: &'static str, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(Severity::Error, code, message)
}
fn warn(code: &'static str, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(Severity::Warning, code, message)
}
fn info(code: &'static str, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(Severity::Info, code, message)
}
