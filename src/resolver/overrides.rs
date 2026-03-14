//! npm-compatible root override normalization and matching.
//!
//! Overrides are only honored from the root manifest, matching npm's safety
//! model. The supported shape covers string overrides, nested ancestry
//! selectors, `.` self-overrides, version-qualified selector keys, and `$name`
//! references to root dependency declarations. Source specs are accepted and
//! handed to the normal dependency source resolver.

use std::collections::BTreeMap;

use semver::{Version, VersionReq};
use serde_json::Value;
use thiserror::Error;

const SUPPORTED_SYNTAX: &str =
    "use root package.json overrides with string specs, nested selectors, `.` self-overrides, version-qualified keys, or $rootDependency references";

/// Manifest location from which an override declaration originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideOrigin {
    Root,
    Workspace,
    InstalledPackage,
}

impl OverrideOrigin {
    fn description(self) -> &'static str {
        match self {
            Self::Root => "root package.json",
            Self::Workspace => "workspace package.json",
            Self::InstalledPackage => "installed package manifest",
        }
    }
}

/// Actionable failure while validating an override declaration.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum OverrideError {
    #[error(
        "unsupported overrides in {origin}; only root package.json overrides are honored; {SUPPORTED_SYNTAX}"
    )]
    UnsupportedOrigin { origin: &'static str },

    #[error("unsupported override key at {location}: {reason}; {SUPPORTED_SYNTAX}")]
    UnsupportedKey {
        location: String,
        reason: &'static str,
    },

    #[error("unsupported override value at {location}: found {found}; {SUPPORTED_SYNTAX}")]
    UnsupportedValue {
        location: String,
        found: &'static str,
    },

    #[error("unsupported override spec at {location}: `{spec}` ({reason}); {SUPPORTED_SYNTAX}")]
    UnsupportedSpec {
        location: String,
        spec: String,
        reason: &'static str,
    },

    #[error(
        "override reference at {location} names undeclared root dependency `${reference}`; declare it in the root dependencies or use an explicit spec"
    )]
    MissingReference { location: String, reference: String },

    #[error(
        "override at {location} changes direct dependency `{package}` from `{declared}` to `{override_spec}`; a direct override must be byte-equal to its declaration or use `${package}`"
    )]
    DirectDependencyMismatch {
        location: String,
        package: String,
        declared: String,
        override_spec: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Selector {
    name: String,
    req: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OverrideRule {
    ancestors: Vec<Selector>,
    package: String,
    /// The target selector's version qualifier, if any. It must match the
    /// request too; otherwise `foo@1` would incorrectly override `foo@2`.
    package_req: Option<String>,
    spec: String,
    key: String,
}

/// Canonical supported overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverrideSet {
    rules: Vec<OverrideRule>,
    normalized: BTreeMap<String, String>,
}

impl OverrideSet {
    /// Validate root overrides and resolve supported `$name` references.
    pub fn from_manifest(
        overrides: &BTreeMap<String, Value>,
        root_declarations: &BTreeMap<String, String>,
        origin: OverrideOrigin,
    ) -> Result<Self, OverrideError> {
        normalize_root_overrides(overrides, root_declarations, origin)
    }

    /// Return the globally effective override for `package`, or the original request.
    pub fn effective_spec<'a>(&'a self, package: &str, requested: &'a str) -> &'a str {
        self.effective_spec_for(package, requested, &[])
    }

    /// Return the effective override for `package` under a visible ancestor chain.
    ///
    /// `ancestors` are ordered from root-most package to immediate parent.
    pub fn effective_spec_for<'a>(
        &'a self,
        package: &str,
        requested: &'a str,
        ancestors: &[(String, Version)],
    ) -> &'a str {
        self.rules
            .iter()
            .filter(|rule| rule.package == package)
            .filter(|rule| ancestors_match(&rule.ancestors, ancestors))
            .filter(|rule| request_matches_selector(rule.package_req.as_deref(), requested))
            .max_by(|a, b| {
                a.ancestors
                    .len()
                    .cmp(&b.ancestors.len())
                    .then_with(|| a.key.cmp(&b.key))
            })
            .map_or(requested, |rule| rule.spec.as_str())
    }

    /// Ordered normalized entries for lockfile serialization and graph identity.
    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.normalized
    }
}

