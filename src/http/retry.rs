//! Transport-independent retry policy shared by the blocking HTTP client and
//! the async resolver.
//!
//! All npm-compatible retry decisions (which statuses and transport errors
//! retry, `Retry-After` parsing, bounded exponential backoff, and the retry
//! body drain limit) live here so the blocking and async transports cannot
//! drift. Blocking `Response` draining stays in `http::mod`; async draining
//! stays in the async resolver, but both bound reads by
//! [`RETRY_BODY_DRAIN_LIMIT`].

use std::cmp;
use std::time::{Duration, SystemTime};

use crate::config::NetworkConfig;

/// Maximum bytes to drain from a retryable-status response body before giving
/// up and dropping the connection. Shared by blocking and async retry paths.
pub(crate) const RETRY_BODY_DRAIN_LIMIT: usize = 64 * 1024;

/// Whether an HTTP status code is retryable per npm policy.
///
/// Only `408 Request Timeout`, `429 Too Many Requests`, and `5xx` server
/// errors retry; ordinary `4xx` (including authentication failures) and
/// successful responses do not.
pub(crate) fn is_retryable_status(code: u16) -> bool {
    matches!(code, 408 | 429 | 500..=599)
}

/// Coarse classification of a reqwest transport failure, for retry decisions.
/// `reqwest::Error` is shared by the blocking and async clients, so this
/// classification applies to both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportKind {
    /// Connection establishment failure (refused, reset, DNS, TLS handshake).
    Connect,
    /// Request or response timeout.
    Timeout,
    /// Builder/URL error — never retried.
    Builder,
    /// Request construction error — never retried.
    Request,
    /// Body transfer error.
    Body,
    /// Response decode error.
    Decode,
    /// Redirect policy error (too many redirects).
    Redirect,
    /// Anything else.
    Other,
}

impl TransportKind {
    pub(crate) fn from_error(error: &reqwest::Error) -> Self {
        if error.is_connect() {
            Self::Connect
        } else if error.is_timeout() {
            Self::Timeout
        } else if error.is_builder() {
            Self::Builder
        } else if error.is_request() {
            Self::Request
        } else if error.is_body() {
            Self::Body
        } else if error.is_decode() {
            Self::Decode
        } else if error.is_redirect() {
            Self::Redirect
        } else {
            Self::Other
        }
    }

    pub(crate) fn is_retryable(self) -> bool {
        matches!(self, Self::Connect | Self::Timeout)
    }
}

/// Classify a reqwest transport error and decide whether it should be retried.
/// Only connection-establishment and timeout failures retry; builder, request,
/// body, decode, and redirect errors do not.
pub(crate) fn is_retryable_transport(error: &reqwest::Error) -> bool {
    TransportKind::from_error(error).is_retryable()
}

/// Human-facing label for a transport failure kind.
pub(crate) fn transport_kind(error: &reqwest::Error) -> String {
    let kind = TransportKind::from_error(error);
    match kind {
        TransportKind::Connect => "connection failed",
        TransportKind::Timeout => "timeout",
        TransportKind::Builder => "invalid request",
        TransportKind::Request => "request failed",
        TransportKind::Body => "body transfer failed",
        TransportKind::Decode => "response decode failed",
        TransportKind::Redirect => "too many redirects",
        TransportKind::Other => "transport failed",
    }
    .to_string()
}

/// Parse a `Retry-After` header value (`delta-seconds` or an HTTP-date) into a
/// delay relative to `now`. Past HTTP-dates yield zero (retry immediately).
pub(crate) fn parse_retry_after_at(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Bounded exponential backoff for one retry, honoring an optional
/// `Retry-After` delay. Grows as `retry_min_timeout * retry_factor ^ retry`,
/// clamped to `retry_max_timeout`, and never below the server-requested delay.
pub(crate) fn retry_delay(
    network: &NetworkConfig,
    retry: usize,
    retry_after: Option<Duration>,
) -> Duration {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_policy_retries_408_429_and_5xx_only() {
        assert!(is_retryable_status(408));
        assert!(is_retryable_status(429));
        for code in 500..=599 {
            assert!(is_retryable_status(code), "{code}");
        }
        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(304));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(403));
        assert!(!is_retryable_status(404));
    }

    #[test]
    fn transport_retry_policy_retries_only_connect_and_timeout_kinds() {
        // Every TransportKind is enumerated so adding a new variant forces a
        // deliberate decision about its retry behavior.
        let cases = [
            (TransportKind::Connect, true),
            (TransportKind::Timeout, true),
            (TransportKind::Builder, false),
            (TransportKind::Request, false),
            (TransportKind::Body, false),
            (TransportKind::Decode, false),
            (TransportKind::Redirect, false),
            (TransportKind::Other, false),
        ];
        assert_eq!(cases.len(), 8);
        for (kind, expected) in cases {
            assert_eq!(kind.is_retryable(), expected, "{kind:?}");
        }
    }

    #[test]
    fn retry_after_supports_delay_seconds_and_http_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
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
            Duration::ZERO.into()
        );
        assert_eq!(parse_retry_after_at("not-a-date", now), None);
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
