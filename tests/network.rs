mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use bpm::config::NpmConfig;
use bpm::http::HttpClient;

use common::{MiniServer, TransientFailure};

fn client_with_npmrc(project: &Path, lines: &[&str]) -> HttpClient {
    fs::write(project.join(".npmrc"), format!("{}\n", lines.join("\n"))).unwrap();
    HttpClient::new(NpmConfig::load(project, None).unwrap())
}

fn auth_line(authority: &str, token: &str) -> String {
    format!("//{authority}/:_authToken={token}")
}

fn retry_after_http_date() -> String {
    httpdate::fmt_http_date(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
}

fn start_redirect_server(location: String) -> (String, Arc<Mutex<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_thread = seen.clone();

    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while buf.len() < 64 * 1024 {
            let Ok(read) = stream.read(&mut chunk) else {
                break;
            };
            if read == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..read]);
            if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        *seen_thread.lock().unwrap() = buf.clone();
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
    });

    (format!("http://{addr}"), seen)
}

#[derive(Clone)]
struct ScriptedResponse {
    status: u16,
    body: Vec<u8>,
    content_type: &'static str,
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        408 => "Request Timeout",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Scripted Failure",
    }
}

fn read_request(
    stream: &mut std::net::TcpStream,
    sequence: usize,
    connection_id: usize,
) -> Option<common::CapturedRequest> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    while bytes.len() < 64 * 1024 {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    if bytes.is_empty() {
        return None;
    }

    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.split("\r\n");
    let mut request_parts = lines.next().unwrap_or_default().split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_owned();
    let path = request_parts.next().unwrap_or("/").to_owned();
    let mut headers = std::collections::BTreeMap::<String, Vec<String>>::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers
            .entry(name.trim().to_ascii_lowercase())
            .or_default()
            .push(value.trim().to_owned());
    }

    Some(common::CapturedRequest {
        sequence,
        connection_id,
        method,
        path,
        headers,
    })
}

fn start_scripted_retry_server(
    responses: Vec<ScriptedResponse>,
) -> (String, Arc<Mutex<Vec<common::CapturedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_thread = requests.clone();
    let responses = Arc::new(responses);
    let hits = Arc::new(AtomicUsize::new(0));
    let connections = Arc::new(AtomicUsize::new(0));

    let _ = thread::spawn({
        let responses = responses.clone();
        let hits = hits.clone();
        let connections = connections.clone();
        move || {
            for incoming in listener.incoming() {
                let Ok(mut stream) = incoming else { break };
                let connection_id = connections.fetch_add(1, Ordering::SeqCst) + 1;
                let responses = responses.clone();
                let requests = requests_thread.clone();
                let hits = hits.clone();

                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

                    loop {
                        let sequence = hits.load(Ordering::SeqCst);
                        let Some(request) = read_request(&mut stream, sequence, connection_id)
                        else {
                            break;
                        };
                        let sequence = hits.fetch_add(1, Ordering::SeqCst);
                        let mut request = request;
                        request.sequence = sequence;
                        requests.lock().unwrap().push(request);

                        let response = responses
                            .get(sequence)
                            .cloned()
                            .unwrap_or_else(|| responses.last().cloned().unwrap());
                        let head = format!(
                            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: keep-alive\r\n\r\n",
                            response.status,
                            status_reason(response.status),
                            response.body.len(),
                            response.content_type,
                        );
                        if stream.write_all(head.as_bytes()).is_err() {
                            break;
                        }
                        for chunk in response.body.chunks(4096) {
                            if stream.write_all(chunk).is_err() {
                                break;
                            }
                        }
                        let _ = stream.flush();
                    }
                });
            }
        }
    });

    (format!("http://{addr}"), requests)
}

#[test]
fn retryable_status_exhaustion_reports_attempt_count() {
    let server = MiniServer::start_routed_with_failures(
        vec![TransientFailure::new(408), TransientFailure::new(429)],
        |_path| None,
    );
    let project = tempfile::tempdir().unwrap();
    let client = client_with_npmrc(project.path(), &["fetch-retries=1"]);

    let err = client.get(&server.url("/pkg")).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("after 2 attempt(s)"), "{text}");
}

#[test]
fn retry_after_http_date_exhaustion_reports_attempt_count() {
    let retry_after = retry_after_http_date();
    let server = MiniServer::start_routed_with_failures(
        vec![
            TransientFailure::new(503).with_retry_after(retry_after),
            TransientFailure::new(503),
        ],
        |_path| None,
    );
    let project = tempfile::tempdir().unwrap();
    let client = client_with_npmrc(
        project.path(),
        &[
            "fetch-retries=1",
            "fetch-retry-mintimeout=1",
            "fetch-retry-maxtimeout=1",
            "fetch-retry-factor=1",
        ],
    );

    let err = client.get(&server.url("/pkg")).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("after 2 attempt(s)"), "{text}");
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn retryable_body_drain_keeps_connection_reusable_after_retry() {
    let (url, requests) = start_scripted_retry_server(vec![
        ScriptedResponse {
            status: 503,
            body: b"retry body".to_vec(),
            content_type: "text/plain",
        },
        ScriptedResponse {
            status: 200,
            body: b"success body".to_vec(),
            content_type: "text/plain",
        },
    ]);
    let project = tempfile::tempdir().unwrap();
    let client = client_with_npmrc(
        project.path(),
        &[
            "fetch-retries=1",
            "fetch-retry-mintimeout=1",
            "fetch-retry-maxtimeout=1",
            "fetch-retry-factor=1",
        ],
    );

    let response = client.get(&url).unwrap();
    let mut body = String::new();
    response.into_reader().read_to_string(&mut body).unwrap();

    assert_eq!(body, "success body");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].connection_id, requests[1].connection_id);
}