/// Normalize root overrides into deterministic strings.
pub fn normalize_root_overrides(
    overrides: &BTreeMap<String, Value>,
    root_declarations: &BTreeMap<String, String>,
    origin: OverrideOrigin,
) -> Result<OverrideSet, OverrideError> {
    if !overrides.is_empty() && origin != OverrideOrigin::Root {
        return Err(OverrideError::UnsupportedOrigin {
            origin: origin.description(),
        });
    }

    let mut rules = Vec::new();
    let mut normalized = BTreeMap::new();
    for (key, value) in overrides {
        parse_override_entry(
            key,
            value,
            &[],
            root_declarations,
            &mut rules,
            &mut normalized,
        )?;
    }
    rules.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(OverrideSet { rules, normalized })
}

fn parse_override_entry(
    key: &str,
    value: &Value,
    ancestors: &[Selector],
    root_declarations: &BTreeMap<String, String>,
    rules: &mut Vec<OverrideRule>,
    normalized: &mut BTreeMap<String, String>,
) -> Result<(), OverrideError> {
    let location = override_location(key, ancestors);
    let selector = parse_selector(key, &location)?;
    match value {
        Value::String(raw) => add_rule(
            ancestors,
            selector,
            raw,
            &location,
            root_declarations,
            rules,
            normalized,
        ),
        Value::Object(object) => {
            let mut nested_ancestors = ancestors.to_vec();
            nested_ancestors.push(selector.clone());
            for (child, child_value) in object {
                if child == "." {
                    let raw =
                        child_value
                            .as_str()
                            .ok_or_else(|| OverrideError::UnsupportedValue {
                                location: format!("{location}/."),
                                found: json_kind(child_value),
                            })?;
                    add_rule(
                        ancestors,
                        selector.clone(),
                        raw,
                        &format!("{location}/."),
                        root_declarations,
                        rules,
                        normalized,
                    )?;
                } else {
                    parse_override_entry(
                        child,
                        child_value,
                        &nested_ancestors,
                        root_declarations,
                        rules,
                        normalized,
                    )?;
                }
            }
            Ok(())
        }
        _ => Err(OverrideError::UnsupportedValue {
            location,
            found: json_kind(value),
        }),
    }
}

fn add_rule(
    ancestors: &[Selector],
    selector: Selector,
    raw: &str,
    location: &str,
    root_declarations: &BTreeMap<String, String>,
    rules: &mut Vec<OverrideRule>,
    normalized: &mut BTreeMap<String, String>,
) -> Result<(), OverrideError> {
    let effective = if let Some(reference) = raw.strip_prefix('$') {
        if ancestors.is_empty()
            && root_declarations.contains_key(&selector.name)
            && reference != selector.name
        {
            return Err(direct_mismatch(
                location,
                &selector.name,
                &root_declarations[&selector.name],
                raw,
            ));
        }
        root_declarations.get(reference).cloned().ok_or_else(|| {
            OverrideError::MissingReference {
                location: location.to_owned(),
                reference: reference.to_owned(),
            }
        })?
    } else {
        raw.to_owned()
    };

    validate_spec(&effective, location)?;
    if ancestors.is_empty() {
        if let Some(declared) = root_declarations.get(&selector.name) {
            if raw != declared && raw != format!("${}", selector.name) {
                return Err(direct_mismatch(location, &selector.name, declared, raw));
            }
        }
    }

    let key = canonical_rule_key(ancestors, &selector);
    normalized.insert(key.clone(), effective.clone());
    rules.push(OverrideRule {
        ancestors: ancestors.to_vec(),
        package: selector.name,
        package_req: selector.req,
        spec: effective,
        key,
    });
    Ok(())
}

fn direct_mismatch(
    location: &str,
    package: &str,
    declared: &str,
    override_spec: &str,
) -> OverrideError {
    OverrideError::DirectDependencyMismatch {
        location: location.to_owned(),
        package: package.to_owned(),
        declared: declared.to_owned(),
        override_spec: override_spec.to_owned(),
    }
}

fn parse_selector(key: &str, location: &str) -> Result<Selector, OverrideError> {
    if key == "." {
        return Err(OverrideError::UnsupportedKey {
            location: location.to_owned(),
            reason: "`.` is only valid inside an override object",
        });
    }
    let (name, req) = split_selector(key);
    if !is_package_name(name) {
        return Err(OverrideError::UnsupportedKey {
            location: location.to_owned(),
            reason: "expected an npm package name selector",
        });
    }
    if let Some(req) = req {
        VersionReq::parse(req).map_err(|_| OverrideError::UnsupportedKey {
            location: location.to_owned(),
            reason: "version-qualified override key has an invalid semver range",
        })?;
    }
    Ok(Selector {
        name: name.to_owned(),
        req: req.map(str::to_owned),
    })
}

