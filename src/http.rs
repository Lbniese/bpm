//! Shared blocking HTTP client for registry metadata and artifact streams.
//!
//! A client owns one clonable [`ureq::Agent`], so cloned clients share its
//! connection pool. Requests apply npmrc authentication only to the exact
//! host/path selected by [`NpmConfig`], never forward authorization across a
//! redirect, and retry only transient failures within configured bounds.

use std::cmp;
use std::fmt;
use std::io::{self, Read};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::config::{NetworkConfig, NpmConfig};

const USER_AGENT: &str = concat!("bpm/", env!("CARGO_PKG_VERSION"));
const AUTHORIZATION: &str = "Authorization";
const RETRY_AFTER: &str = "Retry-After";
const RETRY_BODY_DRAIN_LIMIT: usize = 64 * 1024;

/// A pooled, configured HTTP client suitable for cloning between consumers.
#[derive(Clone)]
pub struct HttpClient {
    agent: ureq::Agent,
    config: NpmConfig,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// Build a client from effective npm configuration.
    ///
    /// `ureq`'s default redirect policy is retained, while authorization
    /// propagation is explicitly disabled. This allows public redirects but
    /// prevents a registry credential from reaching another origin.
    pub fn new(config: NpmConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(config.network.fetch_timeout)
            .redirect_auth_headers(ureq::RedirectAuthHeaders::Never)
            .user_agent(USER_AGENT)
            .build();
        Self { agent, config }
    }

    /// Execute a GET request and return its response for string/JSON handling.
    pub fn get(&self, url: &str) -> Result<ureq::Response, HttpError> {
        let display_url = redact_url(url);
        let attempts = self.config.network.retries.saturating_add(1);

        for attempt in 0..attempts {
            let mut request = self.agent.get(url);
            if let Some(token) = self.config.auth_token_for_url(url) {
                request = request.set(AUTHORIZATION, &format!("Bearer {token}"));
            }

            match request.call() {
                Ok(response) => return Ok(response),
                Err(ureq::Error::Status(code, response)) => {
                    let completed = attempt + 1;
                    if is_retryable_status(code) && completed < attempts {
                        let retry_after = retry_after_delay(&response);
                        drain_response_for_retry(response);
                        thread::sleep(retry_delay(&self.config.network, attempt, retry_after));
                        continue;
                    }
                    return Err(HttpError::Status {
                        url: display_url,
                        code,
                        attempts: completed,
                    });
                }
                Err(ureq::Error::Transport(error)) => {
                    let completed = attempt + 1;
                    if is_retryable_transport(error.kind()) && completed < attempts {
                        thread::sleep(retry_delay(&self.config.network, attempt, None));
                        continue;
                    }
                    return Err(HttpError::Transport {
                        url: display_url,
                        kind: format!("{:?}", error.kind()),
                        attempts: completed,
                    });
                }
            }
        }

        unreachable!("the configured attempt count is always at least one")
    }

