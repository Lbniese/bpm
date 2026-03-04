//! Parsing and deterministic selection rules for garbage collection.
//!
//! This module deliberately has no filesystem or metadata dependencies. The
//! collector supplies a stable snapshot and applies the returned selection.

use std::error::Error;
use std::fmt;
use std::time::Duration;

const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;
const SECONDS_PER_WEEK: u64 = 7 * SECONDS_PER_DAY;

/// Conservative default age below which unreferenced objects are retained.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(30 * SECONDS_PER_DAY);

/// Errors produced while parsing or evaluating a collection policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyError {
    input: String,
    reason: &'static str,
}

impl PolicyError {
    fn new(input: impl Into<String>, reason: &'static str) -> Self {
        Self {
            input: input.into(),
            reason,
        }
    }
}

impl fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid garbage-collection value `{}`: {}",
            self.input, self.reason
        )
    }
}

impl Error for PolicyError {}

/// Parse an age such as `30d` using the strict GC command grammar.
pub fn parse_duration(input: &str) -> Result<Duration, PolicyError> {
    let (digits, suffix) = split_number_and_suffix(input)?;
    let unit_seconds = match suffix {
        "s" => 1,
        "m" => SECONDS_PER_MINUTE,
        "h" => SECONDS_PER_HOUR,
        "d" => SECONDS_PER_DAY,
        "w" => SECONDS_PER_WEEK,
        _ => {
            return Err(PolicyError::new(
                input,
                "expected a positive integer followed by s, m, h, d, or w (for example `30d`)",
            ));
        }
    };
    let value = parse_positive_integer(input, digits)?;
    let seconds = value
        .checked_mul(unit_seconds)
        .ok_or_else(|| PolicyError::new(input, "duration is too large to represent safely"))?;
    Ok(Duration::from_secs(seconds))
}

/// Parse a logical-byte limit such as `50GB` using decimal SI units.
pub fn parse_byte_size(input: &str) -> Result<u64, PolicyError> {
    let (digits, suffix) = split_number_and_suffix(input)?;
    let multiplier = match suffix {
        "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        _ => {
            return Err(PolicyError::new(
                input,
                "expected a positive integer followed by B, KB, MB, GB, or TB (for example `50GB`)",
            ));
        }
    };
    let value = parse_positive_integer(input, digits)?;
    let bytes = value
        .checked_mul(multiplier)
        .ok_or_else(|| PolicyError::new(input, "size is too large to represent safely"))?;
    if bytes > i64::MAX as u64 {
        return Err(PolicyError::new(
            input,
            "size exceeds the metadata database limit of 9223372036854775807 bytes",
        ));
    }
    Ok(bytes)
}

fn split_number_and_suffix(input: &str) -> Result<(&str, &str), PolicyError> {
    if !input.is_ascii() {
        return Err(PolicyError::new(input, "only ASCII input is accepted"));
    }
    let digit_count = input.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 || digit_count == input.len() {
        return Err(PolicyError::new(
            input,
            "a positive integer and unit suffix are required",
        ));
    }
    Ok(input.split_at(digit_count))
}

fn parse_positive_integer(input: &str, digits: &str) -> Result<u64, PolicyError> {
    let value = digits
        .parse::<u64>()
        .map_err(|_| PolicyError::new(input, "numeric value is too large"))?;
    if value == 0 {
        return Err(PolicyError::new(input, "value must be greater than zero"));
    }
    Ok(value)
}

/// Dependency-safe tie breaker used after effective access time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeletionRank {
    Plan,
    Graph,
    Derived,
    Image,
    Artifact,
}

/// An object considered by the pure policy evaluator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyCandidate {
    pub effective_access_ms: i64,
    pub deletion_rank: DeletionRank,
    pub object_id: String,
    pub size_bytes: u64,
    /// Marked objects and objects covered by active leases are protected.
    pub protected: bool,
}

/// User-selected collection limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcPolicy {
    pub grace: Duration,
    pub max_size_bytes: Option<u64>,
}

impl Default for GcPolicy {
    fn default() -> Self {
        Self {
            grace: DEFAULT_GRACE,
            max_size_bytes: None,
        }
    }
}

/// Deterministic result of policy evaluation, before any deletion occurs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyEvaluation {
    pub selected: Vec<PolicyCandidate>,
    pub selected_bytes: u64,
    pub remaining_bytes: u64,
    pub cap_reachable: bool,
}

