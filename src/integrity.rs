//! Integrity identifiers for artifact verification.
//!
//! npm registry records the integrity of a package tarball as
//! `sha512-<base64(digest)>`, where `digest` is the 64-byte SHA-512 of the
//! tarball bytes. We reuse that exact digest as the artifact's identity in the
//! store: `artifact_id = sha512(package tarball bytes)`.
//!
//! Only `sha512` is supported. Other algorithms are rejected rather than
//! silently ignored.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use std::fmt;
use thiserror::Error;

/// Algorithm prefix for the only supported integrity scheme.
pub const SHA512: &str = "sha512";

/// A 64-byte SHA-512 digest, used as the identity of a stored artifact.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha512Digest([u8; 64]);

impl Sha512Digest {
    /// Wrap a raw digest.
    pub fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }

    /// Raw bytes of the digest.
    pub fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }

    /// Lowercase hex encoding (filesystem-safe), used for store paths.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// First 12 hex chars, for compact human-facing display.
    pub fn to_hex_short(&self) -> String {
        // self.0 hex-encodes to 128 chars; 12 is safely within bounds.
        self.to_hex()[..12].to_string()
    }

    /// Parse a 128-char lowercase hex digest.
    pub fn from_hex(s: &str) -> Result<Self, IntegrityError> {
        let bytes = hex::decode(s).map_err(|e| IntegrityError::BadEncoding(e.to_string()))?;
        if bytes.len() != 64 {
            return Err(IntegrityError::BadLength(bytes.len()));
        }
        let mut out = [0u8; 64];
        out.copy_from_slice(&bytes);
        Ok(Self(out))
    }

    /// Standard base64 encoding, matching npm integrity strings.
    pub fn to_base64(&self) -> String {
        BASE64.encode(self.0)
    }

    /// Parse npm-style base64 digest (86..=88 chars after padding).
    pub fn from_base64(s: &str) -> Result<Self, IntegrityError> {
        let bytes = BASE64
            .decode(s)
            .map_err(|e| IntegrityError::BadEncoding(e.to_string()))?;
        if bytes.len() != 64 {
            return Err(IntegrityError::BadLength(bytes.len()));
        }
        let mut out = [0u8; 64];
        out.copy_from_slice(&bytes);
        Ok(Self(out))
    }

    /// Compute the SHA-512 digest of a byte slice (trusted, e.g. from a local fixture).
    pub fn hash_bytes(bytes: &[u8]) -> Self {
        use sha2::{Digest, Sha512};
        let mut h = Sha512::new();
        h.update(bytes);
        let mut out = [0u8; 64];
        out.copy_from_slice(&h.finalize());
        Self(out)
    }

    /// Algorithm label.
    pub fn algorithm(&self) -> &'static str {
        SHA512
    }

    /// Canonical npm integrity string for this digest: `sha512-<base64>`.
    pub fn to_npm_string(&self) -> String {
        format!("{SHA512}-{}", self.to_base64())
    }
}

impl fmt::Debug for Sha512Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Sha512Digest").field(&self.to_hex()).finish()
    }
}

impl fmt::Display for Sha512Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A parsed integrity string, e.g. `sha512-<base64>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integrity {
    digest: Sha512Digest,
}

impl Integrity {
    /// Construct from an already-verified digest.
    pub fn sha512(digest: Sha512Digest) -> Self {
        Self { digest }
    }

    /// The algorithm label (`"sha512"`).
    pub fn algorithm(&self) -> &'static str {
        SHA512
    }

    /// The underlying digest (`artifact_id`).
    pub fn digest(&self) -> &Sha512Digest {
        &self.digest
    }

    /// Reconstruct the canonical npm integrity string.
    pub fn to_npm_string(&self) -> String {
        self.digest.to_npm_string()
    }

    /// Parse `sha512-<value>` where `value` is base64 (npm) or hex.
    ///
    /// npm records the digest as standard (padded) base64; we also accept a
    /// 128-char hex form. We accept by *decoding to 64 bytes* rather than
    /// matching on the encoded length, so the canonical npm length is handled
    /// without being brittle to padding.
    pub fn parse(s: &str) -> Result<Self, IntegrityError> {
        let (algo, value) = s
            .split_once('-')
            .ok_or_else(|| IntegrityError::InvalidFormat(s.to_string()))?;
        if algo != SHA512 {
            return Err(IntegrityError::UnsupportedAlgorithm(algo.to_string()));
        }
        match Sha512Digest::from_base64(value) {
            Ok(digest) => Ok(Self { digest }),
            // Fall back to a 128-char hex digest; any other shape is invalid.
            Err(_) if value.len() == 128 => Ok(Self {
                digest: Sha512Digest::from_hex(value)?,
            }),
            Err(_) => Err(IntegrityError::InvalidFormat(s.to_string())),
        }
    }
}

/// The identity of a stored artifact: its verified SHA-512 digest.
pub type ArtifactId = Sha512Digest;

#[derive(Debug, Error)]
pub enum IntegrityError {
    #[error("invalid integrity string: {0}")]
    InvalidFormat(String),
    #[error("unsupported integrity algorithm: {0} (only sha512 is supported)")]
    UnsupportedAlgorithm(String),
    #[error("invalid digest encoding: {0}")]
    BadEncoding(String),
    #[error("sha512 digest must be 64 bytes, got {0}")]
    BadLength(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_npm_integrity_string() {
        let bytes = [0xABu8; 64];
        let digest = Sha512Digest::from_bytes(bytes);
        let integ = Integrity::sha512(digest);
        let s = integ.to_npm_string();
        assert!(s.starts_with("sha512-"));
        let reparsed = Integrity::parse(&s).unwrap();
        assert_eq!(reparsed, integ);
    }

    #[test]
    fn rejects_non_sha512() {
        let err = Integrity::parse("sha1-AAAA").unwrap_err();
        assert!(matches!(err, IntegrityError::UnsupportedAlgorithm(_)));
    }

    #[test]
    fn rejects_malformed() {
        assert!(matches!(
            Integrity::parse("nodash"),
            Err(IntegrityError::InvalidFormat(_))
        ));
        assert!(matches!(
            Integrity::parse("sha512-tooshort"),
            Err(IntegrityError::InvalidFormat(_))
        ));
    }

    #[test]
    fn hash_bytes_is_deterministic() {
        let a = Sha512Digest::hash_bytes(b"hello").to_hex();
        let b = Sha512Digest::hash_bytes(b"hello").to_hex();
        assert_eq!(a, b);
        assert_ne!(a, Sha512Digest::hash_bytes(b"world").to_hex());
    }
}

#[test]
fn debug_b64() {
    let val = "q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urqw==";
    println!("len={}", val.len());
    match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, val) {
        Ok(b) => println!("decoded {} bytes", b.len()),
        Err(e) => println!("b64 err: {}", e),
    }
    // Also try with no padding
    let val2 = "q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urqw";
    match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, val2) {
        Ok(b) => println!("no pad decoded {} bytes", b.len()),
        Err(e) => println!("no pad b64 err: {}", e),
    }
    // Try with STANDARD_NO_PAD
    match base64::Engine::decode(&base64::engine::general_purpose::STANDARD_NO_PAD, val2) {
        Ok(b) => println!("no pad engine decoded {} bytes", b.len()),
        Err(e) => println!("no pad engine err: {}", e),
    }
}
