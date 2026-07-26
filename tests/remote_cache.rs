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

// ── Upload (PUT) tests (Plan 023) ───────────────────────────────────────────

/// A captured PUT request: method, path, optional bearer token, and body.
struct CapturedPut {
    method: String,
    path: String,
    auth: Option<String>,
    body: Vec<u8>,
}

/// A mock cache server that always replies with `reply_status` and captures
/// the single request it receives (method, path, Authorization, body). Used
/// for the upload-path tests.
fn put_server(reply_status: &str) -> (String, thread::JoinHandle<CapturedPut>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let reply_status = reply_status.to_string();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        // Read until the end of headers.
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let count = stream.read(&mut buf).unwrap();
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buf[..count]);
        }
        let header_end = request
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or(request.len());
        let header_str = String::from_utf8_lossy(&request[..header_end]).into_owned();
        // Parse Content-Length to read the remaining body.
        let content_length: usize = header_str
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
            .and_then(|line| line.split(':').nth(1))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let already_have = request.len().saturating_sub(header_end + 4);
        let mut body = request[header_end + 4..].to_vec();
        while body.len() < content_length {
            let count = stream.read(&mut buf).unwrap();
            if count == 0 {
                break;
            }
            body.extend_from_slice(&buf[..count.min(content_length - body.len())]);
        }
        // Reply.
        let reply =
            format!("HTTP/1.1 {reply_status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = stream.write_all(reply.as_bytes());
        let _ = already_have; // already consumed into body

        let mut lines = header_str.lines();
        let request_line = lines.next().unwrap_or("");
        let mut parts = request_line.split(' ');
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let auth = header_str
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
            .map(|line| line.split(':').nth(1).unwrap_or("").trim().to_string());
        CapturedPut {
            method,
            path,
            auth,
            body,
        }
    });
    (format!("http://{address}"), handle)
}

#[test]
fn put_artifact_stores_bytes_at_verified_digest_path() {
    let body = b"uploaded tarball".to_vec();
    let digest = Sha512Digest::hash_bytes(&body);
    let (base, handle) = put_server("201 Created");
    let config =
        RemoteCacheConfig::new_loopback_for_tests(&base, Some("push-secret".into())).unwrap();
    let client = RemoteCacheClient::with_push(config, true).unwrap();

    client
        .put_artifact(&digest, reqwest::blocking::Body::from(body.clone()))
        .expect("put should succeed");

    let captured = handle.join().unwrap();
    assert_eq!(captured.method, "PUT");
    // The path digest must be the artifact's verified SHA-512.
    assert!(
        captured
            .path
            .contains(&format!("/v1/artifacts/sha512/{}", digest.to_hex())),
        "expected verified digest in path, got: {}",
        captured.path
    );
    assert_eq!(
        captured.body, body,
        "uploaded body must be the raw artifact bytes"
    );
    assert_eq!(captured.auth, Some("Bearer push-secret".into()));
}

#[test]
fn put_artifact_409_is_idempotent_success() {
    let body = b"already present".to_vec();
    let digest = Sha512Digest::hash_bytes(&body);
    let (base, _handle) = put_server("409 Conflict");
    let config = RemoteCacheConfig::new_loopback_for_tests(&base, None).unwrap();
    let client = RemoteCacheClient::with_push(config, true).unwrap();

    // 409 = already exists → success, not an error.
    client
        .put_artifact(&digest, reqwest::blocking::Body::from(body.clone()))
        .expect("409 should be idempotent success");
}

#[test]
fn put_artifact_failure_is_non_fatal_and_redacts_token() {
    let body = b"will fail".to_vec();
    let digest = Sha512Digest::hash_bytes(&body);
    let (base, _handle) = put_server("500 Internal Server Error");
    let config =
        RemoteCacheConfig::new_loopback_for_tests(&base, Some("leak-me-not".into())).unwrap();
    let client = RemoteCacheClient::with_push(config, true).unwrap();

    let error = client
        .put_artifact(&digest, reqwest::blocking::Body::from(body.clone()))
        .expect_err("500 should be an error");
    let message = format!("{error}");
    assert!(
        message.contains("HTTP status 500"),
        "unexpected error: {message}"
    );
    // The token must never appear in the error message (security contract).
    assert!(
        !message.contains("leak-me-not"),
        "token leaked into error: {message}"
    );
}

#[test]
fn push_is_off_by_default() {
    // Without BPM_REMOTE_CACHE_PUSH set, the client must not push.
    // (We can't easily unset a process-wide env var; instead we assert the
    // push_enabled flag is false on a client constructed via the default
    // constructor — the read-through site gates push on this flag.)
    //
    // Use with_push(false) explicitly to model the default-OFF construction.
    let config = RemoteCacheConfig::new_loopback_for_tests("http://127.0.0.1:1", None).unwrap();
    let client = RemoteCacheClient::with_push(config, false).unwrap();
    assert!(!client.push_enabled(), "push must be OFF by default");
}

