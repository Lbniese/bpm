//! npm-compatible package platform filtering.
//!
//! Operating system, CPU, and libc declarations are evaluated independently
//! using npm's `checkList` rule. This module does not inspect the host: callers
//! provide an explicit npm-named target so resolution remains reproducible.

use std::collections::BTreeSet;

use thiserror::Error;

use super::model::{PlatformConstraints, ResolutionDiagnostic, TargetPlatform};

/// Stable diagnostic code emitted when an optional-only package is skipped.
pub const OPTIONAL_PLATFORM_SKIP_CODE: &str = "OPTIONAL_PLATFORM_SKIPPED";

/// Whether every path reaching a package is optional.
///
/// Resolver traversal must upgrade a package to [`Self::Required`] when the
/// same identity is reached through both optional and required paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageReachability {
    OptionalOnly,
    Required,
}

/// Result of checking a package against the resolution target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformDisposition {
    Compatible,
    SkipOptional(ResolutionDiagnostic),
}

/// A target dimension that failed its npm platform declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlatformDimension {
    Os,
    Cpu,
    Libc,
}

/// Required package cannot run on the selected target.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PlatformError {
    #[error(
        "package {package} does not support target {target}; declared constraints: {declared}; mismatched: {mismatched}"
    )]
    Unsupported {
        package: String,
        target: String,
        declared: String,
        mismatched: String,
    },
}

/// Check one package's platform declarations against an explicit target.
///
/// An incompatible optional-only package becomes a stable diagnostic. The
/// same incompatibility is an error when any required path reaches the package.
pub fn check_package_platform(
    package: &str,
    constraints: &PlatformConstraints,
    target: &TargetPlatform,
    reachability: PackageReachability,
) -> Result<PlatformDisposition, PlatformError> {
    let mismatched = mismatched_dimensions(constraints, target);
    if mismatched.is_empty() {
        return Ok(PlatformDisposition::Compatible);
    }

    let error = PlatformError::Unsupported {
        package: package.to_owned(),
        target: display_target(target),
        declared: display_constraints(constraints),
        mismatched: display_dimensions(&mismatched),
    };

    match reachability {
        PackageReachability::Required => Err(error),
        PackageReachability::OptionalOnly => Ok(PlatformDisposition::SkipOptional(
            ResolutionDiagnostic::new(OPTIONAL_PLATFORM_SKIP_CODE, error.to_string())
                .with_package(package),
        )),
    }
}

fn mismatched_dimensions(
    constraints: &PlatformConstraints,
    target: &TargetPlatform,
) -> BTreeSet<PlatformDimension> {
    let mut mismatched = BTreeSet::new();
    if !matches_list(&target.os, &constraints.os) {
        mismatched.insert(PlatformDimension::Os);
    }
    if !matches_list(&target.cpu, &constraints.cpu) {
        mismatched.insert(PlatformDimension::Cpu);
    }

    if !constraints.libc.is_empty() {
        let libc = (target.os == "linux")
            .then_some(target.libc.as_deref())
            .flatten();
        if libc.is_none_or(|value| !matches_list(value, &constraints.libc)) {
            mismatched.insert(PlatformDimension::Libc);
        }
    }
    mismatched
}

/// npm's `checkList`: a matching negation wins; otherwise a positive must
/// match when positives exist. A declaration containing only `any` matches.
fn matches_list(value: &str, declarations: &BTreeSet<String>) -> bool {
    if declarations.is_empty()
        || (declarations.len() == 1 && declarations.first().is_some_and(|item| item == "any"))
    {
        return true;
    }

    let mut positive_match = false;
    let mut positives = 0;
    for declaration in declarations {
        if let Some(blocked) = declaration.strip_prefix('!') {
            if value == blocked {
                return false;
            }
        } else {
            positives += 1;
            positive_match |= value == declaration;
        }
    }
    positives == 0 || positive_match
}

fn display_list(values: &BTreeSet<String>) -> String {
    format!("[{}]", values.iter().cloned().collect::<Vec<_>>().join(","))
}

fn display_target(target: &TargetPlatform) -> String {
    format!(
        "os={}, cpu={}, libc={}",
        target.os,
        target.cpu,
        target.libc.as_deref().unwrap_or("<unavailable>")
    )
}

fn display_constraints(constraints: &PlatformConstraints) -> String {
    format!(
        "os={}, cpu={}, libc={}",
        display_list(&constraints.os),
        display_list(&constraints.cpu),
        display_list(&constraints.libc)
    )
}

fn display_dimensions(dimensions: &BTreeSet<PlatformDimension>) -> String {
    dimensions
        .iter()
        .map(|dimension| match dimension {
            PlatformDimension::Os => "os",
            PlatformDimension::Cpu => "cpu",
            PlatformDimension::Libc => "libc",
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn target(os: &str, cpu: &str, libc: Option<&str>) -> TargetPlatform {
        TargetPlatform {
            os: os.into(),
            cpu: cpu.into(),
            libc: libc.map(str::to_owned),
        }
    }

    #[test]
    fn check_list_matches_npm_positive_negative_and_any_rules() {
        assert!(matches_list("linux", &set(&[])));
        assert!(matches_list("linux", &set(&["any"])));
        assert!(matches_list("linux", &set(&["linux", "darwin"])));
        assert!(!matches_list("linux", &set(&["darwin"])));
        assert!(matches_list("linux", &set(&["!win32", "!darwin"])));
        assert!(!matches_list("linux", &set(&["!linux", "linux"])));
        assert!(!matches_list("linux", &set(&["any", "darwin"])));
    }

    #[test]
    fn dimensions_are_checked_independently_and_in_stable_order() {
        let constraints = PlatformConstraints {
            os: set(&["darwin"]),
            cpu: set(&["arm64"]),
            libc: set(&["musl"]),
        };
        let error = check_package_platform(
            "native@1.0.0",
            &constraints,
            &target("linux", "x64", Some("glibc")),
            PackageReachability::Required,
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "package native@1.0.0 does not support target os=linux, cpu=x64, libc=glibc; declared constraints: os=[darwin], cpu=[arm64], libc=[musl]; mismatched: os,cpu,libc"
        );
    }

    #[test]
    fn libc_constraint_rejects_unavailable_or_non_linux_libc() {
        let constraints = PlatformConstraints {
            libc: set(&["glibc"]),
            ..PlatformConstraints::default()
        };
        for target in [
            target("linux", "x64", None),
            target("darwin", "x64", Some("glibc")),
        ] {
            assert!(check_package_platform(
                "native@1.0.0",
                &constraints,
                &target,
                PackageReachability::Required,
            )
            .is_err());
        }
    }

    #[test]
    fn optional_only_is_skipped_while_required_reachability_wins() {
        let constraints = PlatformConstraints {
            os: set(&["darwin"]),
            ..PlatformConstraints::default()
        };
        let target = target("linux", "x64", Some("glibc"));

        let optional = check_package_platform(
            "native@1.0.0",
            &constraints,
            &target,
            PackageReachability::OptionalOnly,
        )
        .unwrap();
        let PlatformDisposition::SkipOptional(diagnostic) = optional else {
            panic!("optional-only incompatibility must be skipped");
        };
        assert_eq!(diagnostic.code, OPTIONAL_PLATFORM_SKIP_CODE);
        assert_eq!(diagnostic.package.as_deref(), Some("native@1.0.0"));

        assert!(check_package_platform(
            "native@1.0.0",
            &constraints,
            &target,
            PackageReachability::Required,
        )
        .is_err());
    }
}
