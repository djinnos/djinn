//! Session liveness and runtime-cap classification.
//!
//! Two complementary pure classifiers live here so reconcile / observation
//! code can use the same rules for both failure modes:
//!
//! - [`classify_session_progress`] reports a wedged session — its in-process
//!   slot is still alive but the LLM has stopped producing progress, either
//!   because it never returned a single token (the HTTP call is hung before
//!   any byte) or because no new `session_messages` row has been appended for
//!   long enough that the turn cannot still be in flight legitimately.
//!
//! - [`classify_runtime_cap`] reports a model-specific wall-clock runtime
//!   cap breach — independent of the wedged thresholds above — so that
//!   long-grinding sessions on providers we know get stuck (today: the
//!   `zai` / `zai-coding-plan` `glm-5.2` workers) become eligible for reroute
//!   before they consume the soft deadline.
//!
//! Both modules expose pure classifiers so the same rules are used by
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
            let fmt = time::format_description::parse_borrowed::<3>(
                "[year]-[month]-[day] [hour]:[minute]:[second]",
            )
            .ok()?;
            let primitive = time::PrimitiveDateTime::parse(iso, &fmt).ok()?;
            Some(primitive.assume_utc())
        })?;

    let elapsed = (now - parsed).whole_seconds();
    Some(elapsed.max(0))
}

/// A model-specific worker wall-clock runtime cap.
///
/// Used by [`classify_runtime_cap`] to flag long-grinding worker sessions on
/// providers known to wedge (today: the `zai` / `zai-coding-plan` `glm-5.2`
/// workers) so they can be rerouted before they consume the soft deadline.
///
/// A cap hit is a reroute trigger, **not** a task-quality strike: the
/// follow-up wiring should surface it as infrastructure / runtime policy
/// rather than counting it against the task.
#[derive(Debug, Clone)]
pub struct RuntimeCapConfig {
    /// Wall-clock cap in seconds (approximately 90 minutes for the default).
    pub worker_runtime_cap_secs: i64,
    /// `(provider_id, model_name)` pairs that the cap applies to. The
    /// `model_id` field on `SessionRecord` is `provider_id/model_name`, so
    /// matching is done on the split halves — exact equality on both sides,
    /// not substring or prefix.
    pub capped_models: Vec<(String, String)>,
    /// Agent type the cap applies to. Today: only `"worker"`. Reviewer and
    /// planner sessions are deliberately excluded.
    pub worker_agent_type: String,
}

impl RuntimeCapConfig {
    /// Default runtime-cap config used by reconcile / observation paths for
    /// epic `5wxi`: an approximately 90-minute wall-clock cap (5,400
    /// seconds) on worker sessions running the `zai` and `zai-coding-plan`
    /// `glm-5.2` models.
    pub fn default_config() -> Self {
        Self {
            worker_runtime_cap_secs: 5_400,
            capped_models: Self::DEFAULT_CAPPED_MODELS
                .iter()
                .map(|(p, m)| (p.to_string(), m.to_string()))
                .collect(),
            worker_agent_type: Self::DEFAULT_WORKER_AGENT_TYPE.to_string(),
        }
    }

    /// `(provider_id, model_name)` pairs covered by [`Self::default_config`].
    pub const DEFAULT_CAPPED_MODELS: &'static [(&'static str, &'static str)] =
        &[("zai-coding-plan", "glm-5.2"), ("zai", "glm-5.2")];

    /// Default worker agent-type identifier, matching the value written by
    /// `djinn-runtime::RoleKind::Worker` (see `RoleKind::as_str`).
    pub const DEFAULT_WORKER_AGENT_TYPE: &'static str = "worker";
}

/// Verdict returned by [`classify_runtime_cap`].
///
/// Distinct from [`ProgressVerdict`] so the wiring layer can branch on cap
/// hits without conflating them with wedged / stale failures — a cap hit is
/// a reroute trigger, not a task-quality strike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeCapVerdict {
    /// Session is below the runtime cap, or not on a capped model, or not
    /// on a worker role — no reroute needed.
    Live,
    /// Session has been running at or beyond the configured wall-clock cap
    /// for a capped `(provider_id, model_name)` worker. `elapsed_secs` is
    /// the wall-clock seconds since `started_at` (clamped to `>= 0`).
    RuntimeCap {
        elapsed_secs: i64,
        cap_secs: i64,
        provider_id: String,
        model_name: String,
    },
}

