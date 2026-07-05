//! Board-health JSON sections that are large enough to warrant living outside
//! `queries.rs` (keeping that file under the repository size guard).
//!
//! These builders back `TaskRepository::board_health`. Each returns an additive
//! JSON object that is spliced into the top-level board-health report:
//!   * [`liveness_outcomes_section`]  — bounded recent liveness-classifier evidence.
//!   * [`protocol_violations_section`] — bounded recent protocol-violation evidence.
//!   * [`stranded_ready_section`]     — ready/dispatchable tasks that are not being
//!     picked up, with dispatch-gate evidence and exclusion of tasks whose
//!     non-dispatch is already explained by a visible gate (breaker cooldown,
//!     rate-limit backoff, manual pause, or revoked owner credentials).
//!
//! Every query here uses runtime `sqlx::query`/`query_scalar` (not the
//! compile-time macros) so the sections stay resilient to schema evolution and
//! add no entries to the offline query cache.

use std::collections::HashMap;

use sqlx::Row;

/// Bounded number of recent liveness-evidence rows surfaced in `liveness_outcomes`.
const LIVENESS_OUTCOMES_LIMIT: i64 = 25;
/// Bounded number of recent protocol-violation rows surfaced.
const PROTOCOL_VIOLATIONS_LIMIT: i64 = 15;
/// Base stranded-ready threshold: a ready task unclaimed for this many minutes
/// is `warning`; ≥2× is `error`; ≥6× is `critical`.
const STRANDED_THRESHOLD_MINUTES: i64 = 30;

/// Model-health statuses that indicate the chosen model is NOT dispatchable.
/// Any other status (including the unprobed default `unknown`) is treated as
/// ready so board_health does not raise false gate alarms.
const UNHEALTHY_MODEL_STATUSES: &[&str] = &[
    "unreachable",
    "error",
    "down",
    "offline",
    "disconnected",
    "unhealthy",
];

/// Compute the number of elapsed minutes between two ISO-8601 UTC timestamps
/// stored as `YYYY-MM-DDTHH:MM:SS.MSZ`. Returns `None` if either timestamp
/// cannot be parsed. This is intentionally minimal — the format is always
/// produced by Postgres `to_char(now(), ...)` and validated at insert time.
pub(super) fn elapsed_minutes_iso(iso_start: &str, iso_end: &str) -> Option<i64> {
    fn parse_iso_minutes(s: &str) -> Option<i64> {
        // Expected: "2025-01-15T10:30:00.000Z"
        if s.len() < 20 || !s.is_ascii() {
            return None;
        }
        let bytes = s.as_bytes();
        if bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
            return None;
        }
        let year: i64 = s[0..4].parse().ok()?;
        let month: u32 = s[5..7].parse().ok()?;
        let day: u32 = s[8..10].parse().ok()?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        // Find the seconds end: the format is HH:MM:SS.MSZ so we look for '.'
        // (fractional seconds start) or 'Z' (no fractional part fallback).
        let time_part = &s[11..];
        let sec_end = time_part
            .find('.')
            .or_else(|| time_part.find('Z'))
            .unwrap_or(time_part.len());
        // There must be at least HH:MM:SS (8 chars)
        if sec_end < 8 {
            return None;
        }
        let seconds_str = &time_part[0..sec_end];
        // Parse HH:MM:SS
        let total_seconds: i64 =
            seconds_str
                .split(':')
                .enumerate()
                .try_fold(0i64, |acc, (i, part)| {
                    let v: i64 = part.parse().ok()?;
                    Some(acc + v * [3600, 60, 1][i])
                })?;
        // Total days since year 0 (proleptic Gregorian, good enough for elapsed
        // computation — we only care about the *difference* between two nearby
        // timestamps, so leap-year drift is negligible).
        let is_leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let month_days: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
        let mut doy = month_days[(month - 1) as usize] + (day as i64 - 1);
        if month > 2 && is_leap {
            doy += 1;
        }
        let total_days = year * 365 + year / 4 - year / 100 + year / 400 + doy;
        Some(total_days * 24 * 60 + total_seconds / 60)
    }
    let start = parse_iso_minutes(iso_start)?;
    let end = parse_iso_minutes(iso_end)?;
    Some(end - start)
}

/// Return the current UTC time formatted as ISO-8601 with milliseconds from the
/// database clock, matching the Postgres `to_char(now() ...)` format used
/// throughout the codebase. Used for string-comparison thresholds.
pub(super) async fn db_utc_now(pool: &sqlx::PgPool) -> String {
    sqlx::query_scalar(
        r#"SELECT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"#,
    )
    .fetch_one(pool)
    .await
    .unwrap_or_else(|_| "9999-12-31T23:59:59.999Z".to_string())
}

