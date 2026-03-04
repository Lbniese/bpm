//! Persistent normal-suite coverage for the resolver's deterministic value model.
//!
//! The resolver receptionist is intentionally not public yet, so this test imports
//! only its model file directly. Future resolver wiring remains independently owned.

#[path = "../src/resolver/model.rs"]
mod model;

use std::collections::BTreeSet;

use model::{
    DependencyEdge, DependencyKind, PackageIdentity, PackageInstance, PackageMetadata,
    PackageSource, ResolutionDiagnostic, ResolvedGraph, TargetPlatform,
};

fn identity(name: &str) -> PackageIdentity {
    PackageIdentity {
        name: name.into(),
        version: "1.0.0".into(),
        source: PackageSource::Registry {
            registry: "https://registry.npmjs.org/".into(),
        },
        peer_context: Default::default(),
    }
}

fn edge(kind: DependencyKind, name: &str, target: PackageIdentity) -> DependencyEdge {
    DependencyEdge {
        kind,
        name: name.into(),
        spec: "1".into(),
        target,
    }
}

fn instance(
    identity: PackageIdentity,
    edges: impl IntoIterator<Item = DependencyEdge>,
) -> PackageInstance {
    PackageInstance {
        identity,
        metadata: PackageMetadata::default(),
        edges: edges.into_iter().collect(),
    }
}

#[test]
fn graph_construction_and_bytes_are_independent_of_all_insertion_order() {
    let target = TargetPlatform {
        os: "linux".into(),
        cpu: "x64".into(),
        libc: Some("glibc".into()),
    };
    let alpha = identity("alpha");
    let beta = identity("beta");
    let zeta = identity("zeta");

    let alpha_edge = edge(DependencyKind::Prod, "alpha", alpha.clone());
    let beta_edge = edge(DependencyKind::Optional, "beta", beta.clone());
    let zeta_edge = edge(DependencyKind::Dev, "zeta", zeta.clone());

    let alpha_left = instance(alpha.clone(), [zeta_edge.clone(), beta_edge.clone()]);
    let alpha_right = instance(alpha.clone(), [beta_edge.clone(), zeta_edge.clone()]);
    let beta_instance = instance(beta, BTreeSet::new());
    let zeta_instance = instance(zeta, BTreeSet::new());

    let alpha_diagnostic =
        ResolutionDiagnostic::new("A_OPTIONAL", "alpha optional dependency skipped")
            .with_package("alpha");
    let zeta_diagnostic = ResolutionDiagnostic::new("Z_METADATA", "zeta metadata ignored");

    let left = ResolvedGraph {
        root: [zeta_edge.clone(), alpha_edge.clone()]
            .into_iter()
            .collect(),
        instances: [zeta_instance.clone(), beta_instance.clone(), alpha_left]
            .into_iter()
            .collect(),
        diagnostics: [zeta_diagnostic.clone(), alpha_diagnostic.clone()]
            .into_iter()
            .collect(),
    };
    let right = ResolvedGraph {
        root: [alpha_edge, zeta_edge].into_iter().collect(),
        instances: [alpha_right, beta_instance, zeta_instance]
            .into_iter()
            .collect(),
        diagnostics: [alpha_diagnostic, zeta_diagnostic].into_iter().collect(),
    };

    assert_eq!(left, right);
    assert_eq!(left.root.iter().next().unwrap().name, "alpha");
    assert_eq!(left.instances.iter().next().unwrap().identity.name, "alpha");
    assert_eq!(
        left.instances.iter().next().unwrap().edges,
        [
            beta_edge,
            edge(DependencyKind::Dev, "zeta", identity("zeta"))
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(left.diagnostics.iter().next().unwrap().code, "A_OPTIONAL");
    assert_eq!((target.os.as_str(), target.cpu.as_str()), ("linux", "x64"));

    let left_bytes = serde_json::to_vec(&left).unwrap();
    let right_bytes = serde_json::to_vec(&right).unwrap();
    assert_eq!(left_bytes, right_bytes);
    assert_eq!(
        serde_json::from_slice::<ResolvedGraph>(&left_bytes).unwrap(),
        left
    );
}
