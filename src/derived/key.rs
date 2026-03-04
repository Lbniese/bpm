//! Canonical cache keys for lifecycle-derived package images.
//!
//! The encoding is deliberately independent of JSON, debug formatting, map
//! insertion order, locale, and native integer width. Environment values are
//! hashed but this module never formats or exposes them.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;

const DOMAIN: &[u8] = b"bpm-derived-v1\0";
const LIFECYCLE_PHASES: [&str; 3] = ["preinstall", "install", "postinstall"];

/// A 256-bit BLAKE3 identity for one complete set of lifecycle inputs.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DerivedKey(blake3::Hash);

impl DerivedKey {
    /// Raw digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    /// Lowercase hexadecimal representation suitable for store paths.
    pub fn to_hex(&self) -> String {
        self.0.to_hex().to_string()
    }
}

impl fmt::Debug for DerivedKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("DerivedKey")
            .field(&self.to_hex())
            .finish()
    }
}

impl fmt::Display for DerivedKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

/// Target properties that can affect native lifecycle output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetDescriptor<'a> {
    pub os: &'a str,
    pub architecture: &'a str,
    pub family: &'a str,
    pub abi: &'a str,
}

/// Runtime properties observed from the executable used to run scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeIdentity<'a> {
    /// Stable executable identity, preferably a verified executable digest.
    pub executable: &'a [u8],
    pub version: &'a str,
    /// Node's `process.versions.modules` value.
    pub modules_abi: &'a str,
    /// Node's N-API version, when exposed by the runtime.
    pub napi_version: Option<&'a str>,
}

/// Every input visible to or governing a lifecycle execution.
///
/// `source_artifact` is the full SHA-512 digest and `dependency_graph` is the
/// full BLAKE3 graph digest. The environment must be the complete bounded
/// environment that will be supplied after `Command::env_clear()`.
pub struct DerivedInputs<'a> {
    pub source_artifact: &'a [u8; 64],
    pub dependency_graph: &'a [u8; 32],
    pub target: TargetDescriptor<'a>,
    pub runtime: RuntimeIdentity<'a>,
    pub scripts: &'a BTreeMap<String, String>,
    pub environment: &'a BTreeMap<OsString, OsString>,
    pub runner_version: u32,
    pub policy_version: u32,
}

/// Hash all build-visible inputs using the versioned canonical encoding.
pub fn derived_key(inputs: &DerivedInputs<'_>) -> DerivedKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN);

    write_field(&mut hasher, b"source", inputs.source_artifact);
    write_field(&mut hasher, b"dependency_graph", inputs.dependency_graph);

    write_field(&mut hasher, b"target.os", inputs.target.os.as_bytes());
    write_field(
        &mut hasher,
        b"target.architecture",
        inputs.target.architecture.as_bytes(),
    );
    write_field(
        &mut hasher,
        b"target.family",
        inputs.target.family.as_bytes(),
    );
    write_field(&mut hasher, b"target.abi", inputs.target.abi.as_bytes());

    write_field(
        &mut hasher,
        b"runtime.executable",
        inputs.runtime.executable,
    );
    write_field(
        &mut hasher,
        b"runtime.version",
        inputs.runtime.version.as_bytes(),
    );
    write_field(
        &mut hasher,
        b"runtime.modules_abi",
        inputs.runtime.modules_abi.as_bytes(),
    );
    write_optional_field(
        &mut hasher,
        b"runtime.napi_version",
        inputs.runtime.napi_version.map(str::as_bytes),
    );

    for phase in LIFECYCLE_PHASES {
        write_field(&mut hasher, b"script.phase", phase.as_bytes());
        write_optional_field(
            &mut hasher,
            b"script.command",
            inputs.scripts.get(phase).map(String::as_bytes),
        );
    }

    let mut environment: Vec<(&[u8], &[u8])> = inputs
        .environment
        .iter()
        .map(|(name, value)| (os_bytes(name), os_bytes(value)))
        .collect();
    environment.sort_unstable_by(|left, right| left.0.cmp(right.0).then(left.1.cmp(right.1)));
    write_field(
        &mut hasher,
        b"environment.count",
        &(environment.len() as u64).to_le_bytes(),
    );
    for (name, value) in environment {
        write_field(&mut hasher, b"environment.name", name);
        write_field(&mut hasher, b"environment.value", value);
    }

    write_field(
        &mut hasher,
        b"runner_version",
        &inputs.runner_version.to_le_bytes(),
    );
    write_field(
        &mut hasher,
        b"policy_version",
        &inputs.policy_version.to_le_bytes(),
    );

    DerivedKey(hasher.finalize())
}