#[test]
fn oversized_retryable_body_does_not_break_the_retry() {
    let (url, requests) = start_scripted_retry_server(vec![
        ScriptedResponse {
            status: 503,
            body: vec![b'x'; 64 * 1024 + 2],
            content_type: "text/plain",
        },
        ScriptedResponse {
            status: 200,
            body: b"done".to_vec(),
            content_type: "text/plain",
        },
    ]);
    let project = tempfile::tempdir().unwrap();
    let client = client_with_npmrc(
        project.path(),
        &[
            "fetch-retries=1",
            "fetch-retry-mintimeout=1",
            "fetch-retry-maxtimeout=1",
            "fetch-retry-factor=1",
        ],
    );

    let response = client.get(&url).unwrap();
    let mut body = String::new();
    response.into_reader().read_to_string(&mut body).unwrap();

    assert_eq!(body, "done");
    let requests = requests.lock().unwrap();
    // The bounded drain reads at most the retry limit, then drops the rest so a
    // pathological error body can never block the retry loop. reqwest manages
    // the pooled connection itself from there \u2014 it may background-drain and
    // reuse it or close it \u2014 so the retry is not required to open a fresh
    // connection the way ureq did. What matters is exactly one retry attempt
    // reaches the server and the final body is intact.
    assert_eq!(requests.len(), 2);
}

#[test]
fn small_success_body_keeps_connection_reusable() {
    let server = MiniServer::start_keep_alive_routed(|_| {
        Some(common::RouteBody(b"small body".to_vec(), "text/plain"))
    });
    let project = tempfile::tempdir().unwrap();
    let client = client_with_npmrc(project.path(), &[]);

    let first = client.get(&server.url("/one")).unwrap();
    let mut first_body = String::new();
    first.into_reader().read_to_string(&mut first_body).unwrap();
    let second = client.get(&server.url("/two")).unwrap();
    let mut second_body = String::new();
    second
        .into_reader()
        .read_to_string(&mut second_body)
        .unwrap();

    assert_eq!(first_body, "small body");
    assert_eq!(second_body, "small body");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].connection_id, requests[1].connection_id);
}

#[test]
fn stream_reads_body_and_preserves_pooling() {
    let server = MiniServer::start_keep_alive_routed(|_| {
        Some(common::RouteBody(b"stream body".to_vec(), "text/plain"))
    });
    let project = tempfile::tempdir().unwrap();
    let client = client_with_npmrc(project.path(), &[]);

    let mut reader = client.stream(&server.url("/stream")).unwrap();
    let mut body = String::new();
    reader.read_to_string(&mut body).unwrap();

    assert_eq!(body, "stream body");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn host_and_path_tokens_are_scoped() {
    let project = tempfile::tempdir().unwrap();
    let server = MiniServer::start_keep_alive_routed(|_| {
        Some(common::RouteBody(b"ok".to_vec(), "text/plain"))
    });
    let authority = server
        .url("")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let client = client_with_npmrc(
        project.path(),
        &[
            &auth_line(&authority, "root-token"),
            &format!("//{authority}/pkg/:_authToken=path-token"),
        ],
    );

    let _ = client.get(&server.url("/pkg/one")).unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer path-token")
    );
}

#[test]
fn redirect_does_not_forward_authorization_across_origins() {
    let target = MiniServer::start_keep_alive_routed(|_| {
        Some(common::RouteBody(b"redirect target".to_vec(), "text/plain"))
    });
    let (origin_url, origin_seen) = start_redirect_server(target.url("/final"));
    let project = tempfile::tempdir().unwrap();
    let client = client_with_npmrc(
        project.path(),
        &[&auth_line(
            origin_url.trim_start_matches("http://"),
            "origin-token",
        )],
    );

    let body = client.get(&origin_url).unwrap();
    let mut text = String::new();
    body.into_reader().read_to_string(&mut text).unwrap();

    assert_eq!(text, "redirect target");
    let origin_request = String::from_utf8(origin_seen.lock().unwrap().clone()).unwrap();
    assert!(origin_request
        .to_ascii_lowercase()
        .contains("authorization: bearer origin-token"));
    assert_eq!(target.requests()[0].header("authorization"), None);
}

#[test]
fn invalid_urls_and_redacted_errors_are_actionable() {
    let server =
        MiniServer::start_routed_with_failures(vec![TransientFailure::new(500)], |_path| None);
    let project = tempfile::tempdir().unwrap();
    let client = client_with_npmrc(project.path(), &[]);

    let bad = client.get("not-a-url").unwrap_err().to_string();
    assert!(bad.contains("<invalid-url>"), "{bad}");
    assert!(bad.contains("after 1 attempt(s)"), "{bad}");

    let redacted = client
        .get(&format!(
            "http://user:pass@{}/secret?token=1#frag",
            server.url("/")
        ))
        .unwrap_err()
        .to_string();
    assert!(!redacted.contains("user:pass"), "{redacted}");
    assert!(!redacted.contains("token=1"), "{redacted}");
    assert!(redacted.contains("/secret"), "{redacted}");
}