/// Derive the role the coordinator would evaluate for a task given only its
/// persisted `status` and `issue_type`. Mirrors `dispatched_role_for_task`,
/// which needs a full `Task`; board_health only has row columns.
fn dispatched_role_for_status_type(status: &str, issue_type: &str) -> &'static str {
    match status {
        "needs_task_review" | "in_task_review" => "reviewer",
        "needs_lead_intervention" | "in_lead_intervention" => "lead",
        _ => match issue_type {
            "planning" | "decomposition" => "planner",
            "spike" | "review" => "architect",
            _ => "worker",
        },
    }
}

/// Bounded recent liveness-classifier evidence.
pub(super) async fn liveness_outcomes_section(pool: &sqlx::PgPool) -> serde_json::Value {
    let rows = sqlx::query(
        r#"SELECT le.verdict, le.outcome_kind, le.outcome_reason,
                  le.created_at, le.task_id, le.session_id
           FROM liveness_evidence le
           ORDER BY le.created_at DESC
           LIMIT $1"#,
    )
    .bind(LIVENESS_OUTCOMES_LIMIT)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let total = rows.len();
    let by_verdict = {
        let mut counts = serde_json::Map::new();
        for row in &rows {
            let v: String = row.get("verdict");
            let entry = counts.entry(v).or_insert(serde_json::json!(0));
            *entry = serde_json::json!(entry.as_i64().unwrap_or(0) + 1);
        }
        serde_json::Value::Object(counts)
    };
    let recent: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "verdict":        row.get::<String, _>("verdict"),
                "outcome_kind":   row.get::<Option<String>, _>("outcome_kind"),
                "outcome_reason": row.get::<Option<String>, _>("outcome_reason"),
                "created_at":     row.get::<String, _>("created_at"),
                "task_id":        row.get::<Option<String>, _>("task_id"),
                "session_id":     row.get::<String, _>("session_id"),
            })
        })
        .collect();

    serde_json::json!({
        "total":      total,
        "by_verdict": by_verdict,
        "recent":     recent,
    })
}

/// Bounded recent protocol-violation evidence.
pub(super) async fn protocol_violations_section(pool: &sqlx::PgPool) -> serde_json::Value {
    let rows = sqlx::query(
        r#"SELECT le.verdict, le.outcome_kind, le.outcome_reason,
                  le.created_at, le.task_id, le.session_id,
                  t.short_id AS task_short_id,
                  t.title    AS task_title,
                  t.status   AS task_status
           FROM liveness_evidence le
           LEFT JOIN tasks t ON t.id = le.task_id
           WHERE le.verdict = 'protocol_violation'
              OR le.outcome_kind = 'protocol_violation'
           ORDER BY le.created_at DESC
           LIMIT $1"#,
    )
    .bind(PROTOCOL_VIOLATIONS_LIMIT)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let total = rows.len();
    let recent: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "verdict":        row.get::<String, _>("verdict"),
                "outcome_kind":   row.get::<Option<String>, _>("outcome_kind"),
                "outcome_reason": row.get::<Option<String>, _>("outcome_reason"),
                "created_at":     row.get::<String, _>("created_at"),
                "task_id":        row.get::<Option<String>, _>("task_id"),
                "session_id":     row.get::<String, _>("session_id"),
                "task_short_id":  row.get::<Option<String>, _>("task_short_id"),
                "task_title":     row.get::<Option<String>, _>("task_title"),
                "task_status":    row.get::<Option<String>, _>("task_status"),
            })
        })
        .collect();

    serde_json::json!({
        "total":  total,
        "recent": recent,
    })
}