    /// Execute a GET request and expose its body as a streaming reader.
    pub fn stream(&self, url: &str) -> Result<Box<dyn Read + Send + Sync + 'static>, HttpError> {
        self.get(url).map(ureq::Response::into_reader)
    }

    /// POST a JSON request body and return the response body as bytes.
    pub fn post_json(&self, url: &str, body: &[u8]) -> Result<Vec<u8>, HttpError> {
        self.request_json("POST", url, body, &[])
    }

    /// POST a JSON request with additional headers and return the response body as bytes.
    pub fn post_json_with_headers(
        &self,
        url: &str,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, HttpError> {
        self.request_json("POST", url, body, headers)
    }

    /// PUT a JSON request body and return the response body as bytes.
    pub fn put_json(&self, url: &str, body: &[u8]) -> Result<Vec<u8>, HttpError> {
        self.request_json("PUT", url, body, &[])
    }

    /// PUT a JSON request with additional headers and return the response body as bytes.
    pub fn put_json_with_headers(
        &self,
        url: &str,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, HttpError> {
        self.request_json("PUT", url, body, headers)
    }

    fn request_json(
        &self,
        method: &str,
        url: &str,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, HttpError> {
        let display_url = redact_url(url);
        let attempts = self.config.network.retries.saturating_add(1);
        for attempt in 0..attempts {
            let request = match method {
                "POST" => self.agent.post(url),
                "PUT" => self.agent.put(url),
                _ => unreachable!(),
            }
            .set("Content-Type", "application/json")
            .set("Accept", "application/json");
            let mut request = if let Some(token) = self.config.auth_token_for_url(url) {
                request.set(AUTHORIZATION, &format!("Bearer {token}"))
            } else {
                request
            };
            for (name, value) in headers {
                request = request.set(name, value);
            }
            match request.send_bytes(body) {
                Ok(response) => {
                    let mut out = Vec::new();
                    response.into_reader().read_to_end(&mut out).map_err(|_| {
                        HttpError::Transport {
                            url: display_url.clone(),
                            kind: "response read failed".into(),
                            attempts: attempt + 1,
                        }
                    })?;
                    return Ok(out);
                }
                Err(ureq::Error::Status(code, response))
                    if is_retryable_status(code) && attempt + 1 < attempts =>
                {
                    drain_response_for_retry(response);
                    thread::sleep(retry_delay(&self.config.network, attempt, None));
                }
                Err(ureq::Error::Status(code, _)) => {
                    return Err(HttpError::Status {
                        url: display_url,
                        code,
                        attempts: attempt + 1,
                    })
                }
                Err(ureq::Error::Transport(error))
                    if is_retryable_transport(error.kind()) && attempt + 1 < attempts =>
                {
                    thread::sleep(retry_delay(&self.config.network, attempt, None));
                }
                Err(ureq::Error::Transport(error)) => {
                    return Err(HttpError::Transport {
                        url: display_url,
                        kind: format!("{:?}", error.kind()),
                        attempts: attempt + 1,
                    })
                }
            }
        }
        unreachable!()
    }
}

/// A redacted, actionable HTTP failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    Status {
        url: String,
        code: u16,
        attempts: usize,
    },
    Transport {
        url: String,
        kind: String,
        attempts: usize,
    },
}

impl fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status {
                url,
                code,
                attempts,
            } => write!(
                formatter,
                "HTTP GET {url} returned status {code} after {attempts} attempt(s)"
            ),
            Self::Transport {
                url,
                kind,
                attempts,
            } => write!(
                formatter,
                "HTTP GET {url} failed with transport error {kind} after {attempts} attempt(s)"
            ),
        }
    }
}

impl std::error::Error for HttpError {}

fn is_retryable_status(code: u16) -> bool {
    matches!(code, 408 | 429 | 500..=599)
}

fn is_retryable_transport(kind: ureq::ErrorKind) -> bool {
    use ureq::ErrorKind;

    match kind {
        ErrorKind::ConnectionFailed | ErrorKind::Io | ErrorKind::ProxyConnect => true,
        ErrorKind::InvalidUrl
        | ErrorKind::UnknownScheme
        | ErrorKind::Dns
        | ErrorKind::InsecureRequestHttpsOnly
        | ErrorKind::TooManyRedirects
        | ErrorKind::BadStatus
        | ErrorKind::BadHeader
        | ErrorKind::InvalidProxyUrl
        | ErrorKind::ProxyUnauthorized
        | ErrorKind::HTTP => false,
    }
}

fn retry_after_delay(response: &ureq::Response) -> Option<Duration> {
    response
        .header(RETRY_AFTER)
        .and_then(|value| parse_retry_after_at(value, SystemTime::now()))
}

fn parse_retry_after_at(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }

    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or(Duration::ZERO))
}

fn drain_response_for_retry(response: ureq::Response) {
    let mut body = response.into_reader();
    let _ = drain_reader_for_retry(&mut body);
}

/// Drain a retry response only while it remains small enough to pool safely.
///
/// Reading one byte beyond the limit distinguishes a complete 64 KiB body
/// from an oversized body without allowing unbounded work. Dropping an
/// oversized reader leaves bytes unread, causing ureq to close the connection.
fn drain_reader_for_retry(reader: &mut dyn Read) -> io::Result<bool> {
    let limit = u64::try_from(RETRY_BODY_DRAIN_LIMIT).expect("drain limit fits in u64");
    let mut bounded = reader.take(limit + 1);
    let consumed = io::copy(&mut bounded, &mut io::sink())?;
    Ok(consumed <= limit)
}