/// Classify a worker session's runtime against the configured wall-clock cap.
///
/// `started_at` is the ISO-8601 timestamp from `sessions.started_at`.
/// `now` is injected so callers can use a single observation time across a
/// batch and so tests can pin the clock. `role` should be the value stored
/// in `sessions.agent_type` (e.g. `"worker"`, `"reviewer"`, `"planner"`).
/// `model_id` should be the value stored in `sessions.model_id`, formatted
/// as `"provider_id/model_name"` (e.g. `"zai-coding-plan/glm-5.2"`,
/// `"zai/glm-5.2"`, `"xiaomi-token-plan-sgp/mimo-v2.5-pro"`).
///
/// The verdict is [`RuntimeCapVerdict::Live`] (no reroute) when:
///
/// - `role` is not the configured worker agent type (today: anything that
///   isn't `"worker"`), so reviewer/planner sessions are exempt by design.
/// - `model_id` does not exactly match any `(provider_id, model_name)` pair
///   in [`RuntimeCapConfig::capped_models`], so other models are exempt.
/// - The session has not yet reached `worker_runtime_cap_secs` since
///   `started_at`.
/// - `started_at` fails to parse (failing open — same fail-safe posture as
///   [`classify_session_progress`]).
///
/// A `RuntimeCap` verdict is the reroute trigger consumed by follow-up
/// wiring; it must remain distinguishable from a `Wedged` verdict so the
/// wiring does not count cap hits as task-quality strikes.
pub fn classify_runtime_cap(
    role: &str,
    model_id: &str,
    started_at: &str,
    now: OffsetDateTime,
    config: &RuntimeCapConfig,
) -> RuntimeCapVerdict {
    if role != config.worker_agent_type {
        return RuntimeCapVerdict::Live;
    }

    let Some((provider_id, model_name)) = model_id.split_once('/') else {
        return RuntimeCapVerdict::Live;
    };

    let matched = config
        .capped_models
        .iter()
        .find(|(p, m)| p == provider_id && m == model_name);
    let Some((cap_provider, cap_model)) = matched else {
        return RuntimeCapVerdict::Live;
    };

    let Some(elapsed) = elapsed_secs_since(started_at, now) else {
        return RuntimeCapVerdict::Live;
    };

    if elapsed >= config.worker_runtime_cap_secs {
        RuntimeCapVerdict::RuntimeCap {
            elapsed_secs: elapsed,
            cap_secs: config.worker_runtime_cap_secs,
            provider_id: cap_provider.clone(),
            model_name: cap_model.clone(),
        }
    } else {
        RuntimeCapVerdict::Live
    }
}

/// Describes why a live session should be interrupted by reconcile /
/// observation code. Carries enough context for the wiring layer to log
/// a reason that distinguishes infrastructure/runtime-policy events from
/// task-quality failures.
///
/// Returned by [`classify_session_interruption`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInterruptReason {
    /// The slot is alive but the LLM has stopped producing progress (see
    /// [`classify_session_progress`]).  This is an ordinary wedged/stale
    /// failure.
    Wedged { idle_secs: i64, zero_tokens: bool },
    /// The session is on a capped model/role and has exceeded the
    /// wall-clock runtime cap (see [`classify_runtime_cap`]).  This is
    /// an infrastructure / runtime-policy reroute — **not** a task-quality
    /// strike.
    RuntimeCap {
        elapsed_secs: i64,
        cap_secs: i64,
        provider_id: String,
        model_name: String,
    },
}

