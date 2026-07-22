//! Optional verified read-through cache for raw package tarballs.
//!
//! The cache is deliberately a separate trust domain from npm registries. It
//! can provide bytes, but the local store remains authoritative and no remote
//! response is published until it hashes to the requested SHA-512 identity.

use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use reqwest::blocking::{Client, ClientBuilder};
use reqwest::redirect::Policy;
use thiserror::Error;

use crate::download::MAX_ARTIFACT_BYTES;
use crate::integrity::ArtifactId;

#[derive(Clone)]
struct RemoteCacheToken(String);

impl fmt::Debug for RemoteCacheToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Validated remote cache configuration. The token is never included in
/// debug output, errors, URLs, or serialized project state.
#[derive(Clone)]
pub struct RemoteCacheConfig {
    base_url: reqwest::Url,
    token: Option<RemoteCacheToken>,
}

impl fmt::Debug for RemoteCacheConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteCacheConfig")
            .field("base_url", &self.base_url.as_str())
            .field("token", &self.token)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum RemoteCacheConfigError {
    #[error("remote cache URL must use https (HTTP is allowed only for loopback test servers)")]
    InsecureScheme,
    #[error("remote cache URL must not contain userinfo, query, or fragment")]
    SensitiveUrl,
    #[error("invalid remote cache URL: {0}")]
    InvalidUrl(String),
    #[error("HTTP remote cache URLs are allowed only for localhost/loopback test servers")]
    NonLoopbackHttp,
}

impl RemoteCacheConfig {
    pub fn new(base_url: &str, token: Option<String>) -> Result<Self, RemoteCacheConfigError> {
        Self::parse(base_url, token, false)
    }

    /// Constructor for deterministic local integration tests. It accepts HTTP
    /// only when the endpoint is loopback; production callers should use
    /// [`Self::new`].
    pub fn new_loopback_for_tests(
        base_url: &str,
        token: Option<String>,
    ) -> Result<Self, RemoteCacheConfigError> {
        Self::parse(base_url, token, true)
    }