fn write_optional_field(hasher: &mut blake3::Hasher, tag: &[u8], value: Option<&[u8]>) {
    match value {
        Some(value) => {
            write_field(hasher, b"presence", &[1]);
            write_field(hasher, tag, value);
        }
        None => {
            write_field(hasher, b"presence", &[0]);
            write_field(hasher, tag, &[]);
        }
    }
}

fn write_field(hasher: &mut blake3::Hasher, tag: &[u8], value: &[u8]) {
    let tag_len = u32::try_from(tag.len()).expect("derived-key field tag exceeds u32");
    let value_len = u64::try_from(value.len()).expect("derived-key field value exceeds u64");
    hasher.update(&tag_len.to_le_bytes());
    hasher.update(tag);
    hasher.update(&value_len.to_le_bytes());
    hasher.update(value);
}

fn os_bytes(value: &OsStr) -> &[u8] {
    value.as_encoded_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: [u8; 64] = [1; 64];
    const GRAPH: [u8; 32] = [2; 32];

    #[allow(clippy::too_many_arguments)]
    fn key_with(
        source: &[u8; 64],
        graph: &[u8; 32],
        target: TargetDescriptor<'_>,
        runtime: RuntimeIdentity<'_>,
        scripts: &BTreeMap<String, String>,
        environment: &BTreeMap<OsString, OsString>,
        runner_version: u32,
        policy_version: u32,
    ) -> DerivedKey {
        derived_key(&DerivedInputs {
            source_artifact: source,
            dependency_graph: graph,
            target,
            runtime,
            scripts,
            environment,
            runner_version,
            policy_version,
        })
    }

    fn target() -> TargetDescriptor<'static> {
        TargetDescriptor {
            os: "linux",
            architecture: "x86_64",
            family: "unix",
            abi: "gnu",
        }
    }

    fn runtime() -> RuntimeIdentity<'static> {
        RuntimeIdentity {
            executable: b"node-executable-digest",
            version: "22.17.0",
            modules_abi: "127",
            napi_version: Some("10"),
        }
    }

    #[test]
    fn map_insertion_order_does_not_change_key() {
        let scripts = BTreeMap::from([
            ("postinstall".into(), "node post.js".into()),
            ("install".into(), "node build.js".into()),
        ]);
        let environment_a = BTreeMap::from([
            (OsString::from("Z"), OsString::from("last")),
            (OsString::from("A"), OsString::from("first")),
        ]);
        let environment_b = BTreeMap::from([
            (OsString::from("A"), OsString::from("first")),
            (OsString::from("Z"), OsString::from("last")),
        ]);

        assert_eq!(
            key_with(
                &SOURCE,
                &GRAPH,
                target(),
                runtime(),
                &scripts,
                &environment_a,
                1,
                1
            ),
            key_with(
                &SOURCE,
                &GRAPH,
                target(),
                runtime(),
                &scripts,
                &environment_b,
                1,
                1
            )
        );
    }

    #[test]
    fn every_input_domain_invalidates_the_key() {
        let scripts = BTreeMap::from([("install".into(), "node build.js".into())]);
        let environment = BTreeMap::from([(
            OsString::from("SECRET_TOKEN"),
            OsString::from("first-value"),
        )]);
        let baseline = key_with(
            &SOURCE,
            &GRAPH,
            target(),
            runtime(),
            &scripts,
            &environment,
            1,
            1,
        );

        let changed_source = [3; 64];
        let changed_graph = [4; 32];
        assert_ne!(
            baseline,
            key_with(
                &changed_source,
                &GRAPH,
                target(),
                runtime(),
                &scripts,
                &environment,
                1,
                1
            )
        );
        assert_ne!(
            baseline,
            key_with(
                &SOURCE,
                &changed_graph,
                target(),
                runtime(),
                &scripts,
                &environment,
                1,
                1
            )
        );

        for changed_target in [
            TargetDescriptor {
                os: "darwin",
                ..target()
            },
            TargetDescriptor {
                architecture: "aarch64",
                ..target()
            },
            TargetDescriptor {
                family: "windows",
                ..target()
            },
            TargetDescriptor {
                abi: "musl",
                ..target()
            },
        ] {
            assert_ne!(
                baseline,
                key_with(
                    &SOURCE,
                    &GRAPH,
                    changed_target,
                    runtime(),
                    &scripts,
                    &environment,
                    1,
                    1
                )
            );
        }

        for changed_runtime in [
            RuntimeIdentity {
                executable: b"other-executable",
                ..runtime()
            },
            RuntimeIdentity {
                version: "23.0.0",
                ..runtime()
            },
            RuntimeIdentity {
                modules_abi: "131",
                ..runtime()
            },
            RuntimeIdentity {
                napi_version: None,
                ..runtime()
            },
        ] {
            assert_ne!(
                baseline,
                key_with(
                    &SOURCE,
                    &GRAPH,
                    target(),
                    changed_runtime,
                    &scripts,
                    &environment,
                    1,
                    1
                )
            );
        }

        let changed_scripts = BTreeMap::from([("install".into(), "node other.js".into())]);
        let empty_script = BTreeMap::from([("install".into(), String::new())]);
        let missing_script = BTreeMap::new();
        assert_ne!(
            baseline,
            key_with(
                &SOURCE,
                &GRAPH,
                target(),
                runtime(),
                &changed_scripts,
                &environment,
                1,
                1
            )
        );
        assert_ne!(
            key_with(
                &SOURCE,
                &GRAPH,
                target(),
                runtime(),
                &empty_script,
                &environment,
                1,
                1
            ),
            key_with(
                &SOURCE,
                &GRAPH,
                target(),
                runtime(),
                &missing_script,
                &environment,
                1,
                1
            )
        );

        let changed_environment = BTreeMap::from([(
            OsString::from("SECRET_TOKEN"),
            OsString::from("second-value"),
        )]);
        assert_ne!(
            baseline,
            key_with(
                &SOURCE,
                &GRAPH,
                target(),
                runtime(),
                &scripts,
                &changed_environment,
                1,
                1
            )
        );
        assert_ne!(
            baseline,
            key_with(
                &SOURCE,
                &GRAPH,
                target(),
                runtime(),
                &scripts,
                &environment,
                2,
                1
            )
        );
        assert_ne!(
            baseline,
            key_with(
                &SOURCE,
                &GRAPH,
                target(),
                runtime(),
                &scripts,
                &environment,
                1,
                2
            )
        );
    }

    #[test]
    fn key_debug_output_never_contains_environment_values() {
        let scripts = BTreeMap::new();
        let environment = BTreeMap::from([(
            OsString::from("TOKEN"),
            OsString::from("do-not-expose-this"),
        )]);
        let key = key_with(
            &SOURCE,
            &GRAPH,
            target(),
            runtime(),
            &scripts,
            &environment,
            1,
            1,
        );

        assert!(!format!("{key:?}").contains("do-not-expose-this"));
    }
}