fn split_selector(key: &str) -> (&str, Option<&str>) {
    if let Some(scoped) = key.strip_prefix('@') {
        if let Some((scope_pkg, req)) = scoped.rsplit_once('@') {
            if scope_pkg.contains('/') {
                return (&key[..scope_pkg.len() + 1], Some(req));
            }
        }
        return (key, None);
    }
    key.rsplit_once('@').unwrap_or((key, "")).map_empty_req()
}

trait EmptyReq<'a> {
    fn map_empty_req(self) -> (&'a str, Option<&'a str>);
}

impl<'a> EmptyReq<'a> for (&'a str, &'a str) {
    fn map_empty_req(self) -> (&'a str, Option<&'a str>) {
        if self.1.is_empty() {
            (self.0, None)
        } else {
            (self.0, Some(self.1))
        }
    }
}

fn request_matches_selector(selector_req: Option<&str>, requested: &str) -> bool {
    let Some(selector_req) = selector_req else {
        return true;
    };
    let selector_exact = Version::parse(selector_req).ok();
    let selector = VersionReq::parse(selector_req).ok();
    let Ok(parsed) = crate::registry::parse_spec(&format!("override-probe@{requested}")) else {
        // Source and tag requests do not expose a version before resolution;
        // leave range-qualified rules eligible for the normal resolver.
        return true;
    };
    match parsed.req {
        crate::registry::VersionRequest::Exact(version) => selector_exact.map_or_else(
            || selector.is_some_and(|req| req.matches(&version)),
            |exact| exact == version,
        ),
        crate::registry::VersionRequest::Latest => true,
        crate::registry::VersionRequest::Range(request) => {
            selector.as_ref().is_some_and(|selector| {
                request
                    .requirements()
                    .iter()
                    .any(|request| ranges_intersect(selector, request))
            })
        }
    }
}

/// Semver has no public range-intersection operation. Testing the finite set of
/// comparator boundaries (and their immediate successors) is sufficient for
/// npm's ordinary major/minor/patch ranges and avoids applying a `foo@^1`
/// override to a request constrained to `^2`.
fn ranges_intersect(left: &VersionReq, right: &VersionReq) -> bool {
    let mut candidates = vec![Version::new(0, 0, 0)];
    for comparator in left.comparators.iter().chain(&right.comparators) {
        let minor = comparator.minor.unwrap_or(0);
        let patch = comparator.patch.unwrap_or(0);
        candidates.push(Version::new(comparator.major, minor, patch));
        candidates.push(Version::new(
            comparator.major,
            minor,
            patch.saturating_add(1),
        ));
        candidates.push(Version::new(comparator.major.saturating_add(1), 0, 0));
    }
    candidates
        .iter()
        .any(|candidate| left.matches(candidate) && right.matches(candidate))
}

fn ancestors_match(selectors: &[Selector], ancestors: &[(String, Version)]) -> bool {
    if selectors.len() > ancestors.len() {
        return false;
    }
    let offset = ancestors.len() - selectors.len();
    selectors
        .iter()
        .zip(&ancestors[offset..])
        .all(|(selector, (name, version))| {
            selector.name == *name
                && selector.req.as_deref().is_none_or(|req| {
                    VersionReq::parse(req).is_ok_and(|parsed| parsed.matches(version))
                })
        })
}

fn validate_spec(spec: &str, location: &str) -> Result<(), OverrideError> {
    if spec.trim().is_empty() {
        return Err(OverrideError::UnsupportedSpec {
            location: location.to_owned(),
            spec: spec.to_owned(),
            reason: "the override spec is empty",
        });
    }
    Ok(())
}

fn is_package_name(name: &str) -> bool {
    if let Some(scoped) = name.strip_prefix('@') {
        let Some((scope, package)) = scoped.split_once('/') else {
            return false;
        };
        return !package.contains('/') && segment_ok(scope) && segment_ok(package);
    }
    !name.contains('/') && segment_ok(name)
}