/// Ready/dispatchable tasks with no active running session whose unclaimed
/// duration exceeds the stranded threshold, each annotated with dispatch-gate
/// evidence.
///
/// Tasks whose non-dispatch is already explained by a visible gate are
/// **excluded** so the findings surface genuine dispatch starvation rather than
/// intentionally-held work:
///   * breaker-open (a future `cooldown_until`),
///   * rate-limit backoff (`failure_streak >= 3`),
///   * manual pause (a `paused` session or a project/user `dispatch_pauses` row),
///   * owner-credential-blocked (the creator has credentials but they are all
///     revoked and no org-shared fallback credential is available).
///
/// Surviving findings carry a `dispatch_gate` object documenting the evaluated
/// role, its toolset, the model requirement, image readiness, each gate flag,
/// credential availability, a final `gate_verdict`, and machine-readable
/// `reasons` (e.g. `no_eligible_model`, `image_not_ready`).
pub(super) async fn stranded_ready_section(pool: &sqlx::PgPool) -> serde_json::Value {
    let warning_threshold = STRANDED_THRESHOLD_MINUTES;
    let error_threshold = STRANDED_THRESHOLD_MINUTES * 2;
    let critical_threshold = STRANDED_THRESHOLD_MINUTES * 6;

    // Prefetch the model-health rollup keyed by full `provider/model` id so a
    // task's inflight model can be classified without an N+1 join.
    let model_health: HashMap<String, String> =
        sqlx::query(r#"SELECT provider || '/' || model AS model_id, status FROM model_health"#)
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| {
                let id: Option<String> = row.try_get("model_id").ok().flatten();
                let status: String = row.try_get("status").ok()?;
                id.map(|id| (id, status))
            })
            .collect();

    let now_iso = db_utc_now(pool).await;

    let stranded_sql = r#"SELECT t.id, t.short_id, t.title, t.status, t.updated_at, t.owner,
                  t.epic_id, t.issue_type, t.project_id, t.created_by_user_id,
                  e.short_id AS epic_short_id,
                  ds.last_dispatched_role,
                  to_char(ds.cooldown_until AT TIME ZONE 'utc',
                          'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS cooldown_until,
                  COALESCE(ds.failure_streak, 0)::BIGINT AS failure_streak,
                  ds.inflight_model_id,
                  (SELECT al.created_at
                   FROM activity_log al
                   WHERE al.task_id = t.id
                     AND al.event_type = 'status_changed'
                     AND al.payload->>'to_status' = 'open'
                   ORDER BY al.created_at DESC
                   LIMIT 1) AS open_transition_at,
                  (SELECT s.ended_at
                   FROM sessions s
                   WHERE s.task_id = t.id AND s.ended_at IS NOT NULL
                   ORDER BY s.ended_at DESC
                   LIMIT 1) AS session_release_at,
                  EXISTS (SELECT 1 FROM sessions s
                          WHERE s.task_id = t.id AND s.status = 'paused') AS has_paused_session,
                  EXISTS (SELECT 1 FROM dispatch_pauses dp
                          WHERE ((dp.scope = 'project' AND dp.target_id = t.project_id)
                              OR (dp.scope = 'user'    AND dp.target_id = t.created_by_user_id))
                            AND (dp.expires_at IS NULL
                                 OR dp.expires_at > to_char(now() AT TIME ZONE 'utc',
                                                            'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
                         ) AS dispatch_paused,
                  EXISTS (SELECT 1 FROM credentials c
                          WHERE c.revoked_at IS NULL
                            AND (c.owner_user_id = t.created_by_user_id
                                 OR c.owner_user_id IS NULL)
                         ) AS has_active_credential,
                  EXISTS (SELECT 1 FROM credentials c
                          WHERE c.owner_user_id = t.created_by_user_id
                         ) AS has_owner_credential
           FROM tasks t
           LEFT JOIN epics e ON e.id = t.epic_id
           LEFT JOIN dispatch_state ds ON ds.task_id = t.id
           WHERE t.status IN ('open', 'in_progress')
             -- Redundant with the IN filter above, but defensive against
             -- terminal/review statuses.
             AND t.status NOT IN ('closed', 'needs_task_review',
                                  'in_task_review', 'approved',
                                  'pr_draft', 'pr_review')
             -- No active running session for this task.
             AND NOT EXISTS (
                 SELECT 1 FROM sessions s
                 WHERE s.task_id = t.id AND s.status = 'running'
             )
             -- Exclude blocked tasks (unresolved blockers).
             AND NOT EXISTS (
                 SELECT 1 FROM blockers b
                 JOIN tasks bt ON b.blocking_task_id = bt.id
                 WHERE b.task_id = t.id AND bt.status != 'closed'
             )
           ORDER BY t.updated_at ASC"#;

    let rows = sqlx::query(stranded_sql)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

    let findings: Vec<serde_json::Value> = rows
        .into_iter()
        .filter_map(|row| {
            let task_status: String = row.get("status");
            let issue_type: String = row.get("issue_type");

            // ── Exclusion gates ────────────────────────────────────────────
            // Each of these explains, from visible DB state, why the task is
            // (correctly) not being dispatched, so it is not "stranded".

            // Breaker-open: a future cooldown deadline.
            let cooldown_until: Option<String> = row.try_get("cooldown_until").ok().flatten();
            let breaker_open = cooldown_until
                .as_deref()
                .is_some_and(|cd| cd > now_iso.as_str());
            if breaker_open {
                return None;
            }

            // Rate-limit backoff: the dispatch backoff ladder has tripped.
            let failure_streak: i64 = row.try_get("failure_streak").unwrap_or(0);
            let rate_limited = failure_streak >= 3;
            if rate_limited {
                return None;
            }

            // Manual pause: a paused session or a project/user dispatch pause.
            let has_paused_session: bool = row.try_get("has_paused_session").unwrap_or(false);
            let dispatch_paused: bool = row.try_get("dispatch_paused").unwrap_or(false);
            let manually_paused = has_paused_session || dispatch_paused;
            if manually_paused {
                return None;
            }

            // Owner-credential-blocked: the creator's credentials are all
            // revoked and no org-shared fallback is available. Tasks with no
            // creator or no credential rows are NOT treated as blocked.
            let created_by: Option<String> = row.try_get("created_by_user_id").ok().flatten();
            let has_active_credential: bool = row.try_get("has_active_credential").unwrap_or(false);
            let has_owner_credential: bool = row.try_get("has_owner_credential").unwrap_or(false);
            let credential_available = created_by.is_none() || has_active_credential;
            let credential_blocked =
                created_by.is_some() && !has_active_credential && has_owner_credential;
            if credential_blocked {
                return None;
            }

            // ── Unclaimed-since / severity ─────────────────────────────────
            // Prefer the most recent high-confidence release signal (ready/open
            // transition or session release); fall back to updated_at.
            let open_transition_at: Option<String> =
                row.try_get("open_transition_at").ok().flatten();
            let session_release_at: Option<String> =
                row.try_get("session_release_at").ok().flatten();
            let updated_at: String = row.get("updated_at");

            let high_signal = match (open_transition_at, session_release_at) {
                (Some(a), Some(b)) => Some(if a >= b { a } else { b }),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            let (unclaimed_since, confidence) = match high_signal {
                Some(ts) => (ts, "high"),
                None => (updated_at.clone(), "low"),
            };

            let elapsed = elapsed_minutes_iso(&unclaimed_since, &now_iso).unwrap_or(0);
            // Not yet stranded (below 1× threshold).
            if elapsed < warning_threshold {
                return None;
            }
            let severity = if elapsed >= critical_threshold {
                "critical"
            } else if elapsed >= error_threshold {
                "error"
            } else {
                "warning"
            };

            // ── Model / image readiness evidence ───────────────────────────
            let inflight_model_id: Option<String> = row.try_get("inflight_model_id").ok().flatten();
            let mut reasons: Vec<&str> = Vec::new();
            let image_ready = match inflight_model_id.as_deref() {
                // No model chosen yet — nothing model-specific to block on.
                None => true,
                Some(mid) => match model_health.get(mid) {
                    None => {
                        reasons.push("no_eligible_model");
                        false
                    }
                    Some(status) if UNHEALTHY_MODEL_STATUSES.contains(&status.as_str()) => {
                        reasons.push("image_not_ready");
                        false
                    }
                    Some(_) => true,
                },
            };

            let evaluated_role = dispatched_role_for_status_type(&task_status, &issue_type);
            let toolset = super::queries::toolset_for_role(evaluated_role);
            let gate_verdict = if reasons.is_empty() {
                "stranded"
            } else {
                "blocked"
            };

            Some(serde_json::json!({
                "id":            row.get::<String, _>("id"),
                "short_id":      row.get::<String, _>("short_id"),
                "title":         row.get::<String, _>("title"),
                "status":        task_status,
                "owner":         row.get::<String, _>("owner"),
                "updated_at":    updated_at,
                "epic_short_id": row.get::<Option<String>, _>("epic_short_id"),
                "unclaimed_since": unclaimed_since,
                "unclaimed_since_confidence": confidence,
                "elapsed_minutes": elapsed,
                "severity":      severity,
                "threshold":     serde_json::json!({
                    "warning_minutes":  warning_threshold,
                    "error_minutes":    error_threshold,
                    "critical_minutes": critical_threshold,
                }),
                "dispatch_gate": serde_json::json!({
                    "evaluated_role":       evaluated_role,
                    "toolset":              toolset,
                    "model_requirement":    inflight_model_id,
                    "image_ready":          image_ready,
                    "breaker_open":         breaker_open,
                    "manually_paused":      manually_paused,
                    "rate_limited":         rate_limited,
                    "credential_available": credential_available,
                    "gate_verdict":         gate_verdict,
                    "reasons":              reasons,
                    // Retained for backward compatibility with the initial
                    // board_health contract.
                    "last_dispatched_role": row.get::<Option<String>, _>("last_dispatched_role"),
                    "cooldown_until":       cooldown_until,
                }),
            }))
        })
        .collect();

    serde_json::json!({
        "total":             findings.len(),
        "threshold_minutes": STRANDED_THRESHOLD_MINUTES,
        "findings":          findings,
    })
}
