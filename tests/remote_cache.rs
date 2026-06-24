//! Deterministic tests for the verified read-through artifact cache.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use bpm::config::NpmConfig;
use bpm::integrity::{Integrity, Sha512Digest};
use bpm::metrics::Metrics;
use bpm::remote_cache::{RemoteCacheClient, RemoteCacheConfig, RemoteFetch};
use bpm::store::{ArtifactStore, RemoteArtifactSource};

fn server(status: &str, body: Vec<u8>) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let status = status.to_string();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let count = stream.read(&mut buf).unwrap();
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buf[..count]);
        }
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
        String::from_utf8_lossy(&request).into_owned()
    });
    (format!("http://{address}"), handle)
}

#[test]
fn remote_hit_uses_digest_path_and_publishes_verified_bytes() {
    let body = b"remote tarball".to_vec();
    let digest = Sha512Digest::hash_bytes(&body);
    let (base, request) = server("200 OK", body.clone());
    let config =
        RemoteCacheConfig::new_loopback_for_tests(&base, Some("cache-secret".into())).unwrap();
    let client = RemoteCacheClient::new(config).unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::open(store_dir.path()).unwrap();
    let origin = bpm::http::HttpClient::new(NpmConfig::default());
    let mut metrics = Metrics::new();

    let result = store
        .ensure_artifact_with_remote(
            &origin,
            &client,
            "http://127.0.0.1:1/origin.tgz",
            Some(&Integrity::sha512(digest)),
            &mut metrics,
        )
        .unwrap();
    assert_eq!(result.source, RemoteArtifactSource::Remote);
    assert_eq!(std::fs::read(result.artifact.path).unwrap(), body);
    let request = request.join().unwrap();
    assert!(request.contains(&format!("/v1/artifacts/sha512/{}", digest.to_hex())));
    assert!(
        request.contains("authorization: Bearer cache-secret")
            || request
                .to_ascii_lowercase()
                .contains("authorization: bearer cache-secret")
    );
    assert!(!metrics.to_json().contains("cache-secret"));
}

#[test]
fn remote_miss_is_a_normal_fallback() {
    let (base, request) = server("404 Not Found", Vec::new());
    let config = RemoteCacheConfig::new_loopback_for_tests(&base, None).unwrap();
    let client = RemoteCacheClient::new(config).unwrap();
    let digest = Sha512Digest::hash_bytes(b"unused");
    let destination = tempfile::NamedTempFile::new().unwrap();
    std::fs::remove_file(destination.path()).unwrap();
    let result = client.fetch_artifact(&digest, destination.path()).unwrap();
    assert_eq!(result, RemoteFetch::Miss);
    assert!(!destination.path().exists());
    let request = request.join().unwrap();
    assert!(
        request.contains("Accept: application/octet-stream")
            || request
                .to_ascii_lowercase()
                .contains("accept: application/octet-stream")
    );
}