fn segment_ok(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.starts_with(['.', '_'])
        && segment.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

fn canonical_rule_key(ancestors: &[Selector], selector: &Selector) -> String {
    ancestors
        .iter()
        .chain(std::iter::once(selector))
        .map(|selector| match &selector.req {
            Some(req) => format!("{}@{}", selector.name, req),
            None => selector.name.clone(),
        })
        .collect::<Vec<_>>()
        .join(">")
}

fn override_location(package: &str, ancestors: &[Selector]) -> String {
    let mut parts = ancestors
        .iter()
        .map(|selector| selector.name.as_str())
        .chain(std::iter::once(package))
        .map(|part| part.replace('~', "~0").replace('/', "~1"))
        .collect::<Vec<_>>();
    if parts.is_empty() {
        parts.push(String::new());
    }
    format!("/overrides/{}", parts.join("/"))
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn declarations() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("direct".into(), "^1.0.0".into()),
            ("tag-source".into(), "next".into()),
        ])
    }

    #[test]
    fn normalizes_global_specs_and_references_in_key_order() {
        let raw = BTreeMap::from([
            ("zeta".into(), json!("$tag-source")),
            ("alpha".into(), json!(">=2 <3")),
            ("@scope/pkg".into(), json!("beta")),
            ("direct".into(), json!("$direct")),
        ]);

        let set = OverrideSet::from_manifest(&raw, &declarations(), OverrideOrigin::Root)
            .expect("supported subset");

        assert_eq!(
            set.as_map().keys().map(String::as_str).collect::<Vec<_>>(),
            ["@scope/pkg", "alpha", "direct", "zeta"]
        );
        assert_eq!(set.effective_spec("zeta", "1"), "next");
        assert_eq!(set.effective_spec("other", "~4"), "~4");
    }

    #[test]
    fn permits_only_matching_direct_dependency_overrides() {
        let matching = BTreeMap::from([("direct".into(), json!("^1.0.0"))]);
        normalize_root_overrides(&matching, &declarations(), OverrideOrigin::Root)
            .expect("byte-equal direct override");

        let mismatch = BTreeMap::from([("direct".into(), json!("2"))]);
        assert!(matches!(
            normalize_root_overrides(&mismatch, &declarations(), OverrideOrigin::Root),
            Err(OverrideError::DirectDependencyMismatch { .. })
        ));

        let wrong_reference = BTreeMap::from([("direct".into(), json!("$tag-source"))]);
        assert!(matches!(
            normalize_root_overrides(&wrong_reference, &declarations(), OverrideOrigin::Root),
            Err(OverrideError::DirectDependencyMismatch { .. })
        ));
    }

    #[test]
    fn supports_nested_dot_and_version_qualified_forms() {
        let raw = BTreeMap::from([(
            "parent@^1".into(),
            json!({
                ".": "1.2.3",
                "child": "2.0.0",
                "grand": {"leaf@^3": "3.1.0"}
            }),
        )]);
        let set = normalize_root_overrides(&raw, &BTreeMap::new(), OverrideOrigin::Root).unwrap();
        assert_eq!(set.effective_spec("parent", "^1"), "1.2.3");
        assert_eq!(
            set.effective_spec_for(
                "child",
                "^1",
                &[("parent".into(), Version::parse("1.5.0").unwrap())]
            ),
            "2.0.0"
        );
        assert_eq!(
            set.effective_spec_for(
                "leaf",
                "^3",
                &[
                    ("parent".into(), Version::parse("1.5.0").unwrap()),
                    ("grand".into(), Version::parse("1.0.0").unwrap()),
                ]
            ),
            "3.1.0"
        );
    }

    #[test]
    fn version_qualified_rules_do_not_match_a_different_exact_request() {
        let set = normalize_root_overrides(
            &BTreeMap::from([("transitive@1.0.0".into(), json!("9.0.0"))]),
            &BTreeMap::new(),
            OverrideOrigin::Root,
        )
        .unwrap();
        assert_eq!(set.effective_spec("transitive", "1.0.0"), "9.0.0");
        assert_eq!(set.effective_spec("transitive", "2.0.0"), "2.0.0");

        let ranges = normalize_root_overrides(
            &BTreeMap::from([("transitive@^1".into(), json!("9.0.0"))]),
            &BTreeMap::new(),
            OverrideOrigin::Root,
        )
        .unwrap();
        assert_eq!(ranges.effective_spec("transitive", "^2"), "^2");
    }

    #[test]
    fn accepts_source_override_specs() {
        for spec in [
            "npm:fork@1",
            "https://example.test/pkg.tgz",
            "git+ssh://example.test/pkg",
            "file:../pkg",
            "owner/repo",
        ] {
            normalize_root_overrides(
                &BTreeMap::from([("transitive".into(), json!(spec))]),
                &declarations(),
                OverrideOrigin::Root,
            )
            .expect("source specs are valid override targets");
        }
    }

    #[test]
    fn rejects_non_root_override_origins_without_silent_ignoring() {
        let raw = BTreeMap::from([("transitive".into(), json!("2"))]);
        for origin in [OverrideOrigin::Workspace, OverrideOrigin::InstalledPackage] {
            assert!(matches!(
                normalize_root_overrides(&raw, &declarations(), origin),
                Err(OverrideError::UnsupportedOrigin { .. })
            ));
        }
    }
}