impl GcPolicy {
    /// Select unprotected, grace-eligible objects in stable oldest-first order.
    ///
    /// `total_managed_bytes` includes protected and recent objects. A size cap
    /// never overrides either protection or the grace period.
    pub fn evaluate(
        &self,
        now_ms: i64,
        total_managed_bytes: u64,
        candidates: &[PolicyCandidate],
    ) -> Result<PolicyEvaluation, PolicyError> {
        let grace_ms = i64::try_from(self.grace.as_millis()).map_err(|_| {
            PolicyError::new(
                format!("{}ms", self.grace.as_millis()),
                "grace period is too large to compare with timestamps",
            )
        })?;
        let cutoff = now_ms.checked_sub(grace_ms).ok_or_else(|| {
            PolicyError::new(
                now_ms.to_string(),
                "current time is earlier than the configured grace period",
            )
        })?;

        let mut eligible: Vec<_> = candidates
            .iter()
            .filter(|candidate| !candidate.protected && candidate.effective_access_ms <= cutoff)
            .cloned()
            .collect();
        eligible.sort_by(|left, right| {
            (
                left.effective_access_ms,
                left.deletion_rank,
                left.object_id.as_str(),
            )
                .cmp(&(
                    right.effective_access_ms,
                    right.deletion_rank,
                    right.object_id.as_str(),
                ))
        });

        let bytes_to_reclaim = self
            .max_size_bytes
            .map(|cap| total_managed_bytes.saturating_sub(cap));
        let mut selected = Vec::new();
        let mut selected_bytes = 0_u64;
        for candidate in eligible {
            if bytes_to_reclaim.is_some_and(|required| selected_bytes >= required) {
                break;
            }
            selected_bytes = selected_bytes
                .checked_add(candidate.size_bytes)
                .ok_or_else(|| {
                    PolicyError::new(
                        candidate.object_id.clone(),
                        "selected object sizes overflow the supported byte count",
                    )
                })?;
            selected.push(candidate);
        }

        let remaining_bytes = total_managed_bytes.saturating_sub(selected_bytes);
        let cap_reachable = self.max_size_bytes.is_none_or(|cap| remaining_bytes <= cap);
        Ok(PolicyEvaluation {
            selected,
            selected_bytes,
            remaining_bytes,
            cap_reachable,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(
        effective_access_ms: i64,
        deletion_rank: DeletionRank,
        object_id: &str,
        size_bytes: u64,
    ) -> PolicyCandidate {
        PolicyCandidate {
            effective_access_ms,
            deletion_rank,
            object_id: object_id.to_owned(),
            size_bytes,
            protected: false,
        }
    }

    #[test]
    fn parses_strict_duration_units() {
        assert_eq!(parse_duration("1s").unwrap(), Duration::from_secs(1));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("3h").unwrap(), Duration::from_secs(10_800));
        assert_eq!(parse_duration("30d").unwrap(), DEFAULT_GRACE);
        assert_eq!(
            parse_duration("2w").unwrap(),
            Duration::from_secs(1_209_600)
        );
    }

    #[test]
    fn rejects_non_strict_durations() {
        for invalid in [
            "", "0d", "30", "d", " 30d", "30d ", "+30d", "-30d", "1.5d", "1d2h", "30D", "３０d",
        ] {
            assert!(parse_duration(invalid).is_err(), "accepted {invalid:?}");
        }
        assert!(parse_duration("18446744073709551615w").is_err());
    }

    #[test]
    fn parses_decimal_byte_sizes() {
        assert_eq!(parse_byte_size("1B").unwrap(), 1);
        assert_eq!(parse_byte_size("2KB").unwrap(), 2_000);
        assert_eq!(parse_byte_size("3MB").unwrap(), 3_000_000);
        assert_eq!(parse_byte_size("50GB").unwrap(), 50_000_000_000);
        assert_eq!(parse_byte_size("4TB").unwrap(), 4_000_000_000_000);
    }

    #[test]
    fn rejects_non_strict_or_oversized_byte_sizes() {
        for invalid in [
            "", "0B", "50", "GB", " 50GB", "50GB ", "+50GB", "-50GB", "1.5GB", "1GiB", "1gb",
            "１GB",
        ] {
            assert!(parse_byte_size(invalid).is_err(), "accepted {invalid:?}");
        }
        assert_eq!(
            parse_byte_size("9223372036854775807B").unwrap(),
            i64::MAX as u64
        );
        assert!(parse_byte_size("9223372036854775808B").is_err());
        assert!(parse_byte_size("18446744073709551615TB").is_err());
    }

    #[test]
    fn no_cap_selects_every_unprotected_object_outside_grace() {
        let now = 40 * SECONDS_PER_DAY as i64 * 1_000;
        let recent = now - DEFAULT_GRACE.as_millis() as i64 + 1;
        let old = now - DEFAULT_GRACE.as_millis() as i64;
        let mut protected = candidate(old - 1, DeletionRank::Plan, "protected", 7);
        protected.protected = true;
        let evaluation = GcPolicy::default()
            .evaluate(
                now,
                30,
                &[
                    candidate(recent, DeletionRank::Artifact, "recent", 11),
                    protected,
                    candidate(old, DeletionRank::Artifact, "old", 12),
                ],
            )
            .unwrap();
        assert_eq!(evaluation.selected.len(), 1);
        assert_eq!(evaluation.selected[0].object_id, "old");
        assert_eq!(evaluation.selected_bytes, 12);
        assert_eq!(evaluation.remaining_bytes, 18);
        assert!(evaluation.cap_reachable);
    }

    #[test]
    fn size_cap_selects_oldest_until_target_is_met() {
        let policy = GcPolicy {
            grace: Duration::from_secs(1),
            max_size_bytes: Some(10),
        };
        let evaluation = policy
            .evaluate(
                10_000,
                25,
                &[
                    candidate(2_000, DeletionRank::Artifact, "newer", 10),
                    candidate(1_000, DeletionRank::Artifact, "oldest", 6),
                ],
            )
            .unwrap();
        assert_eq!(
            evaluation
                .selected
                .iter()
                .map(|item| item.object_id.as_str())
                .collect::<Vec<_>>(),
            ["oldest", "newer"]
        );
        assert_eq!(evaluation.remaining_bytes, 9);
        assert!(evaluation.cap_reachable);
    }

    #[test]
    fn reports_when_grace_or_protection_makes_cap_unreachable() {
        let mut protected = candidate(1_000, DeletionRank::Artifact, "live", 20);
        protected.protected = true;
        let policy = GcPolicy {
            grace: Duration::from_secs(1),
            max_size_bytes: Some(10),
        };
        let evaluation = policy.evaluate(10_000, 25, &[protected]).unwrap();
        assert!(evaluation.selected.is_empty());
        assert_eq!(evaluation.remaining_bytes, 25);
        assert!(!evaluation.cap_reachable);
    }

    #[test]
    fn candidate_order_uses_rank_then_identifier_for_equal_times() {
        let policy = GcPolicy {
            grace: Duration::from_secs(1),
            max_size_bytes: None,
        };
        let evaluation = policy
            .evaluate(
                10_000,
                3,
                &[
                    candidate(1_000, DeletionRank::Artifact, "a", 1),
                    candidate(1_000, DeletionRank::Plan, "z", 1),
                    candidate(1_000, DeletionRank::Plan, "a", 1),
                ],
            )
            .unwrap();
        assert_eq!(
            evaluation
                .selected
                .iter()
                .map(|item| (item.deletion_rank, item.object_id.as_str()))
                .collect::<Vec<_>>(),
            [
                (DeletionRank::Plan, "a"),
                (DeletionRank::Plan, "z"),
                (DeletionRank::Artifact, "a"),
            ]
        );
    }

    #[test]
    fn cap_above_current_size_selects_nothing() {
        let policy = GcPolicy {
            grace: Duration::from_secs(1),
            max_size_bytes: Some(100),
        };
        let evaluation = policy
            .evaluate(
                10_000,
                10,
                &[candidate(1_000, DeletionRank::Artifact, "old", 10)],
            )
            .unwrap();
        assert!(evaluation.selected.is_empty());
        assert_eq!(evaluation.remaining_bytes, 10);
        assert!(evaluation.cap_reachable);
    }

    #[test]
    fn evaluation_rejects_unrepresentable_grace_period() {
        let policy = GcPolicy {
            grace: Duration::MAX,
            max_size_bytes: None,
        };
        let error = policy.evaluate(0, 0, &[]).unwrap_err();
        assert!(error.to_string().contains("too large"));
    }
}