impl SessionInterruptReason {
    /// Human-readable label used in tracing / log messages.  The
    /// `RuntimeCap` variant intentionally includes "not a task-quality
    /// strike" so downstream log searchers can filter on it.
    pub fn log_label(&self) -> &'static str {
        match self {
            Self::Wedged { .. } => "wedged session",
            Self::RuntimeCap { .. } => "glm-5.2 runtime-cap (not a task-quality strike)",
        }
    }
}

/// Combined classifier: decide whether a running session should be
/// interrupted, and if so, why.
///
/// This is the single call-site helper consumed by `session_active` and
/// `board_reconcile` so both paths use the same decision logic.  It runs
/// the wedged / stale liveness check first; if that returns `Live`, it
/// falls through to the runtime-cap check.  A `RuntimeCap` verdict takes
/// priority only when the session is not already wedged — if the session
/// is both wedged *and* over the runtime cap, `Wedged` wins because the
/// wedged condition is the more specific / urgent signal.
///
/// Returns `None` when the session is live under both classifiers (no
/// interruption needed).
#[allow(clippy::too_many_arguments)]
pub fn classify_session_interruption(
    agent_type: &str,
    model_id: &str,
    started_at: &str,
    last_message_at: Option<&str>,
    tokens_in: i64,
    tokens_out: i64,
    now: OffsetDateTime,
    liveness_config: &LivenessConfig,
    runtime_cap_config: &RuntimeCapConfig,
) -> Option<SessionInterruptReason> {
    // 1. Wedged / stale check (existing behavior).
    let progress = classify_session_progress(
        started_at,
        last_message_at,
        tokens_in,
        tokens_out,
        now,
        liveness_config,
    );
    if let ProgressVerdict::Wedged {
        idle_secs,
        zero_tokens,
    } = progress
    {
        return Some(SessionInterruptReason::Wedged {
            idle_secs,
            zero_tokens,
        });
    }

    // 2. Runtime-cap check (epic 5wxi — glm-5.2 worker cap).
    let cap = classify_runtime_cap(agent_type, model_id, started_at, now, runtime_cap_config);
    if let RuntimeCapVerdict::RuntimeCap {
        elapsed_secs,
        cap_secs,
        provider_id,
        model_name,
    } = cap
    {
        return Some(SessionInterruptReason::RuntimeCap {
            elapsed_secs,
            cap_secs,
            provider_id,
            model_name,
        });
    }

    None
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

    // ── Runtime-cap classifier tests (epic 5wxi, task vt00) ────────────
    //
    // The runtime cap is independent of the wedged-session thresholds
    // above. These tests pin the contract:
    //
    // - `zai-coding-plan/glm-5.2` and `zai/glm-5.2` worker sessions hit
    //   `RuntimeCap` at or beyond ~90 minutes (5,400 seconds).
    // - Below cap, wrong role, wrong model, and bad timestamps stay `Live`.
    // - The verdict is a distinct enum (`RuntimeCapVerdict`) so the
    //   follow-up wiring can branch on it without conflating it with the
    //   wedged `Wedged` verdict.

    /// Cap is exactly 90 minutes (5,400 s) and only targets the two
    /// `glm-5.2` provider ids surveyed in the repo.
    #[test]
    fn runtime_cap_default_config_targets_only_glm5_zai() {
        let cfg = RuntimeCapConfig::default_config();
        assert_eq!(cfg.worker_runtime_cap_secs, 5_400);
        assert_eq!(cfg.worker_agent_type, "worker");
        assert!(
            cfg.capped_models
                .iter()
                .any(|(p, m)| p == "zai-coding-plan" && m == "glm-5.2"),
            "default config must include zai-coding-plan/glm-5.2"
        );
        assert!(
            cfg.capped_models
                .iter()
                .any(|(p, m)| p == "zai" && m == "glm-5.2"),
            "default config must include zai/glm-5.2"
        );
        assert_eq!(cfg.capped_models.len(), 2);
    }

    /// `zai-coding-plan/glm-5.2` worker exactly at 90 minutes triggers the
    /// cap (boundary: `elapsed >= cap`).
    #[test]
    fn runtime_cap_at_threshold_triggers_for_zai_coding_plan_glm5() {
        let now = t("2026-07-06T13:20:00Z");
        // 5,400 seconds == 90 minutes.
        let started = "2026-07-06T11:50:00Z";
        let v = classify_runtime_cap(
            "worker",
            "zai-coding-plan/glm-5.2",
            started,
            now,
            &RuntimeCapConfig::default_config(),
        );
        assert_eq!(
            v,
            RuntimeCapVerdict::RuntimeCap {
                elapsed_secs: 5_400,
                cap_secs: 5_400,
                provider_id: "zai-coding-plan".to_string(),
                model_name: "glm-5.2".to_string(),
            }
        );
    }

    /// `zai/glm-5.2` worker well beyond 90 minutes also triggers.
    #[test]
    fn runtime_cap_well_over_threshold_triggers_for_zai_glm5() {
        let now = t("2026-07-06T13:20:00Z");
        // 6,300 seconds == 105 minutes.
        let started = "2026-07-06T11:35:00Z";
        let v = classify_runtime_cap(
            "worker",
            "zai/glm-5.2",
            started,
            now,
            &RuntimeCapConfig::default_config(),
        );
        assert_eq!(
            v,
            RuntimeCapVerdict::RuntimeCap {
                elapsed_secs: 6_300,
                cap_secs: 5_400,
                provider_id: "zai".to_string(),
                model_name: "glm-5.2".to_string(),
            }
        );
    }

    /// `zai-coding-plan/glm-5.2` worker just below 90 minutes is `Live`.
    #[test]
    fn runtime_cap_below_threshold_is_live() {
        let now = t("2026-07-06T13:20:00Z");
        // 5,399 seconds == 89m59s — under the cap.
        let started = "2026-07-06T11:50:01Z";
        let v = classify_runtime_cap(
            "worker",
            "zai-coding-plan/glm-5.2",
            started,
            now,
            &RuntimeCapConfig::default_config(),
        );
        assert_eq!(v, RuntimeCapVerdict::Live);
    }

    /// `xiaomi-token-plan-sgp/mimo-v2.5-pro` is exempt even though it has
    /// run far past the cap — only the configured `capped_models` count.
    #[test]
    fn runtime_cap_does_not_apply_to_other_models() {
        let now = t("2026-07-06T13:20:00Z");
        let started = "2026-07-05T13:20:00Z"; // 24h ago, would trip the cap
        let v = classify_runtime_cap(
            "worker",
            "xiaomi-token-plan-sgp/mimo-v2.5-pro",
            started,
            now,
            &RuntimeCapConfig::default_config(),
        );
        assert_eq!(v, RuntimeCapVerdict::Live);
    }

    /// `glm-5.2` (without `provider/` prefix) is exempt — the classifier
    /// must not match on a bare model name.
    #[test]
    fn runtime_cap_rejects_model_id_without_provider_prefix() {
        let now = t("2026-07-06T13:20:00Z");
        let started = "2026-07-06T11:50:00Z"; // exactly at cap
        let v = classify_runtime_cap(
            "worker",
            "glm-5.2",
            started,
            now,
            &RuntimeCapConfig::default_config(),
        );
        assert_eq!(v, RuntimeCapVerdict::Live);
    }

    /// Substring matches like `zai-coding-plan-extra/glm-5.2` or
    /// `zai/glm-5.2-extra` must not count — exact equality on both halves.
    #[test]
    fn runtime_cap_requires_exact_provider_and_model_equality() {
        let now = t("2026-07-06T13:20:00Z");
        let started = "2026-07-06T11:50:00Z"; // exactly at cap

        // Wrong provider prefix.
        let v = classify_runtime_cap(
            "worker",
            "zai-coding-plan-extra/glm-5.2",
            started,
            now,
            &RuntimeCapConfig::default_config(),
        );
        assert_eq!(v, RuntimeCapVerdict::Live);

        // Wrong model suffix.
        let v = classify_runtime_cap(
            "worker",
            "zai-coding-plan/glm-5.2-preview",
            started,
            now,
            &RuntimeCapConfig::default_config(),
        );
        assert_eq!(v, RuntimeCapVerdict::Live);
    }

    /// Reviewer sessions are exempt by design — even an hour-long
    /// `zai-coding-plan/glm-5.2` reviewer must not trigger the cap.
    #[test]
    fn runtime_cap_does_not_apply_to_reviewer_role() {
        let now = t("2026-07-06T13:20:00Z");
        let started = "2026-07-05T13:20:00Z"; // 24h ago
        let v = classify_runtime_cap(
            "reviewer",
            "zai-coding-plan/glm-5.2",
            started,
            now,
            &RuntimeCapConfig::default_config(),
        );
        assert_eq!(v, RuntimeCapVerdict::Live);
    }

    /// Planner sessions are exempt by design — same rationale as reviewer.
    #[test]
    fn runtime_cap_does_not_apply_to_planner_role() {
        let now = t("2026-07-06T13:20:00Z");
        let started = "2026-07-05T13:20:00Z"; // 24h ago
        let v = classify_runtime_cap(
            "planner",
            "zai/glm-5.2",
            started,
            now,
            &RuntimeCapConfig::default_config(),
        );
        assert_eq!(v, RuntimeCapVerdict::Live);
    }

    /// `chat` sessions are exempt by design — chat is not on the cap list
    /// and additionally is not a worker role.
    #[test]
    fn runtime_cap_does_not_apply_to_chat_role() {
        let now = t("2026-07-06T13:20:00Z");
        let started = "2026-07-05T13:20:00Z"; // 24h ago
        let v = classify_runtime_cap(
            "chat",
            "zai-coding-plan/glm-5.2",
            started,
            now,
            &RuntimeCapConfig::default_config(),
        );
        assert_eq!(v, RuntimeCapVerdict::Live);
    }

    /// Unparseable `started_at` fails open (same posture as
    /// `classify_session_progress`) — the classifier must not synthesize a
    /// cap verdict from a clock it cannot read.
    #[test]
    fn runtime_cap_unparseable_timestamp_is_live() {
        let now = t("2026-07-06T13:20:00Z");
        let v = classify_runtime_cap(
            "worker",
            "zai-coding-plan/glm-5.2",
            "garbage",
            now,
            &RuntimeCapConfig::default_config(),
        );
        assert_eq!(v, RuntimeCapVerdict::Live);
    }

    /// Space-separated (SQLite-style) timestamps also parse, so a
    /// `started_at` written that way still produces a `RuntimeCap`
    /// verdict when the elapsed wall-clock is past the cap.
    #[test]
    fn runtime_cap_accepts_space_separated_timestamp() {
        let now = t("2026-07-06T13:20:00Z");
        let started = "2026-07-06 11:50:00"; // exactly 5,400s ago
        let v = classify_runtime_cap(
            "worker",
            "zai-coding-plan/glm-5.2",
            started,
            now,
            &RuntimeCapConfig::default_config(),
        );
        assert_eq!(
            v,
            RuntimeCapVerdict::RuntimeCap {
                elapsed_secs: 5_400,
                cap_secs: 5_400,
                provider_id: "zai-coding-plan".to_string(),
                model_name: "glm-5.2".to_string(),
            }
        );
    }

    /// Negative elapsed (future `started_at`) is clamped to zero, so the
    /// classifier returns `Live` — same posture as `elapsed_secs_since`.
    #[test]
    fn runtime_cap_negative_elapsed_clamped_to_zero() {
        let now = t("2026-07-06T13:20:00Z");
        let started = "2026-07-06T13:25:00Z"; // future
        let v = classify_runtime_cap(
            "worker",
            "zai-coding-plan/glm-5.2",
            started,
            now,
            &RuntimeCapConfig::default_config(),
        );
        assert_eq!(v, RuntimeCapVerdict::Live);
    }

    /// Custom config (different cap, different model set) still drives the
    /// same verdict shape — verifies the classifier depends on the config,
    /// not on hard-coded globals.
    #[test]
    fn runtime_cap_uses_provided_config_not_hardcoded() {
        let now = t("2026-07-06T13:20:00Z");
        let started = "2026-07-06T13:00:00Z"; // 1,200s ago, >= 600s
        let cfg = RuntimeCapConfig {
            worker_runtime_cap_secs: 600,
            capped_models: vec![("acme".to_string(), "beta-1".to_string())],
            worker_agent_type: "worker".to_string(),
        };
        let v = classify_runtime_cap("worker", "acme/beta-1", started, now, &cfg);
        assert_eq!(
            v,
            RuntimeCapVerdict::RuntimeCap {
                elapsed_secs: 1_200,
                cap_secs: 600,
                provider_id: "acme".to_string(),
                model_name: "beta-1".to_string(),
            }
        );

        // `glm-5.2` is no longer in the config's capped_models, so even a
        // long-running zai-coding-plan session stays `Live`.
        let v = classify_runtime_cap(
            "worker",
            "zai-coding-plan/glm-5.2",
            "2026-07-05T13:20:00Z",
            now,
            &cfg,
        );
        assert_eq!(v, RuntimeCapVerdict::Live);
    }

    /// The runtime-cap verdict is a distinct enum from `ProgressVerdict`
    /// (which only carries `Live` or `Wedged { .. }`). This is the
    /// acceptance-criterion #3 invariant: follow-up wiring can branch on
    /// `RuntimeCap` without conflating it with wedged/stale failures.
    #[test]
    fn runtime_cap_verdict_is_distinct_type_from_progress_verdict() {
        // Compile-time guarantee: `RuntimeCapVerdict` and `ProgressVerdict`
        // are different enums; this test just pins the runtime tag so a
        // future refactor that collapses them is a deliberate decision.
        let cap: RuntimeCapVerdict = RuntimeCapVerdict::RuntimeCap {
            elapsed_secs: 5_500,
            cap_secs: 5_400,
            provider_id: "zai-coding-plan".to_string(),
            model_name: "glm-5.2".to_string(),
        };
        match cap {
            RuntimeCapVerdict::RuntimeCap { .. } => {}
            RuntimeCapVerdict::Live => panic!("expected RuntimeCap"),
        }
    }

    // ── Combined interruption classifier tests (wiring seam) ────────
    //
    // These tests pin the behaviour of `classify_session_interruption`,
    // which is the single decision helper consumed by `session_active`
    // and `board_reconcile`.  Each test exercises one wiring-seam
    // scenario:
    //
    // 1. Over-cap glm-5.2 worker → `RuntimeCap` reason
    // 2. Below-cap glm-5.2 worker → no interruption
    // 3. Non-worker role on glm-5.2 → no interruption
    // 4. Worker on non-glm-5.2 model → no interruption
    // 5. Wedged session (any model) → `Wedged` reason
    // 6. Wedged + over-cap → `Wedged` wins (more specific)

    /// Over-cap glm-5.2 worker session produces a `RuntimeCap` reason.
    /// A recent `last_message_at` keeps the session out of the wedged
    /// path so only the runtime-cap check fires.
    #[test]
    fn interruption_over_cap_glm5_worker() {
        let now = t("2026-07-06T13:20:00Z");
        let started = "2026-07-06T11:50:00Z"; // 5,400s == 90min
        let last_msg = "2026-07-06T13:19:00Z"; // 60s ago — not wedged
        let r = classify_session_interruption(
            "worker",
            "zai-coding-plan/glm-5.2",
            started,
            Some(last_msg),
            500,
            200,
            now,
            &LivenessConfig::OBSERVATION,
            &RuntimeCapConfig::default_config(),
        );
        match r.unwrap() {
            SessionInterruptReason::RuntimeCap {
                elapsed_secs,
                cap_secs,
                provider_id,
                model_name,
            } => {
                assert_eq!(elapsed_secs, 5_400);
                assert_eq!(cap_secs, 5_400);
                assert_eq!(provider_id, "zai-coding-plan");
                assert_eq!(model_name, "glm-5.2");
            }
            other => panic!("expected RuntimeCap, got {:?}", other),
        }
    }

    /// Below-cap glm-5.2 worker session produces no interruption.
    #[test]
    fn interruption_below_cap_glm5_worker_is_none() {
        let now = t("2026-07-06T13:20:00Z");
        let started = "2026-07-06T11:50:01Z"; // 5,399s — under cap
        let last_msg = "2026-07-06T13:19:00Z"; // 60s ago — not wedged
        let r = classify_session_interruption(
            "worker",
            "zai-coding-plan/glm-5.2",
            started,
            Some(last_msg),
            500,
            200,
            now,
            &LivenessConfig::OBSERVATION,
            &RuntimeCapConfig::default_config(),
        );
        assert!(r.is_none(), "below-cap worker should be None, got {:?}", r);
    }

    /// Reviewer role on glm-5.2 — even at 24h — produces no interruption.
    #[test]
    fn interruption_non_worker_role_on_glm5_is_none() {
        let now = t("2026-07-06T13:20:00Z");
        let started = "2026-07-05T13:20:00Z"; // 24h ago
        let last_msg = "2026-07-06T13:19:00Z"; // 60s ago — not wedged
        let r = classify_session_interruption(
            "reviewer",
            "zai-coding-plan/glm-5.2",
            started,
            Some(last_msg),
            500,
            200,
            now,
            &LivenessConfig::OBSERVATION,
            &RuntimeCapConfig::default_config(),
        );
        assert!(
            r.is_none(),
            "reviewer should not be interrupted, got {:?}",
            r
        );
    }

    /// Worker on a non-glm-5.2 model — even at 24h — produces no
    /// interruption from the runtime-cap path.
    #[test]
    fn interruption_worker_on_other_model_is_none() {
        let now = t("2026-07-06T13:20:00Z");
        let started = "2026-07-05T13:20:00Z"; // 24h ago
        let last_msg = "2026-07-06T13:19:00Z"; // 60s ago — not wedged
        let r = classify_session_interruption(
            "worker",
            "xiaomi-token-plan-sgp/mimo-v2.5-pro",
            started,
            Some(last_msg),
            500,
            200,
            now,
            &LivenessConfig::OBSERVATION,
            &RuntimeCapConfig::default_config(),
        );
        assert!(
            r.is_none(),
            "non-glm-5.2 worker should not be interrupted, got {:?}",
            r
        );
    }

    /// Wedged session (zero tokens, past threshold) produces `Wedged`
    /// regardless of runtime-cap status.
    #[test]
    fn interruption_wedged_session_produces_wedged_reason() {
        let now = t("2026-07-06T13:20:00Z");
        let started = "2026-07-06T13:17:00Z"; // 180s == zero-token threshold
        let r = classify_session_interruption(
            "worker",
            "xiaomi-token-plan-sgp/mimo-v2.5-pro",
            started,
            None,
            0,
            0,
            now,
            &LivenessConfig::OBSERVATION,
            &RuntimeCapConfig::default_config(),
        );
        match r.unwrap() {
            SessionInterruptReason::Wedged {
                idle_secs,
                zero_tokens,
            } => {
                assert_eq!(idle_secs, 180);
                assert!(zero_tokens);
            }
            other => panic!("expected Wedged, got {:?}", other),
        }
    }

    /// When a session is BOTH wedged and over the runtime cap, `Wedged`
    /// wins (it's the more specific / urgent signal).
    #[test]
    fn interruption_wedged_wins_over_runtime_cap() {
        let now = t("2026-07-06T13:20:00Z");
        // Zero tokens, 91 minutes — both wedged (zero-token threshold
        // = 180s) and over the runtime cap (5,400s).
        let started = "2026-07-06T11:49:00Z"; // 5,460s ago
        let r = classify_session_interruption(
            "worker",
            "zai-coding-plan/glm-5.2",
            started,
            None,
            0,
            0,
            now,
            &LivenessConfig::OBSERVATION,
            &RuntimeCapConfig::default_config(),
        );
        match r.unwrap() {
            SessionInterruptReason::Wedged { .. } => {} // expected
            other => panic!("expected Wedged to win over RuntimeCap, got {:?}", other),
        }
    }
}
