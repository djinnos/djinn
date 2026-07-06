//! Session liveness classification.
//!
//! A running session is "wedged" when its in-process slot is still alive but
//! the LLM has stopped producing progress — either it never returned a single
//! token (the HTTP call is hung before any byte) or no new `session_messages`
//! row has been appended for long enough that the turn cannot still be in
//! flight legitimately.
//!
//! This module exposes a pure classifier so the same rules are used by
//! `session_active` (observation), `board_reconcile` (active healing on
//! demand), and the coordinator's periodic stall sweep.

use time::OffsetDateTime;
use time::format_description::well_known::Iso8601;

/// Thresholds for [`classify_session_progress`].
///
/// `zero_token_threshold_secs` applies when the session has produced zero
/// tokens in either direction — no LLM response ever arrived, so any
/// elapsed wall-clock time past the threshold is unambiguously wedged.
///
/// `general_threshold_secs` applies once at least one token has flowed:
/// it is measured against the last `session_messages.created_at`, falling
/// back to `started_at`. Should be larger than typical LLM turn latency.
#[derive(Debug, Clone, Copy)]
pub struct LivenessConfig {
    pub zero_token_threshold_secs: i64,
    pub general_threshold_secs: i64,
}

impl LivenessConfig {
    /// Observation defaults used by `session_active` (and on-demand
    /// `board_reconcile`). The background coordinator sweep applies the
    /// zero-token short-circuit but keeps its own (more lenient) idle
    /// threshold for sessions that have produced tokens.
    ///
    /// `zero_token_threshold_secs` was lowered from 180s to 90s so a
    /// non-reasoning zero-token session fails over within the same budget as
    /// the provider-side first-event (TTFT) guard. A per-model reasoning floor
    /// (~600s) raises this at the provider transport layer for known reasoning
    /// families; this session-level default remains the floor for all other
    /// models.
    pub const OBSERVATION: Self = Self {
        zero_token_threshold_secs: 90,
        general_threshold_secs: 600,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressVerdict {
    Live,
    Wedged { idle_secs: i64, zero_tokens: bool },
}

/// Classify a session's progress based on its tokens and the timestamp of
/// its most recent activity.
///
/// `last_message_at` is the ISO-8601 timestamp of the newest row in
/// `session_messages` for this session (caller's responsibility to query).
/// When `None`, the classifier falls back to `started_at`.
///
/// `now` is injected so callers can use a single observation time across a
/// batch of sessions, and so tests can pin the clock.
///
/// If either timestamp fails to parse, the verdict is `Live` — failing
/// open is safer than reaping a session because we couldn't read its
/// clock.
pub fn classify_session_progress(
    started_at: &str,
    last_message_at: Option<&str>,
    tokens_in: i64,
    tokens_out: i64,
    now: OffsetDateTime,
    config: &LivenessConfig,
) -> ProgressVerdict {
    let reference_iso = last_message_at.unwrap_or(started_at);
    let Some(elapsed) = elapsed_secs_since(reference_iso, now) else {
        return ProgressVerdict::Live;
    };

    let zero_tokens = tokens_in == 0 && tokens_out == 0;
    let threshold = if zero_tokens {
        config.zero_token_threshold_secs
    } else {
        config.general_threshold_secs
    };

    if elapsed >= threshold {
        ProgressVerdict::Wedged {
            idle_secs: elapsed,
            zero_tokens,
        }
    } else {
        ProgressVerdict::Live
    }
}

/// Parse an ISO-8601 datetime string from the DB (e.g.
/// `"2026-03-27T13:52:47.231Z"` or `"2026-03-27 13:52:47"`) and return
/// seconds elapsed between that instant and `now`. Clamped to `>= 0` so
/// clock skew never produces a negative idle reading.
pub fn elapsed_secs_since(iso: &str, now: OffsetDateTime) -> Option<i64> {
    let parsed = OffsetDateTime::parse(iso, &Iso8601::DEFAULT)
        .ok()
        .or_else(|| {
            let fmt =
                time::format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second]")
                    .ok()?;
            let primitive = time::PrimitiveDateTime::parse(iso, &fmt).ok()?;
            Some(primitive.assume_utc())
        })?;

    let elapsed = (now - parsed).whole_seconds();
    Some(elapsed.max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(iso: &str) -> OffsetDateTime {
        OffsetDateTime::parse(iso, &Iso8601::DEFAULT).unwrap()
    }

    #[test]
    fn zero_tokens_wedged_at_threshold() {
        let now = t("2026-05-22T12:43:00Z");
        let started = "2026-05-22T12:39:30Z"; // 210s ago, >= 90s
        let v = classify_session_progress(started, None, 0, 0, now, &LivenessConfig::OBSERVATION);
        match v {
            ProgressVerdict::Wedged {
                idle_secs,
                zero_tokens,
            } => {
                assert!(zero_tokens);
                assert_eq!(idle_secs, 210);
            }
            ProgressVerdict::Live => panic!("expected wedged"),
        }
    }

    #[test]
    fn zero_tokens_live_below_threshold() {
        let now = t("2026-05-22T12:43:00Z");
        let started = "2026-05-22T12:42:00Z"; // 60s ago, < 90s
        let v = classify_session_progress(started, None, 0, 0, now, &LivenessConfig::OBSERVATION);
        assert_eq!(v, ProgressVerdict::Live);
    }

    #[test]
    fn with_tokens_uses_last_message_and_general_threshold() {
        let now = t("2026-05-22T12:43:00Z");
        let started = "2026-05-22T11:00:00Z"; // ages ago
        let last_msg = "2026-05-22T12:31:00Z"; // 720s ago, >= 600s
        let v = classify_session_progress(
            started,
            Some(last_msg),
            500,
            300,
            now,
            &LivenessConfig::OBSERVATION,
        );
        match v {
            ProgressVerdict::Wedged {
                idle_secs,
                zero_tokens,
            } => {
                assert!(!zero_tokens);
                assert_eq!(idle_secs, 720);
            }
            ProgressVerdict::Live => panic!("expected wedged"),
        }
    }

    #[test]
    fn with_tokens_live_when_recent_message() {
        let now = t("2026-05-22T12:43:00Z");
        let started = "2026-05-22T11:00:00Z"; // ages ago — would wedge if used
        let last_msg = "2026-05-22T12:42:00Z"; // 60s ago
        let v = classify_session_progress(
            started,
            Some(last_msg),
            500,
            300,
            now,
            &LivenessConfig::OBSERVATION,
        );
        assert_eq!(v, ProgressVerdict::Live);
    }

    #[test]
    fn unparseable_timestamp_is_live() {
        let now = t("2026-05-22T12:43:00Z");
        let v = classify_session_progress("garbage", None, 0, 0, now, &LivenessConfig::OBSERVATION);
        assert_eq!(v, ProgressVerdict::Live);
    }

    #[test]
    fn space_separated_timestamp_parses() {
        let now = t("2026-05-22T12:43:00Z");
        let started = "2026-05-22 12:39:30"; // SQLite-style, 210s ago
        let v = classify_session_progress(started, None, 0, 0, now, &LivenessConfig::OBSERVATION);
        assert!(matches!(v, ProgressVerdict::Wedged { .. }));
    }

    #[test]
    fn negative_elapsed_clamped_to_zero() {
        let now = t("2026-05-22T12:00:00Z");
        let started = "2026-05-22T12:05:00Z"; // future
        let v = classify_session_progress(started, None, 0, 0, now, &LivenessConfig::OBSERVATION);
        assert_eq!(v, ProgressVerdict::Live);
    }

    #[test]
    fn zero_token_observation_threshold_is_90s() {
        // The non-reasoning zero-token failover budget was lowered from 180s
        // to 90s so it matches the provider-side first-event (TTFT) guard.
        assert_eq!(
            LivenessConfig::OBSERVATION.zero_token_threshold_secs,
            90,
            "non-reasoning zero-token budget must be no more than 90s"
        );
    }

    #[test]
    fn zero_tokens_wedged_exactly_at_90s_boundary() {
        let now = t("2026-05-22T12:43:00Z");
        let started = "2026-05-22T12:41:30Z"; // exactly 90s ago
        let v = classify_session_progress(started, None, 0, 0, now, &LivenessConfig::OBSERVATION);
        assert!(
            matches!(
                v,
                ProgressVerdict::Wedged {
                    zero_tokens: true,
                    ..
                }
            ),
            "exactly 90s zero-token should be wedged"
        );
    }

    #[test]
    fn zero_tokens_live_just_below_90s_boundary() {
        let now = t("2026-05-22T12:43:00Z");
        let started = "2026-05-22T12:41:31Z"; // 89s ago
        let v = classify_session_progress(started, None, 0, 0, now, &LivenessConfig::OBSERVATION);
        assert_eq!(v, ProgressVerdict::Live);
    }
}
