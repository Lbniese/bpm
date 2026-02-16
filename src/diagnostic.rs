//! Structured diagnostics with deterministic ordering.
//!
//! Diagnostics are produced by checks such as `bpm doctor`. They must render
//! deterministically so that machine-readable output and tests are stable across
//! runs, locales, and hash-map iteration order. `Severity` and `Diagnostic`
//! derive a total `Ord`; callers additionally sort by `code` to keep the
//! emitted order stable and independent of insertion order.

use serde::Serialize;

/// Diagnostic severity.
///
/// Ordering is `Info < Warning < Error`, used for both display grouping and
/// deterministic sort keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Severity {
    /// Informational note; does not affect exit status.
    Info,
    /// Behavior differs from npm or a feature is not yet honored.
    Warning,
    /// Hard problem that blocks correct operation; causes nonzero exit.
    Error,
}

impl Severity {
    /// Human-readable label.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

/// A single structured diagnostic.
///
/// `code` is a stable machine identifier (`"MANIFEST_NOT_FOUND"`). `message` is
/// a human-readable sentence. `field`/`package` further locate the issue in
/// the manifest when applicable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
}

impl Diagnostic {
    /// Create a diagnostic with the given severity and stable code.
    pub fn new(severity: Severity, code: &'static str, message: impl Into<String>) -> Self {
        Diagnostic {
            severity,
            code,
            message: message.into(),
            field: None,
            package: None,
        }
    }

    /// Attach a manifest field path (e.g. `dependencies`, `scripts.build`).
    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    /// Attach a package name scope (e.g. a dependency the diagnostic concerns).
    pub fn with_package(mut self, package: impl Into<String>) -> Self {
        self.package = Some(package.into());
        self
    }
}

/// Sort diagnostics in a stable, locale-independent order.
///
/// Order: `code`, then `severity` (descending so errors surface first within a
/// code), then `message`. The input vector is sorted in place.
pub fn sort_diagnostics(diags: &mut [Diagnostic]) {
    diags.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then(b.severity.cmp(&a.severity))
            .then(a.message.cmp(&b.message))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_ordering_is_total() {
        assert!(Severity::Info < Severity::Warning);
        assert!(Severity::Warning < Severity::Error);
    }

    #[test]
    fn sort_is_deterministic_and_independent_of_insertion_order() {
        // Two permuted inputs containing the same diagnostics.
        let mut a = vec![
            Diagnostic::new(Severity::Info, "Z", "zeta"),
            Diagnostic::new(Severity::Warning, "A", "alpha"),
            Diagnostic::new(Severity::Error, "A", "alpha"),
            Diagnostic::new(Severity::Error, "A", "alpha"),
        ];
        let mut b = vec![
            Diagnostic::new(Severity::Error, "A", "alpha"),
            Diagnostic::new(Severity::Info, "Z", "zeta"),
            Diagnostic::new(Severity::Error, "A", "alpha"),
            Diagnostic::new(Severity::Warning, "A", "alpha"),
        ];

        sort_diagnostics(&mut a);
        sort_diagnostics(&mut b);

        // Identical, permuted inputs must sort to identical output.
        assert_eq!(a, b, "insertion order leaked into sort");

        // Codes are ordered stably: A before Z.
        let codes: Vec<&str> = a.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec!["A", "A", "A", "Z"]);

        // Within code "A", errors precede warnings.
        assert_eq!(a[0].severity, Severity::Error);
        assert_eq!(a[1].severity, Severity::Error);
        assert_eq!(a[2].severity, Severity::Warning);
        assert_eq!(a[3].severity, Severity::Info);
    }
}
