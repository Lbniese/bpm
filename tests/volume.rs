use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde_json::json;

use bpm::gc::policy::GcPolicy;
use bpm::metadata::MetadataRepository;
use bpm::volume::read_graph_inventory;

fn hex(byte: u8, length: usize) -> String {
    std::iter::repeat_n(format!("{byte:x}"), length).collect()
}

fn write_graph_metadata(
    graph_dir: &Path,
    graph: &str,
    artifacts: serde_json::Value,
    derived: &[&str],
) {
    let metadata = json!({
        "graph_id_hex": graph,
        "layout_version": 7,
        "packages_materialized": 0,
        "bins_linked": 0,
        "inventory_version": 1,
        "artifacts": artifacts,
        "derived": derived,
    });
    fs::write(
        graph_dir.join("metadata.json"),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();
}

#[test]
fn malformed_graph_inventory_is_not_read_as_complete() {
    let graph = hex(7, 64);
    let temp = tempfile::tempdir().unwrap();
    let graph_dir = temp
        .path()
        .join("graphs/blake3")
        .join(&graph[..2])
        .join(&graph);
    fs::create_dir_all(&graph_dir).unwrap();

    let artifacts = json!([{"id": "not-a-hex-id", "requires_image": true}]);
    write_graph_metadata(&graph_dir, &graph, artifacts, &[]);

    assert!(read_graph_inventory(&graph_dir).is_none());
}

#[test]
fn malformed_graph_inventory_rejects_duplicate_artifact_ids() {
    let graph = hex(8, 64);
    let temp = tempfile::tempdir().unwrap();
    let graph_dir = temp
        .path()
        .join("graphs/blake3")
        .join(&graph[..2])
        .join(&graph);
    fs::create_dir_all(&graph_dir).unwrap();

    let artifact = hex(2, 128);
    let artifacts = json!([
        {"id": artifact, "requires_image": true},
        {"id": artifact, "requires_image": true},
    ]);
    write_graph_metadata(&graph_dir, &graph, artifacts, &[]);

    assert!(read_graph_inventory(&graph_dir).is_none());
}

#[test]
fn malformed_graph_inventory_rejects_duplicate_derived_ids() {
    let graph = hex(9, 64);
    let temp = tempfile::tempdir().unwrap();
    let graph_dir = temp
        .path()
        .join("graphs/blake3")
        .join(&graph[..2])
        .join(&graph);
    fs::create_dir_all(&graph_dir).unwrap();

    let derived = hex(3, 64);
    let artifacts = json!([]);
    write_graph_metadata(&graph_dir, &graph, artifacts, &[&derived, &derived]);

    assert!(read_graph_inventory(&graph_dir).is_none());
}

#[test]
fn malformed_graph_inventory_rejects_truncated_artifacts_array() {
    let graph = hex(9, 64);
    let temp = tempfile::tempdir().unwrap();
    let graph_dir = temp
        .path()
        .join("graphs/blake3")
        .join(&graph[..2])
        .join(&graph);
    fs::create_dir_all(&graph_dir).unwrap();

    fs::write(
        graph_dir.join("metadata.json"),
        r#"{
  "graph_id_hex": "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
  "layout_version": 7,
  "packages_materialized": 0,
  "bins_linked": 0,
  "inventory_version": 1,
  "artifacts": [{"id": "deadbeef", "requires_image": true
}"#,
    )
    .unwrap();

    assert!(read_graph_inventory(&graph_dir).is_none());
}

#[test]
fn malformed_graph_inventory_blocks_collection_as_incomplete_graph() {
    let temp = tempfile::tempdir().unwrap();
    let repository = MetadataRepository::open(temp.path()).unwrap();

    let artifact = hex(1, 128);
    let graph = hex(9, 64);

    let artifact_path = temp.path().join("artifacts/sha512").join(&artifact[..2]);
    fs::create_dir_all(&artifact_path).unwrap();
    fs::write(artifact_path.join(format!("{artifact}.tgz")), vec![1_u8; 4]).unwrap();

    let graph_dir = temp
        .path()
        .join("graphs/blake3")
        .join(&graph[..2])
        .join(&graph);
    fs::create_dir_all(&graph_dir).unwrap();
    let artifacts = json!([{"id": "not-a-hex-id", "requires_image": true}]);
    write_graph_metadata(&graph_dir, &graph, artifacts, &[]);

    let old = SystemTime::now() - Duration::from_secs(60);
    fs::File::open(artifact_path.join(format!("{artifact}.tgz")))
        .unwrap()
        .set_modified(old)
        .unwrap();
    fs::File::open(&graph_dir)
        .unwrap()
        .set_modified(old)
        .unwrap();

    let report = repository
        .collect(GcPolicy {
            grace: Duration::from_secs(1),
            max_size_bytes: None,
        })
        .unwrap();

    assert_eq!(
        report.deleted, 0,
        "malformed graph inventory must keep the graph and deps protected",
    );
    assert!(artifact_path.join(format!("{artifact}.tgz")).exists());
    assert!(graph_dir.exists());
}

#[test]
fn valid_graph_inventory_remains_complete_in_collection_workflow() {
    let temp = tempfile::tempdir().unwrap();
    let repository = MetadataRepository::open(temp.path()).unwrap();

    let artifact = hex(2, 128);
    let graph = hex(8, 64);

    let artifact_path = temp.path().join("artifacts/sha512").join(&artifact[..2]);
    fs::create_dir_all(&artifact_path).unwrap();
    fs::write(artifact_path.join(format!("{artifact}.tgz")), vec![2_u8; 4]).unwrap();

    let graph_dir = temp
        .path()
        .join("graphs/blake3")
        .join(&graph[..2])
        .join(&graph);
    fs::create_dir_all(&graph_dir).unwrap();
    let artifacts = json!([{"id": artifact, "requires_image": true}]);
    write_graph_metadata(&graph_dir, &graph, artifacts, &[]);

    let old = SystemTime::now() - Duration::from_secs(60);
    fs::File::open(artifact_path.join(format!("{artifact}.tgz")))
        .unwrap()
        .set_modified(old)
        .unwrap();
    fs::File::open(&graph_dir)
        .unwrap()
        .set_modified(old)
        .unwrap();

    let report = repository
        .collect(GcPolicy {
            grace: Duration::from_secs(1),
            max_size_bytes: None,
        })
        .unwrap();

    assert_eq!(
        report.deleted, 2,
        "valid complete graph should be collectible when no project references exist",
    );
    assert!(!artifact_path.join(format!("{artifact}.tgz")).exists());
    assert!(!graph_dir.exists());
}
