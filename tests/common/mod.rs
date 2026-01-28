//! Shared test helpers: build in-memory tar.gz archives and run a tiny local
//! HTTP server, so download/store/concurrency tests need no network.
// Not every helper is used by every test file.
#![allow(dead_code)]

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bpm::integrity::Sha512Digest;

/// Build a gzip-compressed tar in memory. `build` appends entries to the builder.
pub fn build_tgz<F>(build: F) -> Vec<u8>
where
    F: FnOnce(&mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>),
{
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    build(&mut builder);
    let enc = builder.into_inner().expect("tar into_inner");
    enc.finish().expect("gzip finish")
}

/// Append a regular file `path` with `mode` and contents `data`.
pub fn add_file(
    b: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>,
    path: &str,
    mode: u32,
    data: &[u8],
) {
    let mut h = tar::Header::new_gnu();
    h.set_path(path).unwrap();
    h.set_size(data.len() as u64);
    h.set_mode(mode);
    h.set_cksum();
    b.append(&h, data).unwrap();
}

/// Append a directory entry.
pub fn add_dir(b: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>, path: &str, mode: u32) {
    let mut h = tar::Header::new_gnu();
    h.set_path(path).unwrap();
    h.set_entry_type(tar::EntryType::Directory);
    h.set_size(0);
    h.set_mode(mode);
    h.set_cksum();
    b.append(&h, &[][..]).unwrap();
}

/// Append a symlink entry: `path` -> `target`.
pub fn add_symlink(
    b: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>,
    path: &str,
    target: &str,
) {
    let mut h = tar::Header::new_gnu();
    h.set_path(path).unwrap();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_link_name(target).unwrap();
    h.set_size(0);
    h.set_mode(0o777);
    h.set_cksum();
    b.append(&h, &[][..]).unwrap();
}

/// npm-style integrity string for `bytes`.
pub fn integrity_of(bytes: &[u8]) -> String {
    Sha512Digest::hash_bytes(bytes).to_npm_string()
}

/// A single-endpoint HTTP/1.1 server returning fixed bytes for any GET.
pub struct MiniServer {
    addr: String,
    hits: Arc<AtomicUsize>,
    _handle: thread::JoinHandle<()>,
}

impl MiniServer {
    /// Start serving `body` for every request. Counts every accepted request.
    pub fn start(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr").to_string();
        let hits = Arc::new(AtomicUsize::new(0));
        let body = Arc::new(body);
        let hits_for_thread = hits.clone();

        // Keep connections short so repeated fetches don't reuse a pooled one.
        let _ = listener.set_nonblocking(false);
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let body = body.clone();
                let hits = hits_for_thread.clone();
                thread::spawn(move || serve(stream, body, hits));
            }
        });

        Self {
            addr,
            hits,
            _handle: handle,
        }
    }

    pub fn url(&self, path: &str) -> String {
        let addr = &self.addr;
        format!("http://{addr}/{path}", path = path)
    }

    pub fn url_for(&self) -> String {
        self.url("pkg.tgz")
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }
}

fn serve(mut stream: TcpStream, body: Arc<Vec<u8>>, hits: Arc<AtomicUsize>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    hits.fetch_add(1, Ordering::Relaxed);

    // Drain the request line/headers (best-effort); the body is a GET.
    let mut buf = [0u8; 1024];
    let _ = std::io::Read::read(&mut stream, &mut buf);

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/gzip\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&body);
}

/// One raw tar entry, written byte-for-byte (bypassing `tar::Builder`'s path
/// sanitizer) so hostile paths can be constructed for security tests.
pub struct RawEntry<'a> {
    pub name: &'a [u8],
    pub typeflag: u8,
    pub linkname: &'a [u8],
    pub mode: u32,
    pub data: &'a [u8],
}

fn write_octal(buf: &mut [u8], value: u64, width: usize) {
    let s = format!("{value:0width$o}");
    let body = s.as_bytes();
    // width+1 to leave room for a trailing NUL
    buf[..body.len()].copy_from_slice(body);
    buf[body.len()] = 0;
}

/// Compress a raw tar stream gzip-compressed from the given raw entries.
pub fn build_raw_tgz(entries: &[RawEntry<'_>]) -> Vec<u8> {
    let mut tar_bytes = Vec::new();
    for e in entries {
        let mut h = [0u8; 512];
        h[..e.name.len()].copy_from_slice(e.name);
        write_octal(&mut h[100..108], e.mode as u64, 7);
        write_octal(&mut h[108..116], 0, 7);
        write_octal(&mut h[116..124], 0, 7);
        write_octal(&mut h[124..136], e.data.len() as u64, 11);
        write_octal(&mut h[136..148], 0, 11);
        h[156] = e.typeflag;
        h[157..157 + e.linkname.len()].copy_from_slice(e.linkname);
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        // checksum with field as spaces
        for slot in &mut h[148..156] {
            *slot = b' ';
        }
        let sum: u32 = h.iter().map(|&b| b as u32).sum();
        let chk = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(chk.as_bytes());

        tar_bytes.extend_from_slice(&h);
        tar_bytes.extend_from_slice(e.data);
        let pad = (512 - (e.data.len() % 512)) % 512;
        tar_bytes.extend(std::iter::repeat_n(0u8, pad));
    }
    // two empty blocks terminate the archive
    tar_bytes.extend(std::iter::repeat_n(0u8, 1024));

    use std::io::Read;
    let mut enc = flate2::read::GzEncoder::new(
        std::io::Cursor::new(tar_bytes),
        flate2::Compression::default(),
    );
    let mut out = Vec::new();
    enc.read_to_end(&mut out).expect("gzip encode");
    out
}