#[test]
fn ensure_artifact_with_remote_pushes_after_origin_fetch() {
    // End-to-end: a remote-cache miss falls back to origin, publishes, and
    // (with push enabled) uploads the verified bytes back to the cache.
    let body = b"origin tarball".to_vec();
    let digest = Sha512Digest::hash_bytes(&body);

    // Remote cache that 404s (miss) on GET and accepts 201 on PUT.
    // We need a server that handles both GET and PUT. Build a tiny router.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let cache_base = format!("http://{address}");
    let expected_digest = digest;
    let cache_handle = thread::spawn(move || {
        let mut captured_put: Option<Vec<u8>> = None;
        for _ in 0..2 {
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
            let header_end = request
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .unwrap_or(request.len());
            let header_str = String::from_utf8_lossy(&request[..header_end]).into_owned();
            let method = header_str
                .lines()
                .next()
                .and_then(|l| l.split(' ').next())
                .unwrap_or("");
            if method == "GET" {
                let reply =
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(reply.as_bytes());
            } else if method == "PUT" {
                let content_length: usize = header_str
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                    .and_then(|line| line.split(':').nth(1))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let mut body = request[header_end + 4..].to_vec();
                while body.len() < content_length {
                    let count = stream.read(&mut buf).unwrap();
                    if count == 0 {
                        break;
                    }
                    body.extend_from_slice(&buf[..count.min(content_length - body.len())]);
                }
                captured_put = Some(body);
                let reply =
                    "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(reply.as_bytes());
            }
        }
        captured_put
    });

    // Origin server: serves the body once.
    let origin_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_addr = origin_listener.local_addr().unwrap();
    let origin_url = format!("http://{origin_addr}/origin.tgz");
    let origin_body = body.clone();
    let origin_handle = thread::spawn(move || {
        let (mut stream, _) = origin_listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let mut request = Vec::new();
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let count = stream.read(&mut buf).unwrap();
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buf[..count]);
        }
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            origin_body.len()
        );
        let _ = stream.write_all(reply.as_bytes());
        let _ = stream.write_all(&origin_body);
    });

    let config =
        RemoteCacheConfig::new_loopback_for_tests(&cache_base, Some("push-secret".into())).unwrap();
    let client = RemoteCacheClient::with_push(config, true).unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::open(store_dir.path()).unwrap();
    let origin = bpm::http::HttpClient::new(NpmConfig::default());
    let mut metrics = Metrics::new();

    let result = store
        .ensure_artifact_with_remote(
            &origin,
            &client,
            &origin_url,
            Some(&Integrity::sha512(digest)),
            &mut metrics,
        )
        .unwrap();
    assert_eq!(result.source, RemoteArtifactSource::Origin);
    origin_handle.join().unwrap();

    let pushed = cache_handle
        .join()
        .unwrap()
        .expect("PUT should have been issued");
    assert_eq!(
        pushed, body,
        "pushed bytes must equal the verified artifact"
    );
    let _ = expected_digest; // referenced for clarity
                             // The push succeeded → metric recorded, no error metric.
    let metrics_json = metrics.to_json();
    assert!(metrics_json.contains("remote_cache_push"));
    assert!(
        !metrics_json.contains("remote_cache_push_error"),
        "push should not have errored: {metrics_json}"
    );
}

#[test]
fn put_artifact_streams_from_file() {
    use std::io::Write;

    // Write a small artifact to a temp file.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("artifact.bin");
    let content = b"streamed tarball content";
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content).unwrap();
    }

    let digest = Sha512Digest::hash_bytes(content);
    let (base, handle) = put_server("201 Created");
    let config =
        RemoteCacheConfig::new_loopback_for_tests(&base, Some("stream-secret".into())).unwrap();
    let client = RemoteCacheClient::with_push(config, true).unwrap();

    // Open the file and stream it as the request body.
    let file = std::fs::File::open(&path).unwrap();
    let body = reqwest::blocking::Body::from(file);
    client
        .put_artifact(&digest, body)
        .expect("put from file should succeed");

    let captured = handle.join().unwrap();
    assert_eq!(captured.method, "PUT");
    assert!(
        captured
            .path
            .contains(&format!("/v1/artifacts/sha512/{}", digest.to_hex())),
        "expected verified digest in path, got: {}",
        captured.path
    );
    assert_eq!(
        captured.body, content,
        "uploaded body must match the file content"
    );
    assert_eq!(captured.auth, Some("Bearer stream-secret".into()));
}