    fn parse(
        base_url: &str,
        token: Option<String>,
        allow_loopback_http: bool,
    ) -> Result<Self, RemoteCacheConfigError> {
        let mut url = base_url
            .parse::<reqwest::Url>()
            .map_err(|e| RemoteCacheConfigError::InvalidUrl(e.to_string()))?;
        if url.cannot_be_a_base() || url.host_str().is_none() {
            return Err(RemoteCacheConfigError::InvalidUrl(
                "absolute URL required".into(),
            ));
        }
        if url.username() != ""
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(RemoteCacheConfigError::SensitiveUrl);
        }
        let https = url.scheme().eq_ignore_ascii_case("https");
        let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
        if !(https || allow_loopback_http && url.scheme() == "http" && loopback) {
            return Err(if url.scheme() == "http" {
                if loopback {
                    RemoteCacheConfigError::InsecureScheme
                } else {
                    RemoteCacheConfigError::NonLoopbackHttp
                }
            } else {
                RemoteCacheConfigError::InsecureScheme
            });
        }
        while url.path().ends_with('/') && url.path() != "/" {
            let trimmed = url.path().trim_end_matches('/').to_string();
            url.set_path(&trimmed);
        }
        Ok(Self {
            base_url: url,
            token: token.filter(|v| !v.is_empty()).map(RemoteCacheToken),
        })
    }

    pub fn base_url(&self) -> &str {
        self.base_url.as_str()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteFetch {
    Hit { bytes_written: u64 },
    Miss,
}

#[derive(Debug, Error)]
pub enum RemoteCacheError {
    #[error("remote cache request failed at {endpoint}: {message}")]
    Request { endpoint: String, message: String },
    #[error("remote cache returned a redirect for {endpoint}")]
    Redirect { endpoint: String },
    #[error("remote cache response for {endpoint} is too large")]
    TooLarge { endpoint: String },
    #[error("remote cache response for {endpoint} was incomplete: {message}")]
    Body { endpoint: String, message: String },
    #[error("remote cache could not write {path}: {source}")]
    Write { path: String, source: io::Error },
}

/// A no-redirect, isolated HTTP client for the remote cache.
#[derive(Clone)]
pub struct RemoteCacheClient {
    config: RemoteCacheConfig,
    client: Client,
}

impl fmt::Debug for RemoteCacheClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteCacheClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl RemoteCacheClient {
    pub fn new(config: RemoteCacheConfig) -> Result<Self, RemoteCacheError> {
        let client = ClientBuilder::new()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_e| RemoteCacheError::Request {
                endpoint: config.base_url().to_string(),
                message: "client setup failed".into(),
            })?;
        Ok(Self { config, client })
    }

    pub fn config(&self) -> &RemoteCacheConfig {
        &self.config
    }

    /// Stream one raw artifact into an unpublished caller-owned temp file.
    /// Status 404 is a normal miss; every other non-200 status is an error so
    /// the caller can warn and fall back to the origin.
    pub fn fetch_artifact(
        &self,
        id: &ArtifactId,
        destination: &Path,
    ) -> Result<RemoteFetch, RemoteCacheError> {
        let endpoint = format!(
            "{}/v1/artifacts/sha512/{}",
            self.config.base_url().trim_end_matches('/'),
            id.to_hex()
        );
        let mut request = self
            .client
            .get(&endpoint)
            .header("Accept", "application/octet-stream");
        if let Some(token) = &self.config.token {
            request = request.bearer_auth(&token.0);
        }
        let mut response = request.send().map_err(|error| RemoteCacheError::Request {
            endpoint: endpoint.clone(),
            message: classify_reqwest(&error),
        })?;
        let status = response.status().as_u16();
        if status == 404 {
            return Ok(RemoteFetch::Miss);
        }
        if (300..400).contains(&status) {
            return Err(RemoteCacheError::Redirect { endpoint });
        }
        if status != 200 {
            return Err(RemoteCacheError::Request {
                endpoint,
                message: format!("HTTP status {status}"),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_ARTIFACT_BYTES)
        {
            return Err(RemoteCacheError::TooLarge { endpoint });
        }
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|source| RemoteCacheError::Write {
            path: parent.display().to_string(),
            source,
        })?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination)
            .map_err(|source| RemoteCacheError::Write {
                path: destination.display().to_string(),
                source,
            })?;
        let mut limited = (&mut response).take(MAX_ARTIFACT_BYTES + 1);
        let bytes = io::copy(&mut limited, &mut file).map_err(|source| RemoteCacheError::Body {
            endpoint: self.config.base_url().to_string(),
            message: source.to_string(),
        })?;
        if bytes > MAX_ARTIFACT_BYTES {
            let _ = std::fs::remove_file(destination);
            return Err(RemoteCacheError::TooLarge {
                endpoint: self.config.base_url().to_string(),
            });
        }
        file.flush()
            .and_then(|_| file.sync_all())
            .map_err(|source| RemoteCacheError::Write {
                path: destination.display().to_string(),
                source,
            })?;
        Ok(RemoteFetch::Hit {
            bytes_written: bytes,
        })
    }
}

fn classify_reqwest(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timeout".into()
    } else if error.is_connect() {
        "connection failed".into()
    } else if error.is_redirect() {
        "redirect rejected".into()
    } else {
        "transport failure".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_sensitive_and_insecure_urls() {
        assert!(matches!(
            RemoteCacheConfig::new("http://cache.example", None),
            Err(RemoteCacheConfigError::NonLoopbackHttp)
        ));
        assert!(matches!(
            RemoteCacheConfig::new("https://u:p@cache.example/x", None),
            Err(RemoteCacheConfigError::SensitiveUrl)
        ));
        assert!(RemoteCacheConfig::new("https://cache.example///", Some("secret".into())).is_ok());
    }
    #[test]
    fn debug_redacts_token() {
        let config =
            RemoteCacheConfig::new("https://cache.example", Some("super-secret".into())).unwrap();
        assert!(!format!("{config:?}").contains("super-secret"));
    }
}