fn retry_delay(network: &NetworkConfig, retry: usize, retry_after: Option<Duration>) -> Duration {
    let exponent = u32::try_from(retry).unwrap_or(u32::MAX);
    let factor = network
        .retry_factor
        .checked_pow(exponent)
        .unwrap_or(u32::MAX);
    let exponential = network
        .retry_min_timeout
        .checked_mul(factor)
        .unwrap_or(network.retry_max_timeout);
    let requested = cmp::max(exponential, retry_after.unwrap_or_default());
    cmp::min(requested, network.retry_max_timeout)
}

/// Remove user information, query, and fragment from URLs included in errors.
fn redact_url(url: &str) -> String {
    let without_suffix = url.split(['?', '#']).next().unwrap_or(url);
    let Some((scheme, remainder)) = without_suffix.split_once("://") else {
        return "<invalid-url>".to_string();
    };
    let authority_end = remainder.find('/').unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    let path = &remainder[authority_end..];
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!("{}://{}{}", scheme.to_ascii_lowercase(), host, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::time::UNIX_EPOCH;

    #[test]
    fn transport_retry_policy_exhaustively_whitelists_transient_kinds() {
        use ureq::ErrorKind::*;

        let cases = [
            (InvalidUrl, false),
            (UnknownScheme, false),
            (Dns, false),
            (InsecureRequestHttpsOnly, false),
            (ConnectionFailed, true),
            (TooManyRedirects, false),
            (BadStatus, false),
            (BadHeader, false),
            (Io, true),
            (InvalidProxyUrl, false),
            (ProxyConnect, true),
            (ProxyUnauthorized, false),
            (HTTP, false),
        ];

        assert_eq!(cases.len(), 13);
        for (kind, expected) in cases {
            assert_eq!(is_retryable_transport(kind), expected, "{kind:?}");
        }
    }

    #[test]
    fn retry_after_supports_delay_seconds_and_http_dates() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(
            parse_retry_after_at(" 17 ", now),
            Some(Duration::from_secs(17))
        );

        let future = now + Duration::from_secs(23);
        assert_eq!(
            parse_retry_after_at(&httpdate::fmt_http_date(future), now),
            Some(Duration::from_secs(23))
        );
        assert_eq!(
            parse_retry_after_at(&httpdate::fmt_http_date(now - Duration::from_secs(23)), now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after_at("not-a-date", now), None);
    }

    #[test]
    fn retry_body_drain_is_bounded_and_detects_reusable_bodies() {
        for size in [0, RETRY_BODY_DRAIN_LIMIT - 1, RETRY_BODY_DRAIN_LIMIT] {
            let mut reader = Cursor::new(vec![0_u8; size]);
            assert!(drain_reader_for_retry(&mut reader).unwrap(), "size {size}");
            assert_eq!(reader.position(), size as u64);
        }

        let mut oversized = Cursor::new(vec![0_u8; RETRY_BODY_DRAIN_LIMIT + 100]);
        assert!(!drain_reader_for_retry(&mut oversized).unwrap());
        assert_eq!(oversized.position(), (RETRY_BODY_DRAIN_LIMIT + 1) as u64);
    }

    #[test]
    fn redacts_credentials_query_and_fragment() {
        assert_eq!(
            redact_url("https://user:secret@example.test/pkg?token=secret#private"),
            "https://example.test/pkg"
        );
        assert_eq!(redact_url("not a url"), "<invalid-url>");
    }

    #[test]
    fn retry_delay_is_exponential_and_bounded() {
        let network = NetworkConfig {
            retries: 4,
            retry_factor: 2,
            retry_min_timeout: Duration::from_millis(10),
            retry_max_timeout: Duration::from_millis(50),
            fetch_timeout: Duration::from_secs(1),
        };
        assert_eq!(retry_delay(&network, 0, None), Duration::from_millis(10));
        assert_eq!(retry_delay(&network, 2, None), Duration::from_millis(40));
        assert_eq!(retry_delay(&network, 9, None), Duration::from_millis(50));
        assert_eq!(
            retry_delay(&network, 0, Some(Duration::from_secs(2))),
            Duration::from_millis(50)
        );
    }
}
